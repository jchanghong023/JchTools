//! 转 Markdown 的只读扫描、任务编排与结果落盘（T-07～T-12、T-22～T-25）。

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering as AtomicOrdering},
    time::Duration,
};

#[cfg(test)]
use std::process::Command;

use crate::{fsutil, markdown_assets, markdown_document};

const PDF: &[&str] = &["pdf"];
const OFFICE: &[&str] = &[
    "doc", "docx", "docm", "dot", "dotx", "dotm", "ppt", "pptx", "pptm", "pps", "ppsx", "pot",
    "potx", "potm", "xls", "xlsx", "xlsm", "xlsb", "xlt", "xltx", "xltm", "xla", "xlam", "odt",
    "ods", "odp",
];
const IMAGES: &[&str] = &[
    "png", "jpg", "jpeg", "webp", "bmp", "gif", "tif", "tiff", "jp2", "j2k", "j2c", "jpx", "jpm",
    "mj2", "jbig2", "jb2", "pnm", "pbm", "pgm", "ppm",
];
const MEDIA: &[&str] = &["mp4", "m4a"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatGroup {
    Pdf,
    Office,
    Images,
    Media,
    Other,
}

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
        index: usize,
        total: usize,
    },
    FileFinished {
        relative: PathBuf,
        success: bool,
        partial: bool,
        message: String,
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

#[derive(Clone, Debug)]
struct Item {
    source: PathBuf,
    relative: PathBuf,
    target: PathBuf,
    is_media: bool,
    media_dir: String,
}

#[derive(Clone, Debug, Default)]
struct Plan {
    output_root: PathBuf,
    items: Vec<Item>,
    summary: Summary,
}

pub fn readiness() -> Result<(), String> {
    platform_preflight()?;
    markdown_assets::readiness()
}

/// 任务启动前的场景化预检（XB-19：单独使用一种功能不强制初始化另一种）。
/// 所选分组决定需要检查的场景：Media 组走媒体链路（SenseVoice/VAD/sherpa/FFmpeg
/// 成员存在性），其余分组（PDF/Office/图片/其他）走文档场景；混选时两个场景
/// 都必须就绪，缺失原因汇总指认。修复前 run() 无条件用文档条件预检——纯媒体
/// 目录缺文档模型会被提前拒绝，缺媒体资产则要等任务开始后才逐文件报错。
pub fn readiness_for_groups(groups: &[FormatGroup]) -> Result<(), String> {
    platform_preflight()?;
    let needs_document = groups
        .iter()
        .any(|group| !matches!(group, FormatGroup::Media));
    let needs_media = groups.contains(&FormatGroup::Media);
    let mut problems = Vec::new();
    if needs_document {
        if let Err(error) = markdown_assets::readiness() {
            problems.push(format!("文档转换组件未就绪：{error}"));
        }
    }
    if needs_media {
        if let Err(error) = markdown_assets::validate_media() {
            problems.push(format!("媒体转录组件未就绪：{error}"));
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

pub fn initialize(cancel: &AtomicBool, progress: impl FnMut(String)) -> Result<(), String> {
    platform_preflight()?;
    markdown_assets::initialize(cancel, progress)
}

pub fn run(
    options: &Options,
    cancel: &AtomicBool,
    mut events: impl FnMut(Event),
) -> Result<Summary, String> {
    if options.timeout_secs == 0 {
        return Err("单文件超时必须为正整秒".to_string());
    }
    if options.groups.is_empty() {
        return Err("至少选择一组文件类型".to_string());
    }
    readiness_for_groups(&options.groups)?;
    let runtime_dir = markdown_assets::runtime_dir()?;
    let supported = supported_formats(&options.groups)?;
    if cancel.load(AtomicOrdering::Acquire) {
        events(Event::Started { total: 0 });
        return Ok(Summary {
            stopped: true,
            ..Summary::default()
        });
    }
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
    let total = plan.items.len() + plan.summary.skipped_existing + plan.summary.skipped_duplicate;
    tracing::info!(
        input = %options.input_dir.display(),
        output = %options.output_dir.display(),
        files = plan.items.len(),
        skipped_existing = plan.summary.skipped_existing,
        skipped_duplicate = plan.summary.skipped_duplicate,
        "转 Markdown 批次开始"
    );
    events(Event::Started { total });
    let mut summary = plan.summary;
    let output_root = plan.output_root;
    // 文档和媒体逐文件提交到会话共享引擎；批次不拥有引擎生命周期。
    for (index, item) in plan.items.iter().enumerate() {
        if cancel.load(AtomicOrdering::Relaxed) {
            events(Event::Log("已停止：当前文件之外不再开始转换".to_string()));
            break;
        }
        events(Event::FileStarted {
            relative: item.relative.clone(),
            index: index + 1,
            total,
        });
        // F21/T-29：单文件预算从进入该文件起算。
        let deadline = markdown_document::Deadline::new(Duration::from_secs(options.timeout_secs));
        let media_dir = &item.media_dir;
        let outcome = if item.is_media {
            convert_media(&item.source, &deadline).map(|markdown| {
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
            markdown_document::convert(&item.source, &runtime_dir, media_dir, &deadline)
        };
        let outcome = outcome.and_then(|document| {
            write_new_markdown(
                &output_root,
                &item.target,
                &document.markdown,
                &document.media,
            )?;
            Ok(document.warnings)
        });
        match outcome {
            Ok(warnings) => {
                let partial = !warnings.is_empty();
                if partial {
                    tracing::warn!(
                        file = %item.relative.display(),
                        warning_count = warnings.len(),
                        "转换部分内容未提取"
                    );
                    summary.partial += 1;
                } else {
                    summary.success += 1;
                }
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
            Err(message) => {
                summary.failed += 1;
                tracing::error!(
                    file = %item.relative.display(),
                    // 引擎错误可包含文档片段，只记录失败位置；详情仅供任务界面呈现。
                    kind = if message.contains("超时") { "timeout" } else { "conversion_or_output" },
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
        success = summary.success,
        partial = summary.partial,
        failed = summary.failed,
        skipped_existing = summary.skipped_existing,
        skipped_duplicate = summary.skipped_duplicate,
        stopped = summary.stopped,
        "转 Markdown 批次结束"
    );
    Ok(summary)
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

/// 结果名占用索引（T-11/F29）：按大小写折叠键分桶，桶内候选再用
/// [`compare_names`]（Windows `CompareStringOrdinal` 忽略大小写）精确确认。
/// 平铺模式登记全局结果名，层级模式登记「相对父目录+结果名」（见 `scan`）。
/// 折叠只用于缩小候选集：语义上过桶只会多比、不会漏比，occupied 查询近似 O(1)。
#[derive(Default)]
struct OccupiedIndex {
    buckets: std::collections::HashMap<Vec<u16>, Vec<OsString>>,
}

impl OccupiedIndex {
    fn contains(&self, name: &OsStr) -> bool {
        self.contains_with(name, &mut |left, right| {
            compare_names(left, right) == Ordering::Equal
        })
    }

    /// 可注入比较谓词的查询（测试用于断言比较次数增长阶）。
    fn contains_with(
        &self,
        name: &OsStr,
        is_equal: &mut dyn FnMut(&OsStr, &OsStr) -> bool,
    ) -> bool {
        self.buckets
            .get(&case_fold_key(name))
            .is_some_and(|bucket| bucket.iter().any(|existing| is_equal(existing, name)))
    }

    fn insert(&mut self, name: OsString) {
        self.buckets
            .entry(case_fold_key(&name))
            .or_default()
            .push(name);
    }
}

/// 折叠键：逐 UTF-16 单元做简单大写折叠。ASCII 走快路径；非 ASCII 采用
/// Unicode 单单元大写映射，多单元或无映射（含代理项）保留原单元。折叠与
/// `CompareStringOrdinal` 的逐单元大写口径一致，不一致的极端情形只会把
/// 候选落进不同桶后再精确比较（多比不漏比）。
fn case_fold_key(name: &OsStr) -> Vec<u16> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        name.encode_wide().map(fold_unit).collect()
    }
    #[cfg(not(windows))]
    {
        name.to_string_lossy()
            .encode_utf16()
            .map(fold_unit)
            .collect()
    }
}

fn fold_unit(unit: u16) -> u16 {
    if unit < 0x80 {
        return if (u16::from(b'a')..=u16::from(b'z')).contains(&unit) {
            unit - 32
        } else {
            unit
        };
    }
    if (0xD800..=0xDFFF).contains(&unit) {
        return unit;
    }
    let Some(ch) = char::from_u32(u32::from(unit)) else {
        return unit;
    };
    let mut upper = ch.to_uppercase();
    let Some(mapped) = upper.next() else {
        return unit;
    };
    if upper.next().is_some() || mapped.len_utf16() != 1 {
        return unit;
    }
    u16::try_from(u32::from(mapped)).unwrap_or(unit)
}

/// 结果名占用键（T-11）：平铺模式为全局结果名；层级模式为「相对父目录+
/// 结果名」——同一输出父目录内大小写不敏感等价的结果名互相挤占（提交走
/// rename_noreplace，等价名必然失败，必须在计划阶段计为同名跳过而不是浪费
/// 整份转换后计失败，A'-1），不同子目录互不影响（T-10 保留层级）。键经
/// [`OccupiedIndex`] 按大小写折叠比较。
fn occupancy_key(flat: bool, relative_parent: &Path, file_name: &OsStr) -> OsString {
    if flat {
        file_name.to_os_string()
    } else {
        relative_parent.join(file_name).into_os_string()
    }
}

#[cfg(test)]
fn scan(options: &Options, supported: &BTreeSet<String>) -> Result<Plan, String> {
    let cancel = AtomicBool::new(false);
    scan_cancelable(options, supported, &cancel)
}

fn scan_cancelable(
    options: &Options,
    supported: &BTreeSet<String>,
    cancel: &AtomicBool,
) -> Result<Plan, String> {
    if cancel.load(AtomicOrdering::Acquire) {
        return Err("扫描已取消".to_string());
    }
    let input = checked_directory(&options.input_dir)?;
    let output = checked_directory(&options.output_dir)?;
    if path_equal(&input, &output) {
        return Err("输入与输出目录不能相同".to_string());
    }
    let mut paths = Vec::new();
    // T-09 只授权「输出位于输入目录子树内」时排除整棵输出子树；输出是输入的
    // 祖先或两者在不同分支时不排除任何目录——否则输入子树会因前缀判定被整体
    // 静默跳过，违反 T-07 的递归处理要求。
    let output_in_input = path_begins_with(&output, &input);
    let mut pending = vec![input.clone()];
    while let Some(dir) = pending.pop() {
        if cancel.load(AtomicOrdering::Acquire) {
            return Err("扫描已取消".to_string());
        }
        for entry in fs::read_dir(&dir).map_err(|e| format!("无法扫描 {}：{e}", dir.display()))?
        {
            if cancel.load(AtomicOrdering::Acquire) {
                return Err("扫描已取消".to_string());
            }
            let entry = entry.map_err(|e| format!("无法读取 {} 的目录项：{e}", dir.display()))?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|e| format!("无法检查 {}：{e}", path.display()))?;
            if fsutil::is_link(&metadata) {
                continue;
            }
            if metadata.is_dir() {
                if !(output_in_input && path_begins_with(&path, &output)) {
                    pending.push(path);
                }
            } else if metadata.is_file()
                && path
                    .extension()
                    .and_then(OsStr::to_str)
                    .is_some_and(|ext| supported.contains(&ext.to_ascii_lowercase()))
            {
                paths.push(path);
            }
        }
    }
    paths.sort_by(|a, b| {
        compare_paths(
            a.strip_prefix(&input).unwrap_or(a),
            b.strip_prefix(&input).unwrap_or(b),
        )
    });
    let mut occupied = OccupiedIndex::default();
    let mut media_occupied: BTreeMap<PathBuf, OccupiedIndex> = BTreeMap::new();
    let mut plan = Plan {
        output_root: output.clone(),
        ..Plan::default()
    };
    for source in paths {
        let relative = source
            .strip_prefix(&input)
            .map_err(|e| format!("输入路径超出根目录：{e}"))?
            .to_path_buf();
        let file_name = markdown_name(&source)?;
        // 冲突决胜与平铺一致：排序靠前者保留，后续计 skipped_duplicate（T-24：
        // 与转换失败可区分）。
        let occupancy = occupancy_key(
            options.flat,
            relative.parent().unwrap_or_else(|| Path::new("")),
            &file_name,
        );
        if occupied.contains(occupancy.as_os_str()) {
            plan.summary.skipped_duplicate += 1;
            continue;
        }
        occupied.insert(occupancy);
        let target = if options.flat {
            output.join(file_name)
        } else {
            output
                .join(relative.parent().unwrap_or_else(|| Path::new("")))
                .join(file_name)
        };
        if target.exists() {
            plan.summary.skipped_existing += 1;
            continue;
        }
        let is_media = source
            .extension()
            .and_then(OsStr::to_str)
            .is_some_and(|ext| MEDIA.contains(&ext.to_ascii_lowercase().as_str()));
        let media_dir = if is_media {
            String::new()
        } else {
            let parent = target.parent().unwrap_or(&output);
            let occupied_media = media_occupied.entry(parent.to_path_buf()).or_default();
            media_dir_name_reserved(parent, &target, occupied_media)?
        };
        plan.items.push(Item {
            source,
            relative,
            target,
            is_media,
            media_dir,
        });
    }
    Ok(plan)
}

/// 钉住发布物的格式清单：与 `markdown_assets::XBERG_TAG` 同一钉版纪律（XB-09
/// 版本锚是发布 tag 与成员摘要，不是运行期探测）。真实发布物 run49.1 的 worker
/// 协议只提供 `extract` / `ocr_snapshot` / `snapshot_state` / `transcribe`
/// （`formats` / `capabilities` / `cancel` 等共享协议扩展「已实施、尚未发布验收」，
/// 见 Xberg 仓 docs/requirements/WORKER.md），且 XB-14 禁止为查询另起 Xberg
/// 进程，因此清单随钉住版本内置于资源，升级引擎 tag 时必须同步再生成
/// （`xberg.exe formats --format json`，只读诊断）。
fn supported_formats(groups: &[FormatGroup]) -> Result<BTreeSet<String>, String> {
    let table: serde_json::Value =
        serde_json::from_str(include_str!("../resources/markdown-xberg-formats.json"))
            .map_err(|e| format!("内置 Xberg 格式清单无效：{e}"))?;
    if table.get("tag").and_then(serde_json::Value::as_str) != Some(markdown_assets::XBERG_TAG) {
        return Err(
            "内置 Xberg 格式清单与固定版本不一致；请同步再生成 markdown-xberg-formats.json"
                .to_string(),
        );
    }
    let rows = table
        .get("formats")
        .ok_or("内置 Xberg 格式清单缺少 formats 字段")?;
    let bytes = serde_json::to_vec(rows).map_err(|e| e.to_string())?;
    let mut selected = parse_formats(&bytes, groups)?;
    if groups.contains(&FormatGroup::Media) {
        selected.extend(MEDIA.iter().map(|extension| (*extension).to_string()));
    }
    Ok(selected)
}

/// 有界且可取消地运行格式清单探测（F21）：超时或取消后 kill + 限时收尾，
/// 输出被截断时如实报错，不得把半截清单当完整结果。
#[cfg(test)]
fn run_formats_probe(
    command: &mut Command,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<crate::process::CapturedOutput, String> {
    let result = crate::process::run_with_timeout_cancel(command, timeout, cancel);
    let output = match result {
        Ok(output) => output,
        Err(error) => {
            // 先判取消：停止不是错误（T-23），由调用方转为已停止汇总。
            if cancel.load(AtomicOrdering::Relaxed) {
                return Err("格式探测已取消".to_string());
            }
            return Err(format!("无法读取 Xberg 格式清单：{error:#}"));
        }
    };
    if !output.status.success() {
        return Err(format!(
            "Xberg 格式清单失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    if output.stdout_truncated {
        return Err("Xberg 格式清单输出被截断（超过捕获上限），不能当完整清单".to_string());
    }
    Ok(output)
}

/// 解析 Xberg formats JSON 并按分组筛入支持集（纯函数，便于回归）。
fn parse_formats(stdout: &[u8], groups: &[FormatGroup]) -> Result<BTreeSet<String>, String> {
    let rows: Vec<serde_json::Value> =
        serde_json::from_slice(stdout).map_err(|e| format!("Xberg 格式清单无效：{e}"))?;
    let mut selected = BTreeSet::new();
    for row in rows {
        let Some(extension) = row.get("extension").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let extension = extension.trim_start_matches('.').to_ascii_lowercase();
        if extension.is_empty() || extension.contains('.') {
            continue;
        }
        let mime = row
            .get("mime_type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if selected_xberg_extension(&extension, mime, groups) {
            selected.insert(extension);
        }
    }
    Ok(selected)
}

fn selected_xberg_extension(extension: &str, mime: &str, groups: &[FormatGroup]) -> bool {
    if (mime.starts_with("audio/") || mime.starts_with("video/")) && !MEDIA.contains(&extension) {
        return false;
    }
    groups.contains(&classify_format(extension, mime))
}

fn classify_format(extension: &str, mime: &str) -> FormatGroup {
    if PDF.contains(&extension) {
        FormatGroup::Pdf
    } else if OFFICE.contains(&extension)
        || mime.contains("wordprocessing")
        || mime.contains("spreadsheet")
        || mime.contains("presentation")
        || mime.contains("opendocument")
    {
        FormatGroup::Office
    } else if MEDIA.contains(&extension) {
        FormatGroup::Media
    } else if IMAGES.contains(&extension) || mime.starts_with("image/") {
        FormatGroup::Images
    } else {
        FormatGroup::Other
    }
}

fn checked_directory(path: &Path) -> Result<PathBuf, String> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) => current.push(component.as_os_str()),
            Component::RootDir | Component::Normal(_) => {
                current.push(component.as_os_str());
                let metadata = fs::symlink_metadata(&current)
                    .map_err(|e| format!("无法检查目录 {}：{e}", current.display()))?;
                if fsutil::is_link(&metadata) {
                    return Err(format!(
                        "目录路径含符号链接或 junction：{}",
                        current.display()
                    ));
                }
            }
            Component::CurDir => (),
            Component::ParentDir => return Err("请选择不含上级跳转的目录路径".to_string()),
        }
    }
    let canonical =
        fs::canonicalize(path).map_err(|e| format!("无法访问目录 {}：{e}", path.display()))?;
    if !canonical.is_dir() {
        return Err(format!("不是目录：{}", path.display()));
    }
    Ok(canonical)
}

fn markdown_name(source: &Path) -> Result<OsString, String> {
    let stem = source
        .file_stem()
        .ok_or_else(|| format!("无法取得文件名：{}", source.display()))?;
    let ext = source
        .extension()
        .and_then(OsStr::to_str)
        .ok_or_else(|| format!("缺少扩展名：{}", source.display()))?;
    let mut output = OsString::from(stem);
    output.push("_");
    output.push(ext.to_ascii_lowercase());
    output.push(".md");
    Ok(output)
}

fn path_equal(left: &Path, right: &Path) -> bool {
    compare_paths(left, right) == Ordering::Equal
}

fn path_begins_with(path: &Path, prefix: &Path) -> bool {
    let mut path_parts = path.components();
    for prefix_part in prefix.components() {
        let Some(path_part) = path_parts.next() else {
            return false;
        };
        if compare_names(path_part.as_os_str(), prefix_part.as_os_str()) != Ordering::Equal {
            return false;
        }
    }
    true
}

fn compare_paths(left: &Path, right: &Path) -> Ordering {
    let case_insensitive = compare_paths_insensitive(left, right);
    if case_insensitive != Ordering::Equal {
        return case_insensitive;
    }
    compare_paths_original(left, right)
}

fn compare_paths_insensitive(left: &Path, right: &Path) -> Ordering {
    compare_component_wise(left, right, compare_names)
}

// T-11 决胜序等价性论证（A'-低危2）：合同写「相同键再按原始路径 UTF-16 单元
// 序列决胜」，本实现按「逐段原始 UTF-16（首段分出胜负即停）」。两者在决胜段内
// 数学等价：决胜段仅在 [`compare_paths_insensitive`] 全段相等时进入，此时两路径
// 分段数相同、前 i 段原始相等、第 i 段两两大小写不敏感相等但原始不同。大小写
// 不敏感相等的两段不可能互为真前缀——若 c 是 d 的真前缀则 d=c+e（e 非空），逐
// 单元折叠不消灭单元、段内也不含分隔符，fold(d)=fold(c)+fold(e)≠fold(c)，与不
// 区分大小写相等矛盾——故第 i 段的第一差异单元必在两段内部的同一位置，逐段与
// 整路径两种比较的第一差异单元相同，结论一致（性质由
// `flat_tiebreak_per_component_matches_whole_path_sequence` 固定）。
fn compare_paths_original(left: &Path, right: &Path) -> Ordering {
    compare_component_wise(left, right, compare_original)
}

/// 逐段比较的公共骨架：同一分段数与短路语义下，仅段内比较器不同。
/// T-11 决胜序（A'-低危2）依赖「两条路径同为逐段比较」这一不变量，
/// 提取后该不变量只有一处实现。
fn compare_component_wise(
    left: &Path,
    right: &Path,
    compare: impl Fn(&OsStr, &OsStr) -> Ordering,
) -> Ordering {
    let mut left_parts = left.components();
    let mut right_parts = right.components();
    loop {
        match (left_parts.next(), right_parts.next()) {
            (Some(a), Some(b)) => {
                let order = compare(a.as_os_str(), b.as_os_str());
                if order != Ordering::Equal {
                    return order;
                }
            }
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (None, None) => return Ordering::Equal,
        }
    }
}

#[cfg(windows)]
fn compare_names(left: &OsStr, right: &OsStr) -> Ordering {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Globalization::CompareStringOrdinal;

    let a: Vec<_> = left.encode_wide().collect();
    let b: Vec<_> = right.encode_wide().collect();
    let Ok(a_len) = i32::try_from(a.len()) else {
        return a.cmp(&b);
    };
    let Ok(b_len) = i32::try_from(b.len()) else {
        return a.cmp(&b);
    };
    // SAFETY: 两个 UTF-16 缓冲区在整个同步调用期间有效，长度与缓冲区一致。
    match unsafe { CompareStringOrdinal(a.as_ptr(), a_len, b.as_ptr(), b_len, 1) } {
        1 => Ordering::Less,
        2 => Ordering::Equal,
        3 => Ordering::Greater,
        _ => a.cmp(&b),
    }
}

#[cfg(not(windows))]
fn compare_names(left: &OsStr, right: &OsStr) -> Ordering {
    left.to_string_lossy()
        .to_lowercase()
        .cmp(&right.to_string_lossy().to_lowercase())
}

#[cfg(windows)]
fn compare_original(left: &OsStr, right: &OsStr) -> Ordering {
    use std::os::windows::ffi::OsStrExt;
    left.encode_wide().cmp(right.encode_wide())
}

#[cfg(not(windows))]
fn compare_original(left: &OsStr, right: &OsStr) -> Ordering {
    left.cmp(right)
}

/// T-14：媒体目录名 = 产物主干（空白折叠为下划线，去掉尾部点/空格）+ `_media`。
/// 与目标所在输出父目录内已有普通项同名时追加序号让路；链接/junction 直接拒绝。
/// 同一批次内已分配的媒体目录也计入占用，避免空白折叠后的名称碰撞。
fn media_dir_name(output_root: &Path, target: &Path) -> Result<String, String> {
    let parent = target.parent().unwrap_or(output_root);
    let mut occupied = OccupiedIndex::default();
    media_dir_name_reserved(parent, target, &mut occupied)
}

fn media_dir_name_reserved(
    parent: &Path,
    target: &Path,
    occupied: &mut OccupiedIndex,
) -> Result<String, String> {
    let stem = target
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("media");
    let mut base = stem.split_whitespace().collect::<Vec<_>>().join("_");
    base = base.trim_end_matches(['.', ' ']).to_string();
    if base.is_empty() {
        base = "media".to_string();
    }
    let mut candidate = format!("{base}_media");
    let mut sequence = 1u32;
    loop {
        if occupied.contains(OsStr::new(&candidate)) {
            sequence = sequence.saturating_add(1);
            candidate = format!("{base}_{sequence}_media");
            continue;
        }
        let path = parent.join(&candidate);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if fsutil::is_link(&metadata) => {
                return Err(format!("媒体目录含符号链接或 junction：{}", path.display()));
            }
            Ok(_) => {
                sequence = sequence.saturating_add(1);
                candidate = format!("{base}_{sequence}_media");
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                occupied.insert(OsString::from(&candidate));
                return Ok(candidate);
            }
            Err(error) => {
                return Err(format!("无法检查媒体目录 {}：{error}", path.display()));
            }
        }
    }
}

fn prepare_media_destination(
    parent: &Path,
    relative: &str,
    created_dirs: &mut Vec<PathBuf>,
) -> Result<PathBuf, String> {
    let components: Vec<_> = Path::new(relative).components().collect();
    if components.is_empty() {
        return Err("媒体图片路径为空".to_string());
    }
    let mut current = parent.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err("媒体图片路径含非法目录段".to_string());
        };
        current.push(name);
        let is_file = index + 1 == components.len();
        match fs::symlink_metadata(&current) {
            Ok(metadata) if fsutil::is_link(&metadata) => {
                return Err(format!(
                    "媒体输出路径含符号链接或 junction：{}",
                    current.display()
                ));
            }
            Ok(_) if is_file => {
                return Err(format!(
                    "媒体图片目标已存在，不会覆盖：{}",
                    current.display()
                ));
            }
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Err(format!("媒体输出路径不是目录：{}", current.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !is_file => {
                fs::create_dir(&current)
                    .map_err(|error| format!("无法创建媒体目录 {}：{error}", current.display()))?;
                created_dirs.push(current.clone());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "无法检查媒体输出路径 {}：{error}",
                    current.display()
                ));
            }
        }
    }
    Ok(current)
}

fn write_new_markdown(
    output_root: &Path,
    target: &Path,
    content: &str,
    media: &[markdown_document::MediaFile],
) -> Result<(), String> {
    let parent = target.parent().ok_or_else(|| "结果目录无效".to_string())?;
    // T-12 保险丝：扫描已跳过既有结果，这里目标再出现属并发/外部改动——
    // 在动任何 media 文件之前直接拒绝，避免「md 旧、图新」的错位组合。
    if fs::symlink_metadata(target).is_ok() {
        return Err("无法提交新结果（已有结果不会覆盖）：目标已存在".to_string());
    }
    let relative_parent = parent
        .strip_prefix(output_root)
        .map_err(|e| format!("结果不在输出目录内：{e}"))?;
    let mut current = output_root.to_path_buf();
    for component in relative_parent.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err("输出路径含非法目录段".to_string());
        }
        current.push(component.as_os_str());
        match fs::create_dir(&current) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(format!("无法创建输出目录 {}：{error}", current.display())),
        }
        let metadata = fs::symlink_metadata(&current)
            .map_err(|error| format!("无法检查输出目录 {}：{error}", current.display()))?;
        if fsutil::is_link(&metadata) || !metadata.is_dir() {
            return Err(format!("输出路径不是普通目录：{}", current.display()));
        }
    }
    // T-14：先落图片（本次任务的未提交材料），md 最后以不覆盖改名提交——
    // md 在场即代表 media 已完整（T-25：不留半成品）；中途失败回滚本次已写
    // 图片并删除空媒体目录，不触碰既有文件。
    let mut created: Vec<std::path::PathBuf> = Vec::new();
    let mut created_dirs: Vec<std::path::PathBuf> = Vec::new();
    let media_result = (|| -> Result<(), String> {
        for file in media {
            let destination = prepare_media_destination(parent, &file.relative, &mut created_dirs)?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)
                .map_err(|e| format!("无法创建图片 {}：{e}", destination.display()))?;
            created.push(destination.clone());
            output
                .write_all(&file.bytes)
                .map_err(|e| format!("无法写入图片 {}：{e}", destination.display()))?;
            output
                .sync_all()
                .map_err(|e| format!("同步图片失败 {}：{e}", destination.display()))?;
        }
        Ok(())
    })();
    if let Err(error) = media_result {
        for path in created.iter().rev() {
            let _ = fs::remove_file(path);
        }
        for path in created_dirs.iter().rev() {
            let _ = fs::remove_dir(path);
        }
        return Err(error);
    }
    let temp = parent.join(format!(".jch-markdown-{}.tmp", uuid::Uuid::new_v4()));
    let write_result = (|| -> Result<(), String> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|e| format!("无法创建临时结果：{e}"))?;
        file.write_all(content.as_bytes())
            .map_err(|e| format!("写入 Markdown 失败：{e}"))?;
        file.sync_all()
            .map_err(|e| format!("同步 Markdown 失败：{e}"))?;
        drop(file);
        // T-25/T-12：完整成功后以不覆盖改名提交（temp 与目标同目录同卷）。
        // 不用 fs::rename——Windows 上它会替换已存在目标；不用硬链接——
        // exFAT/FAT 等文件系统不支持，会把输出在这些卷上的结果全部判失败。
        fsutil::rename_noreplace(&temp, target)
            .map_err(|e| format!("无法提交新结果（已有结果不会覆盖）：{e:#}"))?;
        Ok(())
    })();
    let _ = fs::remove_file(&temp);
    if write_result.is_err() {
        // 提交失败（含目标被外部占用的保险丝）：回滚本次已写图片并删除空
        // 媒体目录，不把无主媒体留给旧结果（T-25）。
        for path in created.iter().rev() {
            let _ = fs::remove_file(path);
        }
        for path in created_dirs.iter().rev() {
            let _ = fs::remove_dir(path);
        }
    }
    write_result
}

/// 转换一个媒体文件：验证媒体资产后，经会话共享进程请求 transcribe。
fn convert_media(path: &Path, deadline: &markdown_document::Deadline) -> Result<String, String> {
    let component = markdown_assets::media_component_dir().map_err(|error| {
        format!("共享 Xberg 的媒体组件未就绪，请检查已保存目录的模型和运行库：{error}")
    })?;
    markdown_assets::validate_media()?;
    let response = crate::xberg_runtime::request(
        &component,
        serde_json::json!({"command":"transcribe","path":path}),
        deadline.remaining(),
        &AtomicBool::new(false),
    )?;
    let response = crate::xberg_runtime::checked(response)?;
    let markdown = response["markdown"]
        .as_str()
        .ok_or_else(|| "Xberg 转录响应缺少 markdown".to_string())?;
    if markdown.trim().is_empty() {
        return Err("Xberg 转录响应的 markdown 为空，未生成有效媒体结果".to_string());
    }
    Ok(markdown.to_owned())
}

/// E2E 专用接缝（#[doc(hidden)]）：单个媒体文件的真实转录路径（组件解析 →
/// 常驻进程 → 协议请求 → markdown）。批处理语义（T-22/T-23/T-24/T-29）由
/// [`run`] 承载；本接缝仅供自动化验收从真实实现驱动一次完整转录。
#[doc(hidden)]
pub fn e2e_convert_media(path: &Path, timeout_secs: u64) -> Result<String, String> {
    if timeout_secs == 0 {
        return Err("单文件超时必须为正整秒".to_string());
    }
    let deadline = markdown_document::Deadline::new(Duration::from_secs(timeout_secs));
    convert_media(path, &deadline)
}

#[doc(hidden)]
pub fn test_write_new_markdown(
    output_root: &Path,
    target: &Path,
    content: &str,
    media: &[markdown_document::MediaFile],
) -> Result<(), String> {
    write_new_markdown(output_root, target, content, media)
}

#[doc(hidden)]
pub fn test_media_dir_name(output_root: &Path, target: &Path) -> Result<String, String> {
    media_dir_name(output_root, target)
}

#[doc(hidden)]
pub fn test_scan_media_dirs(options: &Options) -> Result<Vec<String>, String> {
    let supported: BTreeSet<String> = [
        "docx", "docm", "dotx", "dotm", "pptx", "pptm", "ppsx", "potx", "potm", "xlsx", "xlsm",
        "xlsb", "xltx", "xltm", "xlam", "odt", "ods", "odp",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    let cancel = AtomicBool::new(false);
    Ok(scan_cancelable(options, &supported, &cancel)?
        .items
        .into_iter()
        .map(|item| item.media_dir)
        .collect())
}

#[doc(hidden)]
pub fn test_scan_count_with_cancel(
    options: &Options,
    cancel: &AtomicBool,
) -> Result<usize, String> {
    let supported: BTreeSet<String> = ["pdf", "docx"].into_iter().map(str::to_string).collect();
    Ok(scan_cancelable(options, &supported, cancel)?.items.len())
}

#[cfg(test)]
mod tests {
    use super::{
        compare_names, compare_paths, compare_paths_insensitive, media_dir_name, occupancy_key,
        parse_formats, run_formats_probe, scan, selected_xberg_extension, write_new_markdown,
        FormatGroup, OccupiedIndex, Options,
    };
    use crate::markdown_document::MediaFile;
    use std::thread;
    use std::time::Instant;
    use std::{
        cmp::Ordering,
        collections::BTreeSet,
        ffi::{OsStr, OsString},
        fs,
        path::{Path, PathBuf},
        process::Command,
        sync::atomic::{AtomicBool, Ordering as AtomicOrdering},
        time::Duration,
    };

    fn supported() -> BTreeSet<String> {
        ["pdf", "docx"].into_iter().map(str::to_string).collect()
    }

    // 覆盖 T-07、T-08：仅 MP4/M4A 进入媒体链路，Xberg 清单中的其他媒体格式不得误入文档转换。
    #[test]
    fn other_xberg_media_formats_are_not_selected() {
        let groups = [FormatGroup::Other, FormatGroup::Media];
        assert!(!selected_xberg_extension("wmv", "video/x-ms-wmv", &groups));
        assert!(!selected_xberg_extension("mp3", "audio/mpeg", &groups));
        assert!(selected_xberg_extension("mp4", "video/mp4", &groups));
        assert!(selected_xberg_extension("m4a", "audio/mp4", &groups));
    }

    fn options(input_dir: PathBuf, output_dir: PathBuf, flat: bool) -> Options {
        Options {
            input_dir,
            output_dir,
            flat,
            groups: vec![FormatGroup::Pdf, FormatGroup::Office],
            timeout_secs: 21_600,
        }
    }

    // 覆盖 T-09、T-10、T-11：只读纳入 Git 树，输出子树排除，保留层级。
    #[test]
    fn scan_includes_git_project_but_excludes_output_subtree() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("input");
        let output = input.join("results");
        fs::create_dir_all(input.join("repo/.git")).unwrap();
        fs::create_dir_all(&output).unwrap();
        fs::write(input.join("repo/a.PDF"), b"pdf").unwrap();
        fs::write(output.join("generated.pdf"), b"pdf").unwrap();

        let plan = scan(&options(input, output.clone(), false), &supported()).unwrap();
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].relative, PathBuf::from("repo/a.PDF"));
        assert_eq!(
            plan.items[0].target,
            fs::canonicalize(&output).unwrap().join("repo/a_pdf.md")
        );
    }

    // 覆盖 T-11：平铺同名按稳定路径排序只保留第一份。
    #[test]
    fn flat_collision_keeps_sorted_first_and_counts_skip() {
        assert_eq!(
            compare_paths(
                PathBuf::from("A/Z/x.PDF").as_path(),
                PathBuf::from("a/a/x.pdf").as_path(),
            ),
            Ordering::Greater
        );
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("input");
        let output = temp.path().join("output");
        fs::create_dir_all(input.join("A")).unwrap();
        fs::create_dir_all(input.join("b")).unwrap();
        fs::create_dir_all(&output).unwrap();
        fs::write(input.join("A/x.PDF"), b"first").unwrap();
        fs::write(input.join("b/x.pdf"), b"second").unwrap();

        let plan = scan(&options(input, output.clone(), true), &supported()).unwrap();
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].relative, PathBuf::from("A/x.PDF"));
        assert_eq!(
            plan.items[0].target,
            fs::canonicalize(&output).unwrap().join("x_pdf.md")
        );
        assert_eq!(plan.summary.skipped_duplicate, 1);
    }

    // 覆盖 T-11（A'-低危2）：平铺决胜段（逐段大小写不敏感全等价、同分段结构）
    // 里「逐段原始 UTF-16（首段分出胜负即停）」与合同「原始路径 UTF-16 单元
    // 序列」必须选出同序——等价性依据：决胜段内各段两两大小写不敏感相等，而
    // 大小写不敏感相等的两段不可能互为真前缀（逐单元折叠不消灭单元、段内无
    // 分隔符），故两种口径的第一差异单元必落在同一段内同一位置。本用例跨多段、
    // 各段大小写形态不同（A\b\x.pdf vs a\B\x.pdf），固定该性质防回归。
    #[test]
    fn flat_tiebreak_per_component_matches_whole_path_sequence() {
        for (left, right) in [
            ("A/b/x.PDF", "a/B/x.pdf"),
            ("a/A/x.pdf", "a/a/x.pdf"),
            ("Aa/b/x.pdf", "aA/B/x.pdf"),
            ("deep/A/b/x.PDF", "deep/a/B/x.pdf"),
        ] {
            let left = Path::new(left);
            let right = Path::new(right);
            assert_eq!(
                compare_paths_insensitive(left, right),
                Ordering::Equal,
                "用例必须处于决胜段（逐段大小写不敏感等价）：{left:?} vs {right:?}"
            );
            assert_eq!(
                compare_paths(left, right),
                whole_path_original_order(left, right),
                "逐段决胜必须与合同整路径序列决胜一致：{left:?} vs {right:?}"
            );
        }
        assert_eq!(
            compare_paths(Path::new("A/b/x.PDF"), Path::new("a/B/x.pdf")),
            Ordering::Less,
            "首段大写 A（U+0041）原始序数小于小写 a（U+0061），平铺跳过时保留前者"
        );
    }

    /// 合同 T-11 的决胜口径参照实现：整条相对路径的原始 UTF-16 单元序列比较。
    fn whole_path_original_order(left: &Path, right: &Path) -> Ordering {
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            left.as_os_str()
                .encode_wide()
                .cmp(right.as_os_str().encode_wide())
        }
        #[cfg(not(windows))]
        {
            // 非 Windows 仅作参照：ASCII 用例下按 Unicode 标量比较与 UTF-16
            // 单元序列同序。
            left.as_os_str()
                .to_string_lossy()
                .cmp(&right.as_os_str().to_string_lossy())
        }
    }

    // 覆盖 T-11、T-12、T-24（A'-1 回归，键语义单元级）：层级模式的占用键 =
    // 「相对父目录+结果名」且大小写不敏感比较——同父目录内大小写变体结果名
    // （对应输入 a.PDF 与 a.pdf 在大小写敏感目录共存时的等价目标）必须命中
    // 跳过，父目录大小写变体同样等价；不同父目录不得挤占（T-10）；平铺键
    // 不含父目录（既有平铺决胜序不变）。
    // 物理场景说明：默认 NTFS 大小写不敏感，同目录两个大小写变体文件无法
    // 共存（第二次写入覆盖同一文件；fsutil 启用目录级大小写敏感需要管理员
    // 权限、CreateFileW(FILE_FLAG_POSIX_SEMANTICS) 也无法绕过），故同键跳过
    // 以本单元断言 + 不同目录集成护栏（hierarchical_duplicate_in_different_
    // directories_both_convert）共同锁定；scan 接线由两者与既有平铺测试约束。
    #[test]
    fn hierarchical_occupancy_key_collides_only_within_same_parent() {
        let mut occupied = OccupiedIndex::default();
        occupied.insert(occupancy_key(
            false,
            Path::new("docs"),
            OsStr::new("a_pdf.md"),
        ));
        assert!(
            occupied.contains(
                occupancy_key(false, Path::new("docs"), OsStr::new("A_PDF.MD")).as_os_str()
            ),
            "同父目录的结果名大小写变体必须命中占用（A'-1：第二个计同名跳过而非提交失败）"
        );
        assert!(
            occupied.contains(
                occupancy_key(false, Path::new("Docs"), OsStr::new("a_pdf.md")).as_os_str()
            ),
            "父目录大小写变体属于同一输出父目录，同样必须命中"
        );
        assert!(
            !occupied.contains(
                occupancy_key(false, Path::new("other"), OsStr::new("a_pdf.md")).as_os_str()
            ),
            "不同父目录不得挤占（T-10 层级保留）"
        );
        assert!(
            !occupied.contains(
                occupancy_key(true, Path::new("docs"), OsStr::new("A_PDF.MD")).as_os_str()
            ),
            "平铺键不含父目录，不得被层级键误命中（平铺决胜序不受影响）"
        );
    }

    // 覆盖 T-10、T-11（A'-1 伴随护栏）：层级模式的占用登记只约束同一输出父
    // 目录；不同子目录的同名文件各自独立转换，不得因全局重名被误跳过。
    #[test]
    fn hierarchical_duplicate_in_different_directories_both_convert() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("input");
        let output = temp.path().join("output");
        fs::create_dir_all(input.join("docs")).unwrap();
        fs::create_dir_all(input.join("notes")).unwrap();
        fs::create_dir_all(&output).unwrap();
        fs::write(input.join("docs/a.PDF"), b"one").unwrap();
        fs::write(input.join("notes/a.pdf"), b"two").unwrap();

        let plan = scan(&options(input, output, false), &supported()).unwrap();
        assert_eq!(plan.items.len(), 2, "不同子目录的同名文件互不挤占");
        assert_eq!(plan.summary.skipped_duplicate, 0);
    }

    // 覆盖 T-07、T-09（输出目录是输入目录的祖先时：输入子树不被前缀判定排除，
    // 递归处理全部匹配文件——只有输出位于输入子树内时才排除输出子树）
    #[test]
    fn scan_keeps_input_subtree_when_output_is_ancestor() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("data").join("reports");
        let output = temp.path().join("data");
        fs::create_dir_all(input.join("sub")).unwrap();
        fs::write(input.join("top.PDF"), b"pdf").unwrap();
        fs::write(input.join("sub/inner.pdf"), b"pdf").unwrap();

        let plan = scan(&options(input, output.clone(), false), &supported()).unwrap();
        assert_eq!(
            plan.items.len(),
            2,
            "输出为输入祖先时子树全部纳入递归处理：{:?}",
            plan.items
        );
        assert_eq!(plan.summary.skipped_existing, 0);
        assert_eq!(plan.summary.skipped_duplicate, 0);
    }

    // 覆盖 T-09：输入输出重合时扫描前拒绝。
    #[test]
    fn same_input_and_output_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().to_path_buf();
        let error = scan(&options(directory.clone(), directory, false), &supported()).unwrap_err();
        assert!(error.contains("不能相同"));
    }

    // 覆盖 T-12、T-25：提交时已有结果不得被覆盖，临时文件须清除。
    #[test]
    fn atomic_commit_refuses_existing_result() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("a_pdf.md");
        fs::write(&target, b"old").unwrap();
        let error = write_new_markdown(temp.path(), &target, "new", &[]).unwrap_err();
        assert!(error.contains("已有结果不会覆盖"));
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    }

    // 覆盖 T-14/T-25：媒体文件先落盘、md 最后提交；引用的图片与 md 同批可见。
    #[test]
    fn media_files_written_before_markdown_commit() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("a_docx.md");
        let media = vec![MediaFile {
            relative: "a_docx_media/image_0.png".to_string(),
            bytes: b"img".to_vec(),
        }];
        write_new_markdown(temp.path(), &target, "body", &media).unwrap();
        assert_eq!(
            fs::read(temp.path().join("a_docx_media").join("image_0.png")).unwrap(),
            b"img"
        );
        assert_eq!(fs::read(&target).unwrap(), b"body");
    }

    // 覆盖 T-25：媒体写入失败时本次已写图片回滚、md 不提交，不留半成品。
    #[test]
    fn media_write_failure_rolls_back_and_fails_file() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("a_docx.md");
        fs::write(temp.path().join("a_docx_media"), b"occupied").unwrap();
        let media = vec![MediaFile {
            relative: "a_docx_media/image_0.png".to_string(),
            bytes: b"img".to_vec(),
        }];
        let error = write_new_markdown(temp.path(), &target, "body", &media).unwrap_err();
        assert!(
            error.contains("不是目录") || error.contains("已存在"),
            "{error}"
        );
        assert!(!target.exists(), "md 不得提交");
        assert_eq!(
            fs::read_dir(temp.path()).unwrap().count(),
            1,
            "既有占用项不得被删除"
        );
        assert_eq!(
            fs::read(temp.path().join("a_docx_media")).unwrap(),
            b"occupied"
        );
    }

    // 覆盖 T-14：媒体目录名折叠空白、避开与既有普通文件的占用（追加序号）。
    #[test]
    fn media_dir_name_sanitizes_and_avoids_file_occupancy() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("my report_docx.md");
        assert_eq!(
            media_dir_name(temp.path(), &target).unwrap(),
            "my_report_docx_media",
            "空白折叠为下划线"
        );
        fs::write(temp.path().join("my_report_docx_media"), "占用".as_bytes()).unwrap();
        assert_eq!(
            media_dir_name(temp.path(), &target).unwrap(),
            "my_report_docx_2_media",
            "与普通文件同名时追加序号让路"
        );
    }

    // ── F21：格式清单探测必须有界、可取消，输出截断不得当完整清单 ──

    #[cfg(windows)]
    fn hanging_command() -> Command {
        let mut command = Command::new("powershell");
        command.args(["-NoProfile", "-Command", "Start-Sleep -Seconds 60"]);
        command
    }

    #[cfg(not(windows))]
    fn hanging_command() -> Command {
        let mut command = Command::new("sleep");
        command.arg("60");
        command
    }

    #[cfg(windows)]
    fn spewing_command(mib: usize) -> Command {
        let mut command = Command::new("powershell");
        command.args([
            "-NoProfile",
            "-Command",
            &format!("[Console]::Out.Write(('x' * {}))", mib * 1024 * 1024),
        ]);
        command
    }

    // 覆盖 T-22/T-24（F21）：探测进程挂起时必须受总超时约束，宿主在预算内恢复。
    #[test]
    fn formats_probe_times_out_on_hanging_process() {
        let mut command = hanging_command();
        let cancel = AtomicBool::new(false);
        let started = Instant::now();
        let result = run_formats_probe(&mut command, Duration::from_millis(400), &cancel);
        assert!(result.is_err(), "挂起进程必须在探测超时后失败");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "探测超时后应立即返回，实际 {:?}",
            started.elapsed()
        );
    }

    // 覆盖 T-23（F21）：停止请求必须立刻中断格式探测，不等满超时。
    #[test]
    fn formats_probe_stops_promptly_on_cancel() {
        use std::sync::Arc;
        let mut command = hanging_command();
        let cancel = Arc::new(AtomicBool::new(false));
        let setter = cancel.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            setter.store(true, AtomicOrdering::Relaxed);
        });
        let started = Instant::now();
        let result = run_formats_probe(&mut command, Duration::from_secs(60), &cancel);
        assert!(result.is_err(), "取消后探测应失败");
        assert!(cancel.load(AtomicOrdering::Relaxed));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "取消后应立即返回，实际 {:?}",
            started.elapsed()
        );
    }

    // 覆盖 T-24（F21）：探测输出超过捕获上限时报截断错误，不得当完整清单解析。
    #[cfg(windows)]
    #[test]
    fn formats_probe_reports_truncated_output() {
        let mut command = spewing_command(9);
        let cancel = AtomicBool::new(false);
        let result = run_formats_probe(&mut command, Duration::from_secs(60), &cancel);
        let error = result.expect_err("截断的输出不得当成功");
        assert!(
            error.contains("截断") || error.contains("不完整"),
            "应说明输出不完整：{error}"
        );
    }

    // 覆盖 T-07/T-08（F21 守护）：探测正常返回时捕获完整输出。
    #[cfg(windows)]
    #[test]
    fn formats_probe_captures_valid_output() {
        let mut command = Command::new("powershell");
        command.args([
            "-NoProfile",
            "-Command",
            "[Console]::Out.Write('[{\"extension\":\".pdf\",\"mime_type\":\"application/pdf\"}]')",
        ]);
        let cancel = AtomicBool::new(false);
        let output = run_formats_probe(&mut command, Duration::from_secs(60), &cancel)
            .expect("正常探测应成功");
        assert!(String::from_utf8_lossy(&output.stdout).contains("pdf"));
    }

    // 覆盖 T-07/T-08：清单行按分组与媒体例外筛入支持集（纯函数回归）。
    #[test]
    fn parse_formats_filters_rows_by_group() {
        let stdout: &[u8] = concat!(
            r#"[{"extension":".pdf","mime_type":"application/pdf"},"#,
            r#"{"extension":".MP4","mime_type":"video/mp4"},"#,
            r#"{"extension":"wmv","mime_type":"video/x-ms-wmv"},"#,
            r#"{"extension":"","mime_type":"application/pdf"},"#,
            r#"{"extension":"tar.gz","mime_type":"application/x-tar"},"#,
            r#"{"extension":".docx","mime_type":"application/vnd.wordprocessing"}]"#
        )
        .as_bytes();
        let groups = vec![FormatGroup::Pdf, FormatGroup::Office, FormatGroup::Media];
        let selected = parse_formats(stdout, &groups).expect("合法清单应解析");
        let expected: BTreeSet<String> = ["pdf", "mp4", "docx"]
            .into_iter()
            .map(str::to_string)
            .collect();
        assert_eq!(selected, expected);
    }

    // ── F29：平铺占用名查询不随规模线性增长比较次数 ──

    // 覆盖 T-11（F29 性能修复，语义不变）：1 千/1 万互异目标的比较次数都近似为零，
    // 增长阶不再随占用名规模线性上升。
    #[test]
    fn occupied_index_compare_count_does_not_grow_with_size() {
        for size in [1_000usize, 10_000] {
            let mut index = OccupiedIndex::default();
            for i in 0..size {
                index.insert(OsString::from(format!("file{i}_pdf.md")));
            }
            let compares = {
                let mut compares = 0usize;
                let mut probe = |a: &OsStr, b: &OsStr| {
                    compares += 1;
                    compare_names(a, b) == Ordering::Equal
                };
                for i in 0..100 {
                    let name = OsString::from(format!("probe{i}_pdf.md"));
                    assert!(!index.contains_with(&name, &mut probe));
                }
                compares
            };
            assert_eq!(
                compares, 0,
                "互异目标应命中不同桶，规模 {size} 时不应发生逐一比较"
            );
        }
    }

    // 覆盖 T-11（F29）：大小写等价变体落入同桶，恰好一次精确比较即判重。
    #[test]
    fn occupied_index_case_variant_hits_single_bucket() {
        let mut index = OccupiedIndex::default();
        index.insert(OsString::from("Report_pdf.md"));
        let compares = {
            let mut compares = 0usize;
            let mut probe = |a: &OsStr, b: &OsStr| {
                compares += 1;
                compare_names(a, b) == Ordering::Equal
            };
            let duplicate = OsString::from("REPORT_pdf.md");
            assert!(index.contains_with(&duplicate, &mut probe));
            let distinct = OsString::from("Reports_pdf.md");
            assert!(!index.contains_with(&distinct, &mut probe));
            compares
        };
        assert_eq!(compares, 1, "大小写变体应在同桶内一次精确确认");
    }
}
