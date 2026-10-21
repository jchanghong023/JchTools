use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::media::collect_media;

/// 单文件处理预算（T-29）：进入该文件时一次性建立，整个转换共用。
#[derive(Clone, Copy, Debug)]
pub struct Deadline {
    at: Option<Instant>,
}

impl Deadline {
    pub fn new(total: Duration) -> Self {
        let now = Instant::now();
        Self {
            // Extremely large user supplied values must not panic in `Instant` addition.
            // `None` is an unbounded deadline; normal UI values still use a precise instant.
            at: now.checked_add(total),
        }
    }

    /// 距预算耗尽的剩余时间；已耗尽时为零。
    pub fn remaining(&self) -> Duration {
        self.at.map_or(Duration::MAX, |at| {
            at.saturating_duration_since(Instant::now())
        })
    }
}

/// Markdown produced for one source file together with non-fatal Xberg warnings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentOutput {
    pub markdown: String,
    pub warnings: Vec<String>,
    /// T-14：随产物写入 `<产物名>_media/` 的图片本体（相对产物目录的路径 + 字节）。
    pub media: Vec<MediaFile>,
}

/// 一张待落盘图片：`relative` 为相对产物所在目录的路径（`<media 目录>/文件名`），
/// 分隔符统一 `/`；字节来自引擎 `images[]` 的 `data_base64`（缺失时回退
/// `data` 的 u8 数字数组）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaFile {
    pub relative: String,
    pub bytes: Vec<u8>,
}

/// Run the fixed local Xberg worker and return the final Markdown.
///
/// `runtime_dir` must contain `xberg.exe` and the optional native runtime files
/// (for example `onnxruntime.dll` and the model tree). The caller owns
/// directory scanning and output-file writes; this function only reads one
/// input file and returns the engine-final Markdown plus non-fatal warnings.
///
/// 零配置请求（WORKER.md 2026-10-04 口径）：除 `command`/`path` 外不传任何
/// 字段；页数自动分流、嵌入合入与规范化内置于引擎，调用方零改写。
pub fn convert(
    path: &Path,
    runtime_dir: &Path,
    media_dir: &str,
    deadline: &Deadline,
) -> Result<DocumentOutput, String> {
    convert_cancelable(
        path,
        runtime_dir,
        media_dir,
        deadline,
        &std::sync::atomic::AtomicBool::new(false),
    )
}

/// T-23/XB-17：按请求取消当前文件，不结束共享引擎或另一场景的任务。
pub fn convert_cancelable(
    path: &Path,
    runtime_dir: &Path,
    media_dir: &str,
    deadline: &Deadline,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<DocumentOutput, String> {
    let span = jchtools_foundation::logging::operation_span("markdown_document", "convert");
    let _entered = span.enter();
    let started = std::time::Instant::now();
    let mut stage = "input";
    tracing::debug!(
        event = "document_conversion_started",
        timeout_ms = u64::try_from(deadline.remaining().as_millis()).unwrap_or(u64::MAX),
        "文档转换开始"
    );
    let result = (|| {
        jchtools_assets::asset_util::ensure_not_cancelled(cancel)?;
        stage = "path_encoding";
        let request_path = path
            .to_str()
            .ok_or_else(|| "输入路径编码无效：Xberg 请求要求有效 UTF-8 路径".to_string())?;
        stage = "input_file";
        if !path.is_file() {
            return Err(format!("输入文件不存在或不可读：{}", path.display()));
        }
        stage = "runtime_directory";
        if !runtime_dir.is_dir() {
            return Err(format!("Xberg 运行目录不存在：{}", runtime_dir.display()));
        }
        let executable = runtime_dir.join(if cfg!(windows) { "xberg.exe" } else { "xberg" });
        stage = "runtime_executable";
        if !executable.is_file() {
            return Err(format!("Xberg 可执行文件不存在：{}", executable.display()));
        }
        stage = "extract_request_boundary";
        let response = jchtools_xberg::xberg_runtime::request(
            runtime_dir,
            serde_json::json!({"command": "extract", "path": request_path}),
            deadline.remaining(),
            cancel,
        )
        .and_then(jchtools_xberg::xberg_runtime::checked)?;
        stage = "response_protocol";
        jchtools_assets::asset_util::ensure_not_cancelled(cancel)?;
        parse_document_response(&response, media_dir)
    })();
    let elapsed_ms = jchtools_foundation::logging::elapsed_ms(started);
    if let Ok(document) = &result {
        tracing::debug!(
            event = "document_conversion_completed",
            elapsed_ms,
            markdown_bytes = document.markdown.len(),
            media_count = document.media.len(),
            warning_count = document.warnings.len(),
            "文档转换完成"
        );
    } else {
        tracing::error!(
            event = "document_conversion_failed",
            stage,
            elapsed_ms,
            cancel_requested = cancel.load(std::sync::atomic::Ordering::Acquire),
            error_type = if stage == "response_protocol" {
                "protocol"
            } else {
                "document_boundary"
            },
            "文档转换失败，详情由上游结构化事件诊断"
        );
    }
    result
}

/// 解析 `extract` 成功响应（WORKER.md 2026-10-04 口径）：`document` 与
/// `xberg extract --format json` 的 `result` 字段同构（`ExtractedDocument`
/// 原样序列化），`content` 即引擎最终 Markdown——嵌入文档已由引擎并入
/// （`children` 仅供结构化消费者，适配器忽略）、已规范化；顶层 `warnings`
/// 恒在，是 `document.processing_warnings` 的副本。唯一调用方效果步骤是
/// [`collect_media`] 的媒体目录前缀。
pub(super) fn parse_document_response(
    response: &Value,
    media_dir: &str,
) -> Result<DocumentOutput, String> {
    let Some(document) = response.get("document") else {
        tracing::error!(
            event = "document_protocol_failed",
            stage = "document_field",
            error_type = "protocol",
            error_code = "missing_document",
            "文档转换成功响应缺少document字段"
        );
        return Err("Xberg 输出协议异常：成功响应缺少 document 字段".to_string());
    };
    if !document.is_object() {
        tracing::error!(
            event = "document_protocol_failed",
            stage = "document_field",
            error_type = "protocol",
            error_code = "invalid_document_type",
            actual_type = value_type_name(document),
            "文档转换响应document字段类型无效"
        );
        return Err(format!(
            "Xberg 输出协议异常：document 必须是对象，实际为 {}",
            value_type_name(document)
        ));
    }
    let Some(content) = document.get("content").and_then(Value::as_str) else {
        tracing::error!(
            event = "document_protocol_failed",
            stage = "content_field",
            error_type = "protocol",
            error_code = "missing_or_invalid_content",
            "文档转换响应content字段缺失或类型无效"
        );
        return Err("Xberg 输出协议异常：document.content 必须是字符串".to_string());
    };
    let mut warnings = Vec::new();
    let (markdown, media) = collect_media(document, content, media_dir, &mut warnings);
    relay_warnings(response, &mut warnings);
    Ok(DocumentOutput {
        markdown,
        warnings,
        media,
    })
}

/// 顶层 `warnings`（恒在）如实转达引擎警告（XB-07/T-16/T-24 分类语义由引擎
/// 承担）；exif 噪声按既有口径过滤，不改变其余文案。
fn relay_warnings(response: &Value, warnings: &mut Vec<String>) {
    let Some(items) = response.get("warnings").and_then(Value::as_array) else {
        return;
    };
    for item in items {
        let source = item
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or("xberg");
        let message = item
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("未提供详细信息");
        if source.eq_ignore_ascii_case("exif")
            && message.to_ascii_lowercase().contains("no exif data found")
        {
            continue;
        }
        warnings.push(format!("{source}：{message}"));
    }
}

fn value_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "布尔值",
        Value::Number(_) => "数字",
        Value::String(_) => "字符串",
        Value::Array(_) => "数组",
        Value::Object(_) => "对象",
    }
}
