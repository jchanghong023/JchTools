//! Xberg 文档到 Markdown 的本地 Rust 薄适配器（2026-10-04 零配置改造）。
//!
//! 这个模块只调用已初始化的 `xberg.exe`（经 XB-14 唯一共享代理），不启动 HTTP
//! 服务，也不依赖 Python。请求零配置：只传 `command` 与 `path`——页数自动分流
//! （`auto_fast_pages`，阈值内置于引擎）、嵌入文档合入与 Markdown 规范化全部
//! 由引擎完成。适配器只负责解析 `document.content`（引擎最终 Markdown）、
//! 执行 Xberg 仓 FORK.md 公布的唯一调用方落盘步骤（给正文图片引用加媒体目录
//! 前缀并登记待落盘字节）以及如实转达 `warnings`；不重复实现引擎已承接的
//! 任何转换效果。

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
    if !path.is_file() {
        return Err(format!("输入文件不存在或不可读：{}", path.display()));
    }
    if !runtime_dir.is_dir() {
        return Err(format!("Xberg 运行目录不存在：{}", runtime_dir.display()));
    }
    let executable = runtime_dir.join(if cfg!(windows) { "xberg.exe" } else { "xberg" });
    if !executable.is_file() {
        return Err(format!("Xberg 可执行文件不存在：{}", executable.display()));
    }
    let response = crate::xberg_runtime::request(
        runtime_dir,
        serde_json::json!({"command": "extract", "path": path}),
        deadline.remaining(),
        &std::sync::atomic::AtomicBool::new(false),
    )
    .and_then(crate::xberg_runtime::checked)?;
    parse_document_response(&response, media_dir)
}

/// 解析 `extract` 成功响应（WORKER.md 2026-10-04 口径）：`document` 与
/// `xberg extract --format json` 的 `result` 字段同构（`ExtractedDocument`
/// 原样序列化），`content` 即引擎最终 Markdown——嵌入文档已由引擎并入
/// （`children` 仅供结构化消费者，适配器忽略）、已规范化；顶层 `warnings`
/// 恒在，是 `document.processing_warnings` 的副本。唯一调用方效果步骤是
/// [`collect_media`] 的媒体目录前缀。
fn parse_document_response(response: &Value, media_dir: &str) -> Result<DocumentOutput, String> {
    let Some(document) = response.get("document") else {
        return Err("Xberg 输出协议异常：成功响应缺少 document 字段".to_string());
    };
    if !document.is_object() {
        return Err(format!(
            "Xberg 输出协议异常：document 必须是对象，实际为 {}",
            value_type_name(document)
        ));
    }
    let Some(content) = document.get("content").and_then(Value::as_str) else {
        return Err("Xberg 输出协议异常：document.content 必须是字符串".to_string());
    };
    let mut markdown = content.to_owned();
    let mut warnings = Vec::new();
    let media = collect_media(document, &mut markdown, media_dir, &mut warnings);
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
    content: &mut String,
    media_dir: &str,
    warnings: &mut Vec<String>,
) -> Vec<MediaFile> {
    let Some(images) = document.get("images") else {
        return Vec::new();
    };
    let Some(images) = images.as_array() else {
        warnings.push("图片资源字段类型无效，已保留占位引用".to_string());
        return Vec::new();
    };
    let mut files = Vec::new();
    for image in images {
        let Some(index) = image.get("image_index").and_then(Value::as_u64) else {
            warnings.push("图片资源缺少 image_index，已保留占位引用".to_string());
            continue;
        };
        let Some(format) = image.get("format").and_then(Value::as_str) else {
            warnings.push(format!("图片 image_{index} 缺少格式，已保留占位引用"));
            continue;
        };
        // 扩展名只接受字母数字：它既进文件名也进引用，异常值宁可保占位不改写。
        if format.is_empty() || !format.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            warnings.push(format!("图片 image_{index} 的格式无效，已保留占位引用"));
            continue;
        }
        let source = format!("image_{index}.{format}");
        let target_ref = format!("{media_dir}/{source}");
        // 先试改写：围栏/行内代码内的字面量不算正文引用（不落盘、不改写）。
        let (rewritten, referenced) = rewrite_image_links(content, &source, &target_ref);
        if !referenced {
            continue;
        }
        match image_bytes(image) {
            Some(bytes) if !bytes.is_empty() => {
                *content = rewritten;
                files.push(MediaFile {
                    relative: target_ref,
                    bytes,
                });
            }
            _ => warnings.push(format!("图片 {source} 缺少有效数据，已保留占位引用")),
        }
    }
    files
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

/// 围栏和行内代码感知的图片链接目标改写（T-14/FORK.md 调用方目录前缀步骤）。
/// 把正文中 destination 等于 `from` 的图片链接改指 `to`，不碰普通文字、
/// alt text、行内代码或已改写媒体目录中的同名片段。
fn rewrite_image_links(text: &str, from: &str, to: &str) -> (String, bool) {
    let mut result = String::with_capacity(text.len());
    let mut fence: Option<(char, usize)> = None;
    let mut changed = false;
    for line in text.split_inclusive('\n') {
        let (body, newline) = match line.strip_suffix('\n') {
            Some(body) => (body, "\n"),
            None => (line, ""),
        };
        let (body, carriage) = match body.strip_suffix('\r') {
            Some(body) => (body, "\r"),
            None => (body, ""),
        };
        if let Some((open_char, open_len)) = fence {
            if closes_fence(body, open_char, open_len) {
                fence = None;
            }
            result.push_str(body);
            result.push_str(carriage);
            result.push_str(newline);
            continue;
        }
        if let Some((ch, run, _indent)) = opening_fence(body) {
            fence = Some((ch, run));
            result.push_str(body);
            result.push_str(carriage);
            result.push_str(newline);
            continue;
        }
        let (rewritten, line_changed) = rewrite_image_links_in_line(body, from, to);
        changed |= line_changed;
        result.push_str(&rewritten);
        result.push_str(carriage);
        result.push_str(newline);
    }
    (result, changed)
}

fn rewrite_image_links_in_line(line: &str, from: &str, to: &str) -> (String, bool) {
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    let mut inline_code: Option<usize> = None;
    let bytes = line.as_bytes();
    let mut changed = false;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let run = bytes[i..].iter().take_while(|&&byte| byte == b'`').count();
            if let Some(open_len) = inline_code {
                if run == open_len {
                    inline_code = None;
                }
            } else {
                inline_code = Some(run);
            }
            out.push_str(&line[i..i + run]);
            i += run;
            continue;
        }
        if inline_code.is_none() && line[i..].starts_with("![") {
            let Some(close_alt) = line[i + 2..].find("](") else {
                out.push('!');
                i += 1;
                continue;
            };
            let close_alt = i + 2 + close_alt;
            let destination_start = close_alt + 2;
            let Some(close_link_rel) = line[destination_start..].find(')') else {
                out.push('!');
                i += 1;
                continue;
            };
            let close_link = destination_start + close_link_rel;
            let destination = line[destination_start..close_link].trim_start();
            let leading = line[destination_start..close_link].len() - destination.len();
            let token_end = destination
                .char_indices()
                .find(|(_, character)| character.is_whitespace())
                .map_or(destination.len(), |(index, _)| index);
            let token = &destination[..token_end];
            if token == from {
                out.push_str(&line[i..destination_start + leading]);
                out.push_str(to);
                out.push_str(&line[destination_start + leading + token.len()..close_link]);
                out.push(')');
                i = close_link + 1;
                changed = true;
                continue;
            }
        }
        let Some(ch) = line[i..].chars().next() else {
            break;
        };
        out.push(ch);
        i += ch.len_utf8();
    }
    (out, changed)
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
/// 开启行之后的其余内容按语言标签处理，不参与关闭判定。
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
    (run >= 3).then_some((ch, run, indent))
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
}
