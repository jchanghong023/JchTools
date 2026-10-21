use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::Duration;

use super::formats::{supported_formats, FormatGroup};
use super::media::convert_media;
use super::output::write_new_markdown_cancelable;
use super::scan::scan_cancelable;
use crate::{markdown_assets, markdown_document};

#[derive(Clone, Debug)]
pub struct Options {
    pub input_dir: PathBuf,
    pub output_dir: PathBuf,
    pub flat: bool,
    pub groups: Vec<FormatGroup>,
    pub timeout_secs: u64,
}

#[derive(Clone, Debug)]
pub enum Event {
    Started {
        total: usize,
    },
    FileStarted {
        relative: PathBuf,
        /// 全批次位置（从 1 起）：包括扫描阶段已跳过或失败的文件及当前文件。
        index: usize,
        total: usize,
    },
    FileFinished {
        relative: PathBuf,
        success: bool,
        partial: bool,
        message: String,
    },
    FileCancelled {
        relative: PathBuf,
    },
    Log(String),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Summary {
    pub success: usize,
    pub partial: usize,
    pub failed: usize,
    pub skipped_existing: usize,
    pub skipped_duplicate: usize,
    pub stopped: bool,
}

pub fn readiness() -> Result<(), String> {
    platform_preflight()?;
    markdown_assets::readiness()
}

/// 默认格式筛选包含 Media；缺少可选音频不能阻断文档/图片批次。
/// 媒体资产在逐文件转录边界检查并明确报错，不触发联网。
pub fn readiness_for_groups(groups: &[FormatGroup]) -> Result<(), String> {
    platform_preflight()?;
    let needs_document = groups
        .iter()
        .any(|group| !matches!(group, FormatGroup::Media));
    let media_only = !needs_document && groups.contains(&FormatGroup::Media);
    let mut problems = Vec::new();
    if needs_document {
        if let Err(error) = markdown_assets::readiness() {
            problems.push(format!("文档转换组件未就绪：{error}"));
        }
    }
    if media_only {
        if let Err(error) = markdown_assets::runtime_dir()
            .and_then(|root| jchtools_xberg::xberg_runtime::validate_assets(&root, "engine"))
        {
            problems.push(format!("共享 Xberg 引擎未就绪：{error}"));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("；"))
    }
}

/// 转换页的就绪门槛（XB-19 场景独立）：文档或媒体任一场景就绪即允许开始任务，
/// 让纯媒体环境（缺文档模型）与纯文档环境（缺媒体模型）都能进入转换页；
/// 本次任务所选分组的精确检查在 [`run`] 启动前执行。
pub fn page_readiness() -> Result<(), String> {
    let document = readiness();
    let media = markdown_assets::validate_media();
    match (&document, &media) {
        (Ok(()), _) | (_, Ok(())) => Ok(()),
        (Err(document_error), Err(media_error)) => Err(format!(
            "文档与媒体组件均未就绪：{document_error}；{media_error}"
        )),
    }
}

pub fn run(
    options: &Options,
    cancel: &AtomicBool,
    mut events: impl FnMut(Event),
) -> Result<Summary, String> {
    let span = jchtools_foundation::logging::operation_span("markdown", "convert_batch");
    let _entered = span.enter();
    let started = std::time::Instant::now();
    let mut stage = "options";
    tracing::info!(
        event = "markdown_batch_started",
        group_count = options.groups.len(),
        timeout_secs = options.timeout_secs,
        "转Markdown任务开始"
    );
    let result = (|| {
        if options.timeout_secs == 0 {
            return Err("单文件超时必须为正整秒".to_string());
        }
        if options.groups.is_empty() {
            return Err("至少选择一组文件类型".to_string());
        }
        stage = "runtime_assets";
        readiness_for_groups(&options.groups)?;
        let runtime_dir = markdown_assets::runtime_dir()?;
        if cancel.load(AtomicOrdering::Acquire) {
            events(Event::Started { total: 0 });
            return Ok(Summary {
                stopped: true,
                ..Summary::default()
            });
        }
        stage = "formats_request_boundary";
        let supported = match supported_formats(&runtime_dir, &options.groups, cancel) {
            Ok(supported) => supported,
            Err(message)
                if cancel.load(AtomicOrdering::Acquire) && confirmed_cancellation(&message) =>
            {
                events(Event::Started { total: 0 });
                events(Event::Log("已停止：格式查询期间未开始转换".to_string()));
                return Ok(Summary {
                    stopped: true,
                    ..Summary::default()
                });
            }
            Err(error) => return Err(error),
        };
        stage = "input_scan";
        let plan = match scan_cancelable(options, &supported, cancel) {
            Ok(plan) => plan,
            Err(_) if cancel.load(AtomicOrdering::Acquire) => {
                events(Event::Started { total: 0 });
                events(Event::Log("已停止：扫描期间未开始转换".to_string()));
                return Ok(Summary {
                    stopped: true,
                    ..Summary::default()
                });
            }
            Err(error) => return Err(error),
        };
        let total = plan.items.len()
            + plan.summary.skipped_existing
            + plan.summary.skipped_duplicate
            + plan.scan_failures.len();
        tracing::info!(
            event = "markdown_batch_planned",
            component = "markdown",
            planned_files = plan.items.len(),
            scan_failure_count = plan.scan_failures.len(),
            total,
            skipped_existing = plan.summary.skipped_existing,
            skipped_duplicate = plan.summary.skipped_duplicate,
            "转 Markdown 批次开始"
        );
        // T-22/U-03：已有结果、重名跳过及扫描失败均已终态，后续文件位置必须
        // 与包含这些项的 total 同口径，不能重新从待转换列表的第 1 项计数。
        let initially_finished = total - plan.items.len();
        events(Event::Started { total });
        // S4-03/T-24：扫描期已定的单文件失败先逐条呈现（文件 + 阶段 + 原因，
        // 与转换失败同一事件格式），统计随 plan.summary 带入；不中止批次。
        for (relative, message) in &plan.scan_failures {
            tracing::warn!(
                event = "markdown_file_failed", file = %jchtools_foundation::logging::safe_error(&relative.to_string_lossy()),
                stage = "scan_output_plan", error_type = "output_plan", elapsed_ms = 0u64,
                "转换单文件失败"
            );
            events(Event::FileFinished {
                relative: relative.clone(),
                success: false,
                partial: false,
                message: message.clone(),
            });
        }
        let mut summary = plan.summary;
        let output_root = plan.output_root;
        // 文档和媒体逐文件提交到会话共享引擎；批次不拥有引擎生命周期。
        stage = "convert_files";
        for (index, item) in plan.items.iter().enumerate() {
            if cancel.load(AtomicOrdering::Relaxed) {
                events(Event::Log("已停止：当前文件之外不再开始转换".to_string()));
                break;
            }
            let file_span = tracing::info_span!("markdown_file", file_index = index + 1,
            file = %jchtools_foundation::logging::safe_error(&item.relative.to_string_lossy()));
            let _file_entered = file_span.enter();
            let file_started = std::time::Instant::now();
            let mut file_stage = if item.is_media {
                "transcribe_boundary"
            } else {
                "extract_boundary"
            };
            tracing::debug!(
                event = "markdown_file_started",
                media = item.is_media,
                timeout_secs = options.timeout_secs,
                "转换单文件开始"
            );
            events(Event::FileStarted {
                relative: item.relative.clone(),
                index: initially_finished + index + 1,
                total,
            });
            // F21/T-29：单文件预算从进入该文件起算。
            let deadline =
                markdown_document::Deadline::new(Duration::from_secs(options.timeout_secs));
            let media_dir = &item.media_dir;
            let outcome = if item.is_media {
                convert_media(&item.source, &deadline, cancel).map(|markdown| {
                    markdown_document::DocumentOutput {
                        markdown,
                        warnings: Vec::new(),
                        media: Vec::new(),
                    }
                })
            } else {
                // 零配置（2026-10-04 跨仓接口改造）：不探测页数、不传 mode；页数
                // 自动分流内化引擎，auto_mode 降级等引擎警告经 warnings 转达，
                // 由下方 partial 语义如实呈现（T-18 界面披露义务随之满足）。
                markdown_document::convert_cancelable(
                    &item.source,
                    &runtime_dir,
                    media_dir,
                    &deadline,
                    cancel,
                )
            };
            let outcome = outcome.and_then(|document| {
                file_stage = "output_write";
                write_new_markdown_cancelable(
                    &output_root,
                    &item.target,
                    &document.markdown,
                    &document.media,
                    &deadline,
                    cancel,
                )?;
                Ok(document.warnings)
            });
            match outcome {
                Ok(warnings) => {
                    let partial = !warnings.is_empty();
                    if partial {
                        tracing::warn!(
                            event = "markdown_file_partial",
                            file = %jchtools_foundation::logging::safe_error(&item.relative.to_string_lossy()),
                            warning_count = warnings.len(),
                            "转换部分内容未提取"
                        );
                        summary.partial += 1;
                    } else {
                        summary.success += 1;
                    }
                    tracing::debug!(
                        event = "markdown_file_completed",
                        partial,
                        elapsed_ms = jchtools_foundation::logging::elapsed_ms(file_started),
                        warning_count = warnings.len(),
                        "转换单文件完成"
                    );
                    events(Event::FileFinished {
                        relative: item.relative.clone(),
                        success: !partial,
                        partial,
                        message: if partial {
                            format!("部分内容未提取：{}", warnings.join("；"))
                        } else {
                            "转换成功".to_string()
                        },
                    });
                }
                Err(ref message)
                    if cancel.load(AtomicOrdering::Acquire) && confirmed_cancellation(message) =>
                {
                    tracing::info!(
                        event = "markdown_file_cancelled",
                        stage = file_stage,
                        elapsed_ms = jchtools_foundation::logging::elapsed_ms(file_started),
                        "当前转换文件已取消"
                    );
                    events(Event::FileCancelled {
                        relative: item.relative.clone(),
                    });
                    break;
                }
                Err(message) => {
                    summary.failed += 1;
                    tracing::error!(
                        event = "markdown_file_failed",
                        stage = file_stage,
                        error_type = "conversion_boundary",
                        elapsed_ms = jchtools_foundation::logging::elapsed_ms(file_started),
                        "转换单文件失败"
                    );
                    events(Event::FileFinished {
                        relative: item.relative.clone(),
                        success: false,
                        partial: false,
                        message,
                    });
                }
            }
        }
        // T-22/T-23：批结束或用户停止后不再发送请求，保留共享引擎及已加载模型。
        summary.stopped = cancel.load(AtomicOrdering::Relaxed);
        tracing::info!(
            event = "markdown_batch_summary",
            success = summary.success,
            partial = summary.partial,
            failed = summary.failed,
            skipped_existing = summary.skipped_existing,
            skipped_duplicate = summary.skipped_duplicate,
            stopped = summary.stopped,
            "转 Markdown 批次结束"
        );
        Ok(summary)
    })();
    if let Ok(summary) = &result {
        tracing::info!(
            event = "markdown_batch_completed",
            outcome = if summary.stopped {
                "cancelled"
            } else if summary.failed > 0 {
                "with_failures"
            } else {
                "success"
            },
            component = "markdown",
            stage,
            skipped_existing = summary.skipped_existing,
            skipped_duplicate = summary.skipped_duplicate,
            stopped = summary.stopped,
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            success = summary.success,
            partial = summary.partial,
            failed = summary.failed,
            "转Markdown任务结束"
        );
    } else {
        tracing::error!(
            event = "markdown_batch_failed",
            stage,
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            error_type = "conversion_batch_boundary",
            cancel_requested = cancel.load(AtomicOrdering::Acquire),
            "转Markdown任务失败"
        );
    }
    result
}

/// T-23/T-24：停止原子量不证明引擎已结束；只把明确确认的取消作为普通取消，
/// 取消接口失败、原请求未结束和其他实际错误仍需保留原诊断。
pub(super) fn confirmed_cancellation(message: &str) -> bool {
    let message = message
        .strip_prefix("Xberg 共享接口不可用：能力握手失败（不会启动备用引擎）：")
        .unwrap_or(message);
    matches!(message, "请求已取消" | "用户已取消初始化") || message.starts_with("Xberg cancelled：")
}

#[cfg(windows)]
pub(crate) fn platform_preflight() -> Result<(), String> {
    use windows_sys::Win32::System::SystemInformation::{OSVERSIONINFOEXW, OSVERSIONINFOW};

    #[link(name = "ntdll")]
    extern "system" {
        #[link_name = "RtlGetVersion"]
        fn rtl_get_version(info: *mut OSVERSIONINFOW) -> i32;
    }

    // SAFETY: OSVERSIONINFOEXW 是纯 C 结构；调用时指针有效，API 按声明的大小写入。
    let mut version: OSVERSIONINFOEXW = unsafe { std::mem::zeroed() };
    version.dwOSVersionInfoSize = u32::try_from(std::mem::size_of::<OSVERSIONINFOEXW>())
        .map_err(|_| "Windows 版本结构大小超出范围".to_string())?;
    // SAFETY: OSVERSIONINFOEXW 的起始布局与 API 所需的 OSVERSIONINFOW 相同。
    let status = unsafe { rtl_get_version((&raw mut version).cast::<OSVERSIONINFOW>()) };
    if status < 0 {
        return Err(format!("无法确认 Windows 版本（NTSTATUS {status:#x}）"));
    }
    if version.dwMajorVersion != 10 || version.dwBuildNumber < 22_000 || version.wProductType != 1 {
        return Err("转 Markdown 仅支持 Windows 11 x64".to_string());
    }
    if !std::is_x86_feature_detected!("avx2") {
        return Err("转 Markdown 需要支持 AVX2 的处理器".to_string());
    }
    Ok(())
}

#[cfg(not(windows))]
pub(crate) fn platform_preflight() -> Result<(), String> {
    Err("转 Markdown 仅支持 Windows 11 x64".to_string())
}
