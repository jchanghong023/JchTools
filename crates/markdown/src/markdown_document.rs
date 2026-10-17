//! Xberg 文档到 Markdown 的本地 Rust 薄适配器（2026-10-04 零配置改造）。
//!
//! 这个模块只调用已初始化的 `xberg.exe`（经 XB-14 唯一共享代理），不启动 HTTP
//! 服务，也不依赖 Python。请求零配置：只传 `command` 与 `path`——页数自动分流
//! （`auto_fast_pages`，阈值内置于引擎）、嵌入文档合入与 Markdown 规范化全部
//! 由引擎完成。适配器只负责解析 `document.content`（引擎最终 Markdown）、
//! 执行 Xberg 仓 FORK.md 公布的唯一调用方落盘步骤（给正文图片引用加媒体目录
//! 前缀并登记待落盘字节）以及如实转达 `warnings`；媒体引用改写保留代码及
//! alt 字面文本，目录中的 Markdown 字符引用按 URL 编码。不重复实现引擎效果。

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;

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
fn parse_document_response(response: &Value, media_dir: &str) -> Result<DocumentOutput, String> {
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

/// T-14 媒体落盘（2026-10-04 引擎重编号口径）：引擎已把嵌入子文档图片围栏
/// 感知地重编号进宿主 `images`（不再使用 docNNN- 前缀），引用与字节一一对应；
/// 本函数执行 FORK.md 公布的唯一调用方步骤——把正文中的裸引用 `image_N.ext`
/// 改写为 `<媒体目录>/image_N.ext` 并登记待落盘字节。围栏与行内代码内的
/// `image_N.ext` 字样是字面文本，不改写也不据此落盘；正文未引用的图片不落盘
/// （与引擎引用↔字节对应口径一致）。字节缺失或解码失败时保留占位引用并告警。
fn collect_media(
    document: &Value,
    content: &str,
    media_dir: &str,
    warnings: &mut Vec<String>,
) -> (String, Vec<MediaFile>) {
    let Some(images) = document.get("images") else {
        return (content.to_owned(), Vec::new());
    };
    let Some(images) = images.as_array() else {
        tracing::warn!(
            event = "document_media_invalid",
            stage = "images_protocol",
            error_type = "invalid_array",
            "图片资源类型无效，保留占位引用"
        );
        warnings.push("图片资源字段类型无效，已保留占位引用".to_string());
        return (content.to_owned(), Vec::new());
    };
    // 元数据只建一次索引；正文扫描与图片数量无关，不为未引用项复制正文或解码。
    let mut entries = Vec::with_capacity(images.len());
    for image in images {
        let Some(index) = image.get("image_index").and_then(Value::as_u64) else {
            tracing::warn!(
                event = "document_media_invalid",
                stage = "image_index",
                error_type = "missing_index",
                "图片资源缺少索引，保留占位引用"
            );
            warnings.push("图片资源缺少 image_index，已保留占位引用".to_string());
            continue;
        };
        let Some(format) = image.get("format").and_then(Value::as_str) else {
            tracing::warn!(
                event = "document_media_invalid",
                stage = "image_format",
                image_index = index,
                error_type = "missing_format",
                "图片资源缺少格式，保留占位引用"
            );
            warnings.push(format!("图片 image_{index} 缺少格式，已保留占位引用"));
            continue;
        };
        // 扩展名只接受字母数字：它既进文件名也进引用，异常值宁可保占位不改写。
        if format.is_empty() || !format.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            tracing::warn!(
                event = "document_media_invalid",
                stage = "image_format",
                image_index = index,
                error_type = "invalid_format",
                "图片资源格式无效，保留占位引用"
            );
            warnings.push(format!("图片 image_{index} 的格式无效，已保留占位引用"));
            continue;
        }
        entries.push((format!("image_{index}.{format}"), image, index));
    }
    // 无有效图片时不解析正文；元数据校验产生的警告仍由调用方转达。
    if entries.is_empty() {
        return (content.to_owned(), Vec::new());
    }
    let mut image_index = std::collections::HashMap::with_capacity(entries.len());
    for (position, (source, _, _)) in entries.iter().enumerate() {
        // 重复元数据沿用首个分组；按原顺序尝试字节，首个成功项只落盘一次。
        image_index.entry(source.as_str()).or_insert(position);
    }
    let mut referenced = vec![false; entries.len()];
    let mut destinations = Vec::new();
    scan_image_links(content, &mut |range| {
        if let Some(&position) = image_index.get(&content[range.clone()]) {
            referenced[position] = true;
            destinations.push((range, position));
        }
    });
    let mut materialized = vec![false; entries.len()];
    let mut files = Vec::new();
    for (source, image, index) in &entries {
        let position = image_index[source.as_str()];
        if !referenced[position] || materialized[position] {
            continue;
        }
        match image_bytes(image) {
            Some(bytes) if !bytes.is_empty() => {
                materialized[position] = true;
                files.push(MediaFile {
                    relative: format!("{media_dir}/{source}"),
                    bytes,
                });
            }
            _ => {
                tracing::warn!(
                    event = "document_media_invalid",
                    stage = "image_data",
                    image_index = index,
                    error_type = "missing_or_invalid_data",
                    "图片资源数据无效，保留占位引用"
                );
                warnings.push(format!("图片 {source} 缺少有效数据，已保留占位引用"));
            }
        }
    }
    if files.is_empty() {
        return (content.to_owned(), files);
    }
    // 只拼接一次正文；插入目录前缀，其余字节（含尖括号、alt/title、CRLF）原样复制。
    let directory = encode_markdown_dir(media_dir);
    let extra = destinations
        .iter()
        .filter(|(_, position)| materialized[*position])
        .count()
        * (directory.len() + 1);
    let mut markdown = String::with_capacity(content.len() + extra);
    let mut copied = 0;
    for (range, position) in destinations {
        if !materialized[position] {
            continue;
        }
        markdown.push_str(&content[copied..range.start]);
        markdown.push_str(&directory);
        markdown.push('/');
        markdown.push_str(&content[range.clone()]);
        copied = range.end;
    }
    markdown.push_str(&content[copied..]);
    (markdown, files)
}

/// 图片字节：优先 `data_base64`（启动配置 `images.include_data_base64=true`
/// 时存在），缺失或解码失败时回退 `data` 的 u8 数字数组（`bytes` serde 在
/// JSON 中的形态）；两者皆无有效字节返回 `None`。
fn image_bytes(image: &Value) -> Option<Vec<u8>> {
    if let Some(data) = image.get("data_base64").and_then(Value::as_str) {
        if let Some(bytes) = decode_base64(data) {
            return Some(bytes);
        }
    }
    image
        .get("data")
        .and_then(Value::as_array)
        .and_then(|items| {
            items
                .iter()
                .map(|item| item.as_u64().and_then(|byte| u8::try_from(byte).ok()))
                .collect::<Option<Vec<u8>>>()
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

/// 沿既有围栏/行内代码/链接语义扫描真实图片目标；只记录区间，不重写正文。
/// alt text、普通链接的目标/title 及已带媒体目录的目标仍按原解析规则处理。
fn scan_image_links(text: &str, visit: &mut impl FnMut(std::ops::Range<usize>)) {
    let mut fence: Option<(char, usize)> = None;
    let mut containers = Vec::new();
    let mut inline_code: Option<usize> = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let remaining = &text[offset..];
        let line_start = offset;
        offset += line.len();
        let body = line.strip_suffix('\n').unwrap_or(line);
        let body = body.strip_suffix('\r').unwrap_or(body);
        // 只剥离当前容器实际占用的前缀；围栏的三空格限制仍相对于容器内容。
        // 空白列表行保留容器，退出列表/引用则同时结束其中未显式关闭的围栏。
        let (mut content_start, matched) = continue_containers(body, &containers);
        if matched != containers.len() {
            containers.truncate(matched);
            fence = None;
            inline_code = None;
        }
        if let Some((open_char, open_len)) = fence {
            if closes_fence(&body[content_start..], open_char, open_len) {
                fence = None;
            }
            continue;
        }
        while let Some((container, consumed)) = opening_container(&body[content_start..]) {
            containers.push(container);
            content_start += consumed;
        }
        if let Some((ch, run, _indent)) = opening_fence(&body[content_start..]) {
            inline_code = None;
            fence = Some((ch, run));
            continue;
        }
        scan_image_links_in_line(body, remaining, line_start, &mut inline_code, visit);
    }
}

/// 容器只参与围栏定位，不参与正文重建；列表宽度相对于上一层容器。
#[derive(Clone, Copy)]
enum MarkdownContainer {
    List(usize),
    Quote,
}

fn continue_containers(line: &str, containers: &[MarkdownContainer]) -> (usize, usize) {
    let mut offset = 0;
    for (matched, container) in containers.iter().enumerate() {
        let remaining = &line[offset..];
        let consumed = match container {
            MarkdownContainer::List(width) => {
                let spaces = leading_spaces(remaining);
                if spaces >= *width {
                    *width
                } else if remaining.trim().is_empty() {
                    // 列表中的空行不要求补齐内容缩进，也不会关闭代码围栏。
                    remaining.len()
                } else {
                    return (offset, matched);
                }
            }
            MarkdownContainer::Quote => {
                let Some(consumed) = quote_prefix(remaining) else {
                    return (offset, matched);
                };
                consumed
            }
        };
        offset += consumed;
    }
    (offset, containers.len())
}

fn quote_prefix(line: &str) -> Option<usize> {
    let indent = leading_spaces(line);
    if indent > 3 || line.as_bytes().get(indent) != Some(&b'>') {
        return None;
    }
    Some(indent + 1 + usize::from(line.as_bytes().get(indent + 1) == Some(&b' ')))
}

fn opening_container(line: &str) -> Option<(MarkdownContainer, usize)> {
    if let Some(consumed) = quote_prefix(line) {
        return Some((MarkdownContainer::Quote, consumed));
    }
    let indent = leading_spaces(line);
    if indent > 3 {
        return None;
    }
    let bytes = line.as_bytes();
    // 分隔线优先于列表，不能从 `- - -` / `***` 虚构后续围栏的容器。
    if let Some(marker @ (b'-' | b'*')) = bytes.get(indent) {
        let mut count = 0;
        let thematic = bytes[indent..].iter().all(|byte| {
            if byte == marker {
                count += 1;
                true
            } else {
                *byte == b' ' || *byte == b'\t'
            }
        });
        if thematic && count >= 3 {
            return None;
        }
    }
    let marker_end = match bytes.get(indent)? {
        b'-' | b'+' | b'*' => indent + 1,
        b'0'..=b'9' => {
            let digits = bytes[indent..]
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            if digits > 9 || !matches!(bytes.get(indent + digits), Some(b'.' | b')')) {
                return None;
            }
            indent + digits + 1
        }
        _ => return None,
    };
    let rest = &line[marker_end..];
    if !rest.is_empty() && !rest.starts_with(' ') {
        return None;
    }
    let padding = if rest.trim().is_empty() {
        1
    } else {
        leading_spaces(rest)
    };
    // CommonMark：标记后 1–4 空格才是内容前缀；更多空格只消费一个，
    // 避免把真正的四空格缩进代码误认成新围栏。空列表项仍保留下一行内容宽度。
    let width = marker_end
        + if (1..=4).contains(&padding) {
            padding
        } else {
            1
        };
    Some((MarkdownContainer::List(width), width.min(line.len())))
}

/// Encode the media directory for a CommonMark link target using Xberg's caller contract.
fn encode_markdown_dir(name: &str) -> String {
    let mut encoded = String::with_capacity(name.len());
    for character in name.chars() {
        let escaped = match character {
            ' ' => Some("%20"),
            '(' => Some("%28"),
            ')' => Some("%29"),
            '<' => Some("%3C"),
            '>' => Some("%3E"),
            '"' => Some("%22"),
            '`' => Some("%60"),
            '%' => Some("%25"),
            '#' => Some("%23"),
            '?' => Some("%3F"),
            '&' => Some("%26"),
            _ if character.is_control() => continue,
            _ => None,
        };
        if let Some(escaped) = escaped {
            encoded.push_str(escaped);
        } else {
            encoded.push(character);
        }
    }
    encoded
}

fn scan_image_links_in_line(
    line: &str,
    remaining: &str,
    offset: usize,
    inline_code: &mut Option<usize>,
    visit: &mut impl FnMut(std::ops::Range<usize>),
) {
    let mut i = 0;
    let bytes = line.as_bytes();
    while i < bytes.len() {
        // CommonMark 正文中的反斜杠转义只作用于 ASCII 标点；代码跨度内
        // 则是普通字符。成对消费也保留 \\ 后的真实图片/反引号语义。
        if inline_code.is_none()
            && bytes[i] == b'\\'
            && bytes.get(i + 1).is_some_and(u8::is_ascii_punctuation)
        {
            i += 2;
            continue;
        }
        if bytes[i] == b'`' {
            let run = bytes[i..].iter().take_while(|&&byte| byte == b'`').count();
            if let Some(open_len) = *inline_code {
                if run == open_len {
                    *inline_code = None;
                }
            } else {
                // 没有同长度关闭串时，反引号只是正文；不能吞掉真实图片。
                if closing_backticks(&remaining[i + run..], run).is_some() {
                    *inline_code = Some(run);
                }
            }
            i += run;
            continue;
        }
        if inline_code.is_none() && line[i..].starts_with("![") {
            let Some(close_alt) = image_label_end(line, i + 2) else {
                i += 1;
                continue;
            };
            if bytes.get(close_alt + 1) != Some(&b'(') {
                i += 1;
                continue;
            }
            let destination_start = close_alt + 2;
            let Some(close_link) = image_link_end(line, destination_start) else {
                i += 1;
                continue;
            };
            let destination = line[destination_start..close_link].trim_start();
            let leading = line[destination_start..close_link].len() - destination.len();
            let token_end = destination
                .char_indices()
                .find(|(_, character)| character.is_whitespace())
                .map_or(destination.len(), |(index, _)| index);
            let token = &destination[..token_end];
            let (token, bracketed) = match token.strip_prefix('<').and_then(|s| s.strip_suffix('>'))
            {
                Some(token) => (token, true),
                None => (token, false),
            };
            let token_start = offset + destination_start + leading + usize::from(bracketed);
            visit(token_start..token_start + token.len());
            // alt/title 的字面文本不参与引用扫描或行内代码状态。
            i = close_link + 1;
            continue;
        }
        if inline_code.is_none() && bytes[i] == b'[' {
            if let Some(close_label) = image_label_end(line, i + 1) {
                if bytes.get(close_label + 1) == Some(&b'(') {
                    if let Some(close_link) = ordinary_link_end(line, close_label + 2) {
                        // label 使用正文共享的代码状态；跨行代码优先于目标/title。
                        let label = &line[i + 1..close_label];
                        if label.contains("![") || label.contains('`') {
                            scan_image_links_in_line(
                                label,
                                &remaining[i + 1..],
                                offset + i + 1,
                                inline_code,
                                visit,
                            );
                        }
                        if inline_code.is_some() {
                            i = close_label;
                            continue;
                        }
                        // 只有合法普通链接的目标及 title 才不属于正文。
                        i = close_link + 1;
                        continue;
                    }
                }
            }
        }
        let Some(ch) = line[i..].chars().next() else {
            break;
        };
        i += ch.len_utf8();
    }
}

/// 平衡括号只确定候选边界；普通链接还须包含合法目标和可选 title，
/// 否则其中的真实图片仍是正文。括号 title 沿用图片解析的嵌套规则。
fn ordinary_link_end(line: &str, start: usize) -> Option<usize> {
    let end = image_link_end(line, start)?;
    let content = line[start..end].trim();
    let bytes = content.as_bytes();
    // 目标可为空；开头空白后的 title 仍须按字面量保留。
    let title_only = line.as_bytes()[start].is_ascii_whitespace()
        && bytes
            .first()
            .is_some_and(|&byte| matches!(byte, b'"' | b'\'' | b'('));
    let mut i = 0;
    let mut depth = 0usize;
    let angle = bytes.first() == Some(&b'<');
    if angle {
        i += 1;
    }
    while !title_only && i < bytes.len() {
        let byte = bytes[i];
        if byte == b'\\' && bytes.get(i + 1).is_some_and(u8::is_ascii_punctuation) {
            i += 2;
            continue;
        }
        if angle {
            if byte == b'>' {
                i += 1;
                break;
            }
            if byte == b'<' || byte == b'\r' || byte == b'\n' {
                return None;
            }
        } else {
            match byte {
                b'(' => depth += 1,
                b')' => depth = depth.checked_sub(1)?,
                byte if byte.is_ascii_whitespace() => break,
                byte if byte.is_ascii_control() || byte == b'<' || byte == b'>' => {
                    return None;
                }
                _ => {}
            }
        }
        i += 1;
    }
    if depth != 0 || (angle && bytes.get(i - 1) != Some(&b'>')) {
        return None;
    }
    if i == bytes.len() {
        return Some(end);
    }
    if !title_only && !bytes[i].is_ascii_whitespace() {
        return None;
    }
    while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    let Some(&delimiter) = bytes.get(i) else {
        return Some(end);
    };
    let title_end = match delimiter {
        b'(' => image_link_end(content, i + 1)?,
        b'"' | b'\'' => {
            i += 1;
            loop {
                let &byte = bytes.get(i)?;
                if byte == b'\\' && bytes.get(i + 1).is_some_and(u8::is_ascii_punctuation) {
                    i += 2;
                } else if byte == delimiter {
                    break i;
                } else {
                    i += 1;
                }
            }
        }
        _ => return None,
    };
    content[title_end + 1..].trim().is_empty().then_some(end)
}

/// 图片目标可含平衡括号，title 可含字面括号；不能在 title 内提前结束链接，
/// 否则其余 title 文本会被当作正文图片改写。反斜杠转义与尖括号目标保持原样。
fn image_link_end(line: &str, start: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut depth = 1usize;
    let mut quote = None;
    let mut angle = false;
    let mut i = start;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte == b'\\' && bytes.get(i + 1).is_some_and(u8::is_ascii_punctuation) {
            i += 2;
            continue;
        }
        if let Some(delimiter) = quote {
            if byte == delimiter {
                quote = None;
            }
        } else if angle {
            if byte == b'>' {
                angle = false;
            }
        } else {
            match byte {
                b'<' if line[start..i].trim().is_empty() => angle = true,
                b'"' | b'\'' if depth == 1 && i > start && bytes[i - 1].is_ascii_whitespace() => {
                    quote = Some(byte);
                }
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// 查找同长度的关闭反引号串；空段落或新围栏是代码跨度的块边界。
fn closing_backticks(text: &str, run: usize) -> Option<usize> {
    let mut offset = 0;
    for (line_index, line) in text.split_inclusive('\n').enumerate() {
        if line_index != 0 && (line.trim().is_empty() || opening_fence(line).is_some()) {
            return None;
        }
        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] != b'`' {
                i += 1;
                continue;
            }
            let length = bytes[i..].iter().take_while(|&&byte| byte == b'`').count();
            if length == run {
                return Some(offset + i);
            }
            i += length;
        }
        offset += line.len();
    }
    None
}

/// 图片 label 可含嵌套方括号、转义及代码，不能把 alt 内的 `](` 当作目标开头。
fn image_label_end(line: &str, start: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut depth = 1usize;
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if bytes.get(i + 1).is_some_and(u8::is_ascii_punctuation) => i += 2,
            b'`' => {
                let run = bytes[i..].iter().take_while(|&&byte| byte == b'`').count();
                i += run;
                if let Some(close) = closing_backticks(&line[i..], run) {
                    i += close + run;
                }
            }
            b'[' => {
                depth += 1;
                i += 1;
            }
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// 标准 base64（RFC 4648 字母表）解码，自足实现——不为单图解码引入新依赖。
/// 遇到非法字符返回 None（调用方保持占位原样）。
fn decode_base64(input: &str) -> Option<Vec<u8>> {
    fn value(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some(u32::from(byte - b'A')),
            b'a'..=b'z' => Some(u32::from(byte - b'a' + 26)),
            b'0'..=b'9' => Some(u32::from(byte - b'0' + 52)),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let input: Vec<u8> = input
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    if input.is_empty() || !input.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let (chunks, remainder) = input.as_chunks::<4>();
    if !remainder.is_empty() {
        return None;
    }
    for (chunk_index, chunk) in chunks.iter().enumerate() {
        let last = chunk_index + 1 == input.len() / 4;
        let a = value(chunk[0])?;
        let b = value(chunk[1])?;
        let c = if chunk[2] == b'=' {
            None
        } else {
            Some(value(chunk[2])?)
        };
        let d = if chunk[3] == b'=' {
            None
        } else {
            Some(value(chunk[3])?)
        };
        if (!last && (c.is_none() || d.is_none())) || (c.is_none() && d.is_some()) {
            return None;
        }
        if c.is_none() && (b & 0x0f) != 0 {
            return None;
        }
        if d.is_none() {
            if let Some(c) = c {
                if (c & 0x03) != 0 {
                    return None;
                }
            }
        }
        out.push(u8::try_from((a << 2) | (b >> 4)).ok()?);
        if let Some(c) = c {
            out.push(u8::try_from(((b & 0x0f) << 4) | (c >> 2)).ok()?);
            if let Some(d) = d {
                out.push(u8::try_from(((c & 0x03) << 6) | d).ok()?);
            }
        }
    }
    Some(out)
}

/// 识别围栏开启行：缩进 ≤3 个空格、行首连续 ≥3 个反引号或波浪线。
/// 开启行之后的其余内容按语言标签处理；反引号围栏的标签不能含反引号。
fn opening_fence(line: &str) -> Option<(char, usize, usize)> {
    let indent = leading_spaces(line);
    if indent > 3 {
        return None;
    }
    let body = &line[indent..];
    let ch = body.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let run = body
        .chars()
        .take_while(|&candidate| candidate == ch)
        .count();
    (run >= 3 && (ch != '`' || !body[run..].contains('`'))).then_some((ch, run, indent))
}

/// 判定关闭行：同字符、连续长度不小于开启行、其后只剩空白（语言标签不算关闭）。
fn closes_fence(line: &str, open_char: char, open_len: usize) -> bool {
    let indent = leading_spaces(line);
    if indent > 3 {
        return false;
    }
    let body = &line[indent..];
    let run = body
        .chars()
        .take_while(|&candidate| candidate == open_char)
        .count();
    // 围栏字符是单字节 ASCII，run 个字符对应 run 个字节的边界。
    run >= open_len && body[run..].trim().is_empty()
}

fn leading_spaces(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

#[rustfmt::skip]
#[warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn response(document: &Value) -> Value {
        json!({"id": 1, "ok": true, "document": document, "warnings": []})
    }

    // 覆盖 T-14/T-17：content 是引擎最终 Markdown，零改写（不追加换行、不拼
    // children、不再规范化）；唯一调用方步骤是媒体目录前缀。children 的并入
    // 由引擎负责，适配器忽略。
    #[test]
    fn content_is_final_and_children_are_ignored() {
        let envelope = response(&json!({
            "content": "# 报告\n\n![图](image_0.png)\n",
            "images": [
                {"image_index": 0, "format": "png", "data_base64": "aGVsbG8="}
            ],
            "children": [
                {"path": "word/embeddings/a.docx", "mime_type": "x",
                 "result": {"content": "子文档正文", "mime_type": "x", "metadata": {}, "tables": []}}
            ]
        }));
        let output = parse_document_response(&envelope, "a_docx_media").unwrap();
        assert_eq!(
            output.markdown,
            "# 报告\n\n![图](a_docx_media/image_0.png)\n"
        );
        assert_eq!(output.media.len(), 1);
        assert_eq!(output.media[0].relative, "a_docx_media/image_0.png");
        assert_eq!(output.media[0].bytes, b"hello".to_vec());
        assert!(output.warnings.is_empty());
    }

    // 覆盖 T-14：媒体目录名按 CommonMark 链接目标编码，跨行行内代码引用保持原样。
    #[test]
    fn media_prefix_is_encoded_and_multiline_inline_code_is_untouched() {
        let envelope = response(&json!({
            "content": "before `literal\n![代码](image_0.png)` after\n![图](image_0.png)",
            "images": [{"image_index": 0, "format": "png", "data_base64": "aGVsbG8="}]
        }));
        let output = parse_document_response(&envelope, "a_(1)%#").unwrap();
        assert_eq!(
            output.markdown,
            "before `literal\n![代码](image_0.png)` after\n![图](a_%281%29%25%23/image_0.png)"
        );
        assert_eq!(output.media.len(), 1);
        assert_eq!(output.media[0].relative, "a_(1)%#/image_0.png");
    }

    // 覆盖 T-14/T-17：转义图片标记和反引号都是正文字符，不产生图片或代码跨度。
    #[test]
    fn escaped_image_markers_remain_literal() {
        let envelope = response(&json!({
            "content": "\\![字面](image_0.png)\n\\` ![图](image_1.png)",
            "images": [
                {"image_index": 0, "format": "png", "data_base64": "aGVsbG8="},
                {"image_index": 1, "format": "png", "data_base64": "aGVsbG8="}
            ]
        }));
        let output = parse_document_response(&envelope, "m").unwrap();
        assert_eq!(
            output.markdown,
            "\\![字面](image_0.png)\n\\` ![图](m/image_1.png)"
        );
        assert_eq!(output.media.len(), 1);
        assert_eq!(output.media[0].relative, "m/image_1.png");
        assert!(output.warnings.is_empty());
    }

    // 覆盖 T-14：围栏与行内代码内的 image_N.ext 字样是字面文本，不改写；
    // 正文真实引用照常加前缀并登记落盘。
    #[test]
    fn fenced_reference_is_literal_but_real_reference_is_rewritten() {
        let envelope = response(&json!({
            "content": "正文 ![图](image_0.png)\n\n```text\nimage_0.png\n```\n",
            "images": [{"image_index": 0, "format": "png", "data_base64": "aGVsbG8="}]
        }));
        let output = parse_document_response(&envelope, "m").unwrap();
        assert!(output.markdown.contains("![图](m/image_0.png)"));
        assert!(output.markdown.contains("```text\nimage_0.png\n```"));
        assert_eq!(output.media.len(), 1);
    }

    // F28 / T-14/T-17：嵌套列表容器中的围栏可缩进 4 个及更多空格；
    // 空行不能把代码误判为正文，代码中的图片字面量零改写、零落盘。
    #[test]
    fn review_f28_nested_list_fences_preserve_literals_without_media() {
        for (list, indent) in [
            ("- 一级\n  - 二级\n", "    "),
            ("- 一级\n  - 二级\n    - 三级\n", "      "),
            ("- 一级\n  - 二级\n    - 三级\n      - 四级\n", "        "),
        ] {
            for fence in ["```", "~~~"] {
                for newline in ["\n", "\r\n"] {
                    let content = format!(
                        "{list}\n{indent}{fence}text\n{indent}![示例](image_0.png)\n\n{indent}![仍是代码](image_0.png)\n{indent}{fence}\n"
                    ).replace('\n', newline);
                    let envelope = response(&json!({
                        "content": content,
                        "images": [{"image_index": 0, "format": "png", "data": [0, 255, 37]}]
                    }));
                    let output = parse_document_response(&envelope, "nested_media").unwrap();
                    assert_eq!(
                        output.markdown.as_bytes(), content.as_bytes(),
                        "indent={}, fence={fence}, newline={newline:?}", indent.len()
                    );
                    assert!(output.media.is_empty(), "代码字面量不能登记待落盘图片");
                    assert!(output.warnings.is_empty());
                }
            }
        }
    }

    // F28 / T-14/T-17：同一索引在围栏中保持字面量、在正文中正常改写；
    // 独立代码图片不落盘，关闭围栏后的列表内及列表外真实图片不能被吞掉。
    #[test]
    fn review_f28_nested_list_fences_rewrite_only_real_images() {
        for (list, indent) in [
            ("- 一级\n  - 二级\n", "    "),
            ("- 一级\n  - 二级\n    - 三级\n", "      "),
        ] {
            for newline in ["\n", "\r\n"] {
                let content = format!(
                    "![前图](image_0.png)\n\n{list}\n{indent}```text\n{indent}![示例](image_0.png)\n\n{indent}![仅代码](image_1.png)\n{indent}```\n\n{indent}![列表图](image_2.png)\n\n![后图](image_0.png)\n"
                ).replace('\n', newline);
                let expected = format!(
                    "![前图](nested_media/image_0.png)\n\n{list}\n{indent}```text\n{indent}![示例](image_0.png)\n\n{indent}![仅代码](image_1.png)\n{indent}```\n\n{indent}![列表图](nested_media/image_2.png)\n\n![后图](nested_media/image_0.png)\n"
                ).replace('\n', newline);
                let envelope = response(&json!({
                    "content": content,
                    "images": [
                        {"image_index": 0, "format": "png", "data": [0, 255]},
                        {"image_index": 1, "format": "png", "data": [11, 22]},
                        {"image_index": 2, "format": "png", "data": [37, 42]}
                    ]
                }));
                let output = parse_document_response(&envelope, "nested_media").unwrap();
                assert_eq!(
                    output.markdown.as_bytes(), expected.as_bytes(),
                    "indent={}, newline={newline:?}", indent.len()
                );
                assert_eq!(output.media, vec![
                    MediaFile { relative: "nested_media/image_0.png".to_string(), bytes: vec![0, 255] },
                    MediaFile { relative: "nested_media/image_2.png".to_string(), bytes: vec![37, 42] },
                ]);
                assert!(output.warnings.is_empty());
            }
        }
    }

    // F28：有序列表、引用与混合容器只影响围栏定位，关闭或退出容器后恢复图片。
    #[test]
    fn review_f28_container_fences_keep_original_offsets_and_media() {
        for source in [
            "12. 项\n\n    ~~~\n    ![代码](image_0.png)\n\n    ~~~\n    ![真图](image_1.png)",
            "- > ```\n  > ![代码](image_0.png)\n  >\n  > ```\n  > ![真图](image_1.png)",
            "> - 项\n>\n>   ~~~\n>   ![代码](image_0.png)\n>\n>   ~~~\n>   ![真图](image_1.png)",
            "- 项\n     ~~~\n  ![代码](image_0.png)\n   ~~~\n  ![真图](image_1.png)",
            "- 项\n  ~~~\n  ![代码](image_0.png)\n![真图](image_1.png)",
        ] {
            for newline in ["\n", "\r\n"] {
                let content = source.replace('\n', newline);
                let expected = content.replace("![真图](image_1.png)", "![真图](m/image_1.png)");
                let envelope = response(&json!({
                    "content": content,
                    "images": [
                        {"image_index": 0, "format": "png", "data": [0, 255]},
                        {"image_index": 1, "format": "png", "data": [37, 128]}
                    ]
                }));
                let output = parse_document_response(&envelope, "m").unwrap();
                assert_eq!(output.markdown.as_bytes(), expected.as_bytes(), "{source:?}");
                assert_eq!(output.media, vec![MediaFile {
                    relative: "m/image_1.png".to_string(),
                    bytes: vec![37, 128],
                }]);
                assert!(output.warnings.is_empty());
            }
        }
    }

    // F28：不能全局取消围栏的三空格限制，也不能把分隔线识别成列表容器。
    #[test]
    fn review_f28_fence_indent_is_relative_to_real_containers_only() {
        for source in [
            "    ~~~\n\n![真图](image_0.png)\n",
            "- 项\n\n      ~~~\n\n  ![真图](image_0.png)\n",
            "- - -\n    ~~~\n\n![真图](image_0.png)\n",
            "***\n    ~~~\n\n![真图](image_0.png)\n",
            "- 项\n  - 子项\n\n列表外\n    ~~~\n\n![真图](image_0.png)\n",
        ] {
            let envelope = response(&json!({
                "content": source,
                "images": [{"image_index": 0, "format": "png", "data": [0, 255, 37]}]
            }));
            let output = parse_document_response(&envelope, "m").unwrap();
            assert_eq!(output.markdown.as_bytes(),
                source.replace("![真图](image_0.png)", "![真图](m/image_0.png)").as_bytes());
            assert_eq!(output.media, vec![MediaFile {
                relative: "m/image_0.png".to_string(),
                bytes: vec![0, 255, 37],
            }]);
            assert!(output.warnings.is_empty());
        }
    }

    // 覆盖 T-14：正文未引用的图片不落盘（引擎引用↔字节一一对应口径）。
    #[test]
    fn unreferenced_image_is_not_materialized() {
        let envelope = response(&json!({
            "content": "正文没有图片\n",
            "images": [{"image_index": 0, "format": "png", "data_base64": "aGVsbG8="}]
        }));
        let output = parse_document_response(&envelope, "m").unwrap();
        assert_eq!(output.markdown, "正文没有图片\n");
        assert!(output.media.is_empty());
        assert!(output.warnings.is_empty());
    }

    // 覆盖 T-16：被引用但字节缺失/损坏时保留占位引用并告警，不冒充成功。
    #[test]
    fn referenced_image_without_data_warns() {
        let envelope = response(&json!({
            "content": "![图](image_0.png)",
            "images": [{"image_index": 0, "format": "png"}]
        }));
        let output = parse_document_response(&envelope, "m").unwrap();
        assert_eq!(output.markdown, "![图](image_0.png)");
        assert!(output.media.is_empty());
        assert_eq!(
            output.warnings,
            vec!["图片 image_0.png 缺少有效数据，已保留占位引用"]
        );
    }

    // 覆盖 T-16：data_base64 缺失时回退 data 数字数组取字节。
    #[test]
    fn data_array_fallback_when_base64_absent() {
        let envelope = response(&json!({
            "content": "![图](image_0.png)",
            "images": [{"image_index": 0, "format": "png", "data": [104, 105]}]
        }));
        let output = parse_document_response(&envelope, "m").unwrap();
        assert_eq!(output.media.len(), 1);
        assert_eq!(output.media[0].bytes, b"hi".to_vec());
    }

    // 覆盖 T-18/T-24：引擎警告（auto_mode 自动降级）如实转达到任务界面；
    // exif 噪声按既有口径过滤，不改变其余文案。
    #[test]
    fn warnings_are_relayed_and_exif_noise_filtered() {
        let mut envelope = response(&json!({"content": "正文"}));
        envelope["warnings"] = json!([
            {"source": "exif", "message": "No EXIF data found"},
            {"source": "auto_mode",
             "message": "large document (520 pages > auto_fast_pages=500): OCR disabled for speed; set auto_fast_pages=0 to keep full quality"}
        ]);
        let output = parse_document_response(&envelope, "m").unwrap();
        assert_eq!(
            output.warnings,
            vec!["auto_mode：large document (520 pages > auto_fast_pages=500): OCR disabled for speed; set auto_fast_pages=0 to keep full quality".to_string()]
        );
    }

    // 覆盖 T-16/T-24：协议异常必须计失败，不得拼出空产物冒充成功——缺
    // document、document 非对象、content 非字符串都要明确报错。
    #[test]
    fn protocol_shape_errors_are_explicit() {
        let missing = json!({"id": 1, "ok": true, "warnings": []});
        let error = parse_document_response(&missing, "m").unwrap_err();
        assert!(error.contains("协议异常"), "{error}");
        assert!(error.contains("document"), "{error}");
        let not_object = response(&json!("content"));
        let error = parse_document_response(&not_object, "m").unwrap_err();
        assert!(error.contains("必须是对象"), "{error}");
        let bad_content = response(&json!({"content": 3}));
        let error = parse_document_response(&bad_content, "m").unwrap_err();
        assert!(error.contains("content"), "{error}");
    }

    // 覆盖 T-16：images 字段类型异常时告警并保留正文，不按协议失败。
    #[test]
    fn invalid_images_field_warns() {
        let envelope = response(&json!({"content": "正文", "images": 3}));
        let output = parse_document_response(&envelope, "m").unwrap();
        assert_eq!(output.markdown, "正文");
        assert!(output.media.is_empty());
        assert_eq!(
            output.warnings,
            vec!["图片资源字段类型无效，已保留占位引用"]
        );
    }

    // 覆盖 T-14/T-17：未闭合反引号是正文，不吞掉后面的真实图片；
    // 行内代码不能跨越空段落，alt 中的代码图片字面量也必须原样保留。
    #[test]
    fn unmatched_backticks_and_code_in_alt_do_not_hide_real_images() {
        for (content, expected) in [
            ("中文 `未闭合 ![图](image_0.png)",
             "中文 `未闭合 ![图](m/image_0.png)"),
            ("`未闭合\n\n![图](image_0.png)\n`",
             "`未闭合\n\n![图](m/image_0.png)\n`"),
            ("![说明 `![代码](image_0.png)`](image_1.png)",
             "![说明 `![代码](image_0.png)`](m/image_1.png)"),
        ] {
            let envelope = response(&json!({
                "content": content,
                "images": [
                    {"image_index": 0, "format": "png", "data_base64": "aGVsbG8="},
                    {"image_index": 1, "format": "png", "data_base64": "aGVsbG8="}
                ]
            }));
            let output = parse_document_response(&envelope, "m").unwrap();
            assert_eq!(output.markdown, expected);
            assert_eq!(output.media.len(), 1);
            assert!(output.warnings.is_empty());
        }
    }

    // 覆盖 T-14：合法 Windows 名称中的 &entity; 不能被阅读器当作字符引用，
    // Unicode 名称及多处引用保留，实际文件路径仍使用未经 URL 编码的名称。
    #[test]
    fn media_directory_ampersands_are_encoded() {
        let envelope = response(&json!({
            "content": "![甲](image_0.png) ![乙](image_0.png)",
            "images": [{"image_index": 0, "format": "png", "data_base64": "aGVsbG8="}]
        }));
        let output = parse_document_response(&envelope, "中文&copy;_media").unwrap();
        assert_eq!(output.markdown,
            "![甲](中文%26copy;_media/image_0.png) ![乙](中文%26copy;_media/image_0.png)");
        assert_eq!(output.media[0].relative, "中文&copy;_media/image_0.png");
        assert_eq!(output.media.len(), 1);
    }

    // 覆盖 T-14/T-16：严格标准 base64 的尾部位与 padding 边界，损坏时
    // data 数组回退仍保留；越界和非整数数组元素不能变成截断图片。
    #[test]
    fn image_byte_encoding_boundaries_are_explicit() {
        for (encoded, expected) in [
            ("AA==", vec![0]), ("AAA=", vec![0, 0]), ("AAAA", vec![0, 0, 0]),
            (" aG Vs\nbG8=\r", b"hello".to_vec()),
        ] {
            assert_eq!(decode_base64(encoded), Some(expected));
        }
        for encoded in ["", "A", "AA", "AAA", "AB==", "AAB=", "AA=A",
                        "=AAA", "AA==AAAA", "AAAA====", "中文", "AA-_"] {
            assert_eq!(decode_base64(encoded), None, "{encoded}");
        }
        assert_eq!(image_bytes(&json!({"data_base64": "AB==", "data": [0, 255]})),
                   Some(vec![0, 255]));
        for data in [json!([256]), json!([-1]), json!([1.5]), json!(["1"])] {
            assert_eq!(image_bytes(&json!({"data": data})), None);
        }
    }

    // 覆盖 T-14/T-17：反引号 info string 含反引号时不是围栏，
    // 真正围栏及其 Unicode info string、CRLF 和多反引号代码仍零改写。
    #[test]
    fn fence_and_code_delimiter_boundaries_preserve_literals() {
        for (content, expected) in [
            ("```语言`标签\n![图](image_0.png)",
             "```语言`标签\n![图](m/image_0.png)"),
            ("```中文\r\n![代码](image_0.png)\r\n```\r\n![图](image_0.png)",
             "```中文\r\n![代码](image_0.png)\r\n```\r\n![图](m/image_0.png)"),
            ("``说明 ` ![代码](image_0.png)`` ![图](image_0.png)",
             "``说明 ` ![代码](image_0.png)`` ![图](m/image_0.png)"),
        ] {
            let envelope = response(&json!({
                "content": content,
                "images": [{"image_index": 0, "format": "png", "data_base64": "aGVsbG8="}]
            }));
            let output = parse_document_response(&envelope, "m").unwrap();
            assert_eq!(output.markdown, expected);
            assert_eq!(output.media.len(), 1);
        }
    }

    // 覆盖 T-14/T-17：外部图片的 title 中含括号和图片字面量时，
    // 只改写其后的真实引用，不能截断 title 后把字面量当成图片。
    #[test]
    fn image_titles_with_parentheses_preserve_literal_references() {
        for content in [
            "![外图](external.png \"说明 ) ![字面](image_0.png)\") ![图](image_0.png)",
            "![外图](external_(1).png '说明 ) ![字面](image_0.png)') ![图](image_0.png)",
            "![外图](external.png (说明 ![字面](image_0.png))) ![图](image_0.png)",
        ] {
            let envelope = response(&json!({
                "content": content,
                "images": [{"image_index": 0, "format": "png", "data_base64": "aGVsbG8="}]
            }));
            let output = parse_document_response(&envelope, "m").unwrap();
            let (literal, _) = content.rsplit_once(" ![图]").unwrap();
            assert_eq!(output.markdown, format!("{literal} ![图](m/image_0.png)"));
            assert_eq!(output.media.len(), 1);
            assert!(output.warnings.is_empty());
        }
    }

    // 覆盖 T-14/T-17：普通链接的 title 是字面文本；label 内嵌图片
    // 仍是真实图片，不能为保护 title 而漏掉其媒体落盘。
    #[test]
    fn ordinary_link_titles_remain_literal_and_label_images_are_materialized() {
        let envelope = response(&json!({
            "content": "[外链](external.md \"说明 ![字面](image_0.png)\")\n[![图](image_0.png)](external.md '![字面](image_0.png)')",
            "images": [{"image_index": 0, "format": "png", "data_base64": "aGVsbG8="}]
        }));
        let output = parse_document_response(&envelope, "m").unwrap();
        assert_eq!(output.markdown,
            "[外链](external.md \"说明 ![字面](image_0.png)\")\n[![图](m/image_0.png)](external.md '![字面](image_0.png)')");
        assert_eq!(output.media.len(), 1);
        assert_eq!(output.media[0].relative, "m/image_0.png");
        assert_eq!(output.media[0].bytes, b"hello");
        assert!(output.warnings.is_empty());
    }

    // 覆盖 T-14：CommonMark 尖括号目标同样是正文图片引用；
    // 目录前缀只进入目标，尖括号、title 和其余正文保持原样。
    #[test]
    fn angle_bracket_image_destinations_are_materialized() {
        let envelope = response(&json!({
            "content": "![图](<image_0.png> \"说明 )\")",
            "images": [{"image_index": 0, "format": "png", "data_base64": "aGVsbG8="}]
        }));
        let output = parse_document_response(&envelope, "m").unwrap();
        assert_eq!(output.markdown, "![图](<m/image_0.png> \"说明 )\")");
        assert_eq!(output.media.len(), 1);
        assert_eq!(output.media[0].relative, "m/image_0.png");
        assert_eq!(output.media[0].bytes, b"hello");
        assert!(output.warnings.is_empty());
    }

    // 覆盖 T-14/T-17：貌似普通链接的 label 内开启跨行代码时，代码优先；
    // 字面图片既不能改写，也不能登记待落盘字节。
    #[test]
    fn review_fix_link_label_backtick_preserves_multiline_code_without_media() {
        for content in [
            "[说明 `字面](external.md)\n![代码](image_0.png)`",
            "[说明 `字面](external.md)\r\n![代码](image_0.png)`",
        ] {
            let envelope = response(&json!({
                "content": content,
                "images": [{"image_index": 0, "format": "png", "data_base64": "aGVsbG8="}]
            }));
            let output = parse_document_response(&envelope, "m").unwrap();
            assert_eq!(output.markdown.as_bytes(), content.as_bytes());
            assert_eq!(output.media, Vec::<MediaFile>::new());
            assert!(output.warnings.is_empty());
        }
    }

    // 覆盖 T-14/T-17：平衡括号不代表合法普通链接；非法目标后的真实图片
    // 必须按引擎最终正文位置改写，并登记与 image_index 匹配的字节。
    #[test]
    fn review_fix_invalid_outer_link_materializes_nested_image() {
        let envelope = response(&json!({
            "content": "[说明](external.md ![图](image_0.png))",
            "images": [{"image_index": 0, "format": "png", "data_base64": "aGVsbG8="}]
        }));
        let output = parse_document_response(&envelope, "m").unwrap();
        assert_eq!(output.markdown, "[说明](external.md ![图](m/image_0.png))");
        assert_eq!(output.media, vec![MediaFile {
            relative: "m/image_0.png".to_string(),
            bytes: b"hello".to_vec(),
        }]);
        assert!(output.warnings.is_empty());
    }

    // 覆盖 T-14/T-17：非法外层链接不能吞掉图片，但合法 title 中的图片仍是
    // 字面量；合法 label 图片独立落盘，避免同一索引掩盖漏图或多落盘。
    #[test]
    fn review_fix_invalid_link_keeps_valid_titles_literal_and_label_media_correct() {
        let content = concat!(
            "[说明](external.md ![图](image_0.png))\n",
            "[双引号](external.md \"说明 ![字面](image_1.png)\")\n",
            "[单引号](external.md '说明 ![字面](image_1.png)')\n",
            "[括号](external.md (说明 ![字面](image_1.png)))\n",
            "[![标签](image_2.png)](external.md \"![字面](image_1.png)\")"
        );
        let envelope = response(&json!({
            "content": content,
            "images": [
                {"image_index": 0, "format": "png", "data_base64": "aGVsbG8="},
                {"image_index": 1, "format": "png", "data": [222, 173]},
                {"image_index": 2, "format": "png", "data": [0, 255, 37]}
            ]
        }));
        let output = parse_document_response(&envelope, "m").unwrap();
        assert_eq!(output.markdown, concat!(
            "[说明](external.md ![图](m/image_0.png))\n",
            "[双引号](external.md \"说明 ![字面](image_1.png)\")\n",
            "[单引号](external.md '说明 ![字面](image_1.png)')\n",
            "[括号](external.md (说明 ![字面](image_1.png)))\n",
            "[![标签](m/image_2.png)](external.md \"![字面](image_1.png)\")"
        ));
        assert_eq!(output.media, vec![
            MediaFile { relative: "m/image_0.png".to_string(), bytes: b"hello".to_vec() },
            MediaFile { relative: "m/image_2.png".to_string(), bytes: vec![0, 255, 37] },
        ]);
        assert!(output.warnings.is_empty());
    }

    // P3-13 / T-14/T-16/T-17：多图批次仅改写真实且有字节的目标；
    // 元数据顺序与引用顺序不同、缺字节/坏字节不能影响其他图片或字面量。
    #[test]
    fn review_remediation_multiple_images_preserve_literals_placeholders_and_warnings() {
        let content = concat!(
            "# 多图\r\n",
            "![二](<image_2.png> \"说明 ) ![title](image_5.png)\")\r\n",
            "```中文\r\n![围栏](image_5.png)\r\n```\r\n",
            "`跨行\n![代码](image_5.png)`\n",
            "![说明 `![alt](image_5.png)`](image_0.png)\n",
            "[![一](image_1.png)](external.md '![title](image_5.png)')\n",
            "\\![转义](image_5.png)\n",
            "![缺失](image_3.png) ![损坏](image_4.png)\n",
            "![再二](image_2.png) ![再零](image_0.png)"
        );
        let envelope = response(&json!({
            "content": content,
            "images": [
                {"image_index": 3, "format": "png"},
                {"image_index": 2, "format": "png", "data": [0, 255]},
                {"image_index": 5, "format": "png", "data_base64": "not-base64"},
                {"image_index": 4, "format": "png", "data": [256]},
                {"image_index": 1, "format": "png", "data_base64": "aGVsbG8="},
                {"image_index": 0, "format": "png", "data": [37]}
            ]
        }));
        let output = parse_document_response(&envelope, "中文_(1)&").unwrap();
        assert_eq!(output.markdown, concat!(
            "# 多图\r\n",
            "![二](<中文_%281%29%26/image_2.png> \"说明 ) ![title](image_5.png)\")\r\n",
            "```中文\r\n![围栏](image_5.png)\r\n```\r\n",
            "`跨行\n![代码](image_5.png)`\n",
            "![说明 `![alt](image_5.png)`](中文_%281%29%26/image_0.png)\n",
            "[![一](中文_%281%29%26/image_1.png)](external.md '![title](image_5.png)')\n",
            "\\![转义](image_5.png)\n",
            "![缺失](image_3.png) ![损坏](image_4.png)\n",
            "![再二](中文_%281%29%26/image_2.png) ![再零](中文_%281%29%26/image_0.png)"
        ));
        assert_eq!(output.media, vec![
            MediaFile { relative: "中文_(1)&/image_2.png".to_string(), bytes: vec![0, 255] },
            MediaFile { relative: "中文_(1)&/image_1.png".to_string(), bytes: b"hello".to_vec() },
            MediaFile { relative: "中文_(1)&/image_0.png".to_string(), bytes: vec![37] },
        ]);
        assert_eq!(output.warnings, vec![
            "图片 image_3.png 缺少有效数据，已保留占位引用",
            "图片 image_4.png 缺少有效数据，已保留占位引用",
        ], "未引用的损坏 image_5 不解码、不落盘、也不额外告警");
    }

    // P3-13 / T-14/T-17：稀疏正文引用与大量未引用元数据一起消费解析器。
    // 只断言正文/媒体语义，不用固定秒数或实现扫描/复制次数冒充性能证明。
    #[test]
    fn review_remediation_many_images_keep_sparse_references_exact() {
        for image_count in [16_u64, 64, 256] {
            let mut content = "正文无需规范化，空格  与 Unicode 保持原样。\r\n".repeat(512);
            let mut expected = content.clone();
            let mut expected_media = Vec::new();
            let mut images = Vec::new();
            for index in 0..image_count {
                images.push(json!({
                    "image_index": index, "format": "png", "data": [index % 256, 255]
                }));
                if index % 7 == 0 {
                    content.push_str(&format!("![图{index}](image_{index}.png)\n"));
                    expected.push_str(&format!("![图{index}](m/image_{index}.png)\n"));
                    expected_media.push(MediaFile {
                        relative: format!("m/image_{index}.png"),
                        bytes: vec![u8::try_from(index % 256).unwrap(), 255],
                    });
                } else {
                    let literal = format!("`![代码{index}](image_{index}.png)`\n");
                    content.push_str(&literal);
                    expected.push_str(&literal);
                }
            }
            let envelope = response(&json!({"content": content, "images": images}));
            let output = parse_document_response(&envelope, "m").unwrap();
            assert_eq!(output.markdown, expected);
            assert_eq!(output.media, expected_media);
            assert!(output.warnings.is_empty());
        }
    }
}
