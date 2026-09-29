//! Xberg 文档到 Markdown 的本地 Rust 适配器。
//!
//! 这个模块只调用已初始化的 `xberg.exe`，不启动 HTTP 服务，也不依赖 Python。
//! 转换进程的标准输出是 `xberg extract --format json` 的 JSON 信封；适配器只取
//! `result.content`，并把 Xberg 返回的嵌入文档按旧工具的规则合并到结果中。

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;

const MAX_CHILD_DEPTH: usize = 5;
/// EOCD 搜索窗口：EOCD 固定 22 字节 + 注释最长 65535 字节（F18）。
const EOCD_WINDOW_BYTES: u64 = 65_557;
/// 单条目数据硬上限：docProps/app.xml 是小元数据文件，压缩或解压声明超过该值
/// 即按元数据损坏回退常规模式（T-18），不得按不可信元数据预留大额内存。
const MAX_ENTRY_DATA_BYTES: u64 = 8 * 1024 * 1024;

/// 单文件处理预算（T-29/F21）：进入该文件时一次性建立，页数预检与转换共用。
#[derive(Clone, Copy, Debug)]
pub struct Deadline {
    total: Duration,
    at: Instant,
}

impl Deadline {
    pub fn new(total: Duration) -> Self {
        Self {
            total,
            at: Instant::now() + total,
        }
    }

    /// 距预算耗尽的剩余时间；已耗尽时为零。
    pub fn remaining(&self) -> Duration {
        self.at.saturating_duration_since(Instant::now())
    }

    pub fn expired(&self) -> bool {
        Instant::now() >= self.at
    }

    /// 配置的预算总长（用于超时提示）。
    pub fn total(&self) -> Duration {
        self.total
    }
}

/// Markdown produced for one source file together with non-fatal Xberg warnings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentOutput {
    pub markdown: String,
    pub warnings: Vec<String>,
}

/// Run the fixed local Xberg CLI and return the final Markdown.
///
/// `runtime_dir` must contain `xberg.exe` and the optional native runtime files
/// (for example `onnxruntime.dll` and the model tree). The fixed extraction
/// configuration is embedded in the JchTools executable. The caller owns
/// directory scanning and output-file writes; this function only reads one
/// input file and returns Markdown plus non-fatal extraction warnings.
///
/// The bundled Xberg CLI is invoked with an explicit config and disabled config
/// discovery so an unrelated project or user config cannot alter conversion.
pub fn convert(
    path: &Path,
    runtime_dir: &Path,
    fast: bool,
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
        serde_json::json!({
            "command": "extract", "path": path, "mode": if fast { "fast" } else { "normal" }
        }),
        deadline.remaining(),
        &std::sync::atomic::AtomicBool::new(false),
    )?;
    let response = crate::xberg_runtime::checked(response)?;
    let value = serde_json::json!({"result": response["document"]});
    let mut document = build_document_output(&value)?;
    if fast {
        document.markdown = format!(
            "> 注意：大文档快速模式已启用，已关闭版面识别、图片提取和图片 OCR。\n\n{}",
            document.markdown
        );
    }
    Ok(document)
}

/// Return a cheap structural page/slide count for the large-document route.
///
/// The function reads the PDF page tree or a bounded window of the ZIP central
/// directory and the small DOCX metadata part. It never renders a page, runs
/// OCR, opens a model, or reads the whole container into memory. Malformed,
/// encrypted, lying-metadata, or otherwise unsupported inputs return `None`,
/// so the caller uses normal conversion mode; an exhausted [`Deadline`] also
/// returns `None` so the shared single-file budget governs both stages.
pub fn page_count(path: &Path, deadline: &Deadline) -> Option<usize> {
    if deadline.expired() {
        return None;
    }
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "pdf" => {
            if deadline.expired() {
                return None;
            }
            let document = lopdf::Document::load(path).ok()?;
            if document.is_encrypted() {
                return None;
            }
            let count = document.get_pages().len();
            (count > 0).then_some(count)
        }
        "docx" | "docm" | "dotx" | "dotm" => {
            let mut zip = ZipReader::open(path)?;
            let mut found = None;
            zip.scan_central_directory(|entry| {
                if entry.name.eq_ignore_ascii_case("docProps/app.xml") {
                    found = Some(entry.clone());
                }
            })?;
            let app = zip.entry_bytes(found.as_ref()?)?;
            parse_xml_number(&app, "Pages")
        }
        "pptx" | "pptm" | "ppsx" | "potx" | "potm" => {
            let mut zip = ZipReader::open(path)?;
            let mut count = 0usize;
            zip.scan_central_directory(|entry| {
                let name = entry.name.to_ascii_lowercase();
                if name.starts_with("ppt/slides/slide")
                    && name
                        .rsplit_once('.')
                        .is_some_and(|(_, extension)| extension == "xml")
                {
                    count += 1;
                }
            })?;
            (count > 0).then_some(count)
        }
        _ => None,
    }
}

#[derive(Debug, Clone)]
struct ZipEntry {
    name: String,
    flags: u16,
    method: u16,
    crc32: u32,
    compressed_size: u64,
    uncompressed_size: u64,
    local_header_offset: u64,
}

fn le_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// 有界 ZIP 结构读取（F18/T-18/附录 D）：只从文件尾部定位 EOCD 与中央目录，
/// 逐条目 seek+读，不把整个容器读入内存，也不按不可信元数据预留大额内存。
struct ZipReader {
    file: std::fs::File,
    file_len: u64,
}

impl ZipReader {
    fn open(path: &Path) -> Option<Self> {
        let file = std::fs::File::open(path).ok()?;
        let file_len = file.metadata().ok()?.len();
        Some(Self { file, file_len })
    }

    /// 从尾部有界窗口定位 EOCD，返回（条目数、中央目录起始、中央目录结束）。
    fn central_directory(&mut self) -> Option<(usize, u64, u64)> {
        let window_start = self.file_len.saturating_sub(EOCD_WINDOW_BYTES);
        let window_len = usize::try_from(self.file_len - window_start).ok()?;
        let mut window = vec![0u8; window_len];
        self.file.seek(SeekFrom::Start(window_start)).ok()?;
        self.file.read_exact(&mut window).ok()?;
        let eocd = (0..window.len().saturating_sub(3))
            .rev()
            .find(|index| window.get(*index..*index + 4) == Some(b"PK\x05\x06"))?;
        let count = usize::from(le_u16(window.get(eocd + 10..eocd + 12)?));
        let size = u64::from(le_u32(window.get(eocd + 12..eocd + 16)?));
        let offset = u64::from(le_u32(window.get(eocd + 16..eocd + 20)?));
        // ZIP64 needs a proper ZIP parser; treating placeholder values as ordinary
        // offsets would silently produce an incorrect count.
        if count == 0xffff || size == 0xffff_ffff || offset == 0xffff_ffff {
            return None;
        }
        let end = offset.checked_add(size)?;
        if end > self.file_len {
            return None;
        }
        Some((count, offset, end))
    }

    /// 流式扫描中央目录，逐条目回调；命中的条目由调用方保留，其余只 seek 跳过。
    /// 任何结构不一致返回 None（按损坏回退常规模式）。
    fn scan_central_directory(&mut self, mut visit: impl FnMut(&ZipEntry)) -> Option<()> {
        let (count, offset, end) = self.central_directory()?;
        let mut header = [0u8; 46];
        let mut cursor = offset;
        let mut seen = 0usize;
        while seen < count {
            if cursor.checked_add(46)? > end {
                return None;
            }
            self.file.seek(SeekFrom::Start(cursor)).ok()?;
            self.file.read_exact(&mut header).ok()?;
            if &header[0..4] != b"PK\x01\x02" {
                return None;
            }
            let entry = ZipEntry {
                name: String::new(),
                flags: le_u16(&header[8..10]),
                method: le_u16(&header[10..12]),
                crc32: le_u32(&header[16..20]),
                compressed_size: u64::from(le_u32(&header[20..24])),
                uncompressed_size: u64::from(le_u32(&header[24..28])),
                local_header_offset: u64::from(le_u32(&header[42..46])),
            };
            let name_len = usize::from(le_u16(&header[28..30]));
            let extra_len = u64::from(le_u16(&header[30..32]));
            let comment_len = u64::from(le_u16(&header[32..34]));
            let name_start = cursor.checked_add(46)?;
            let next = name_start
                .checked_add(name_len as u64)?
                .checked_add(extra_len)?
                .checked_add(comment_len)?;
            if next > end {
                return None;
            }
            let mut name_bytes = vec![0u8; name_len];
            self.file.read_exact(&mut name_bytes).ok()?;
            let entry = ZipEntry {
                name: String::from_utf8_lossy(&name_bytes).into_owned(),
                ..entry
            };
            visit(&entry);
            cursor = next;
            seen += 1;
        }
        Some(())
    }

    /// 只读取一个条目的数据：压缩与解压声明都先过硬上限，解压输出超限即放弃，
    /// 加密条目按不可信元数据回退。任何失败返回 None（回退常规模式）。
    fn entry_bytes(&mut self, entry: &ZipEntry) -> Option<Vec<u8>> {
        if entry.flags & 0x0001 != 0 {
            return None;
        }
        if entry.compressed_size > MAX_ENTRY_DATA_BYTES
            || entry.uncompressed_size > MAX_ENTRY_DATA_BYTES
        {
            return None;
        }
        let offset = entry.local_header_offset;
        if offset.checked_add(30)? > self.file_len {
            return None;
        }
        self.file.seek(SeekFrom::Start(offset)).ok()?;
        let mut header = [0u8; 30];
        self.file.read_exact(&mut header).ok()?;
        if &header[0..4] != b"PK\x03\x04" {
            return None;
        }
        let name_len = u64::from(le_u16(&header[26..28]));
        let extra_len = u64::from(le_u16(&header[28..30]));
        let data_start = offset
            .checked_add(30)?
            .checked_add(name_len)?
            .checked_add(extra_len)?;
        let data_end = data_start.checked_add(entry.compressed_size)?;
        if data_end > self.file_len {
            return None;
        }
        let mut compressed = vec![0u8; usize::try_from(entry.compressed_size).ok()?];
        self.file.seek(SeekFrom::Start(data_start)).ok()?;
        self.file.read_exact(&mut compressed).ok()?;
        match entry.method {
            0 => (compressed.len() as u64 == entry.uncompressed_size
                && crc32(&compressed) == entry.crc32)
                .then_some(compressed),
            8 => {
                let mut decoder = flate2::read::DeflateDecoder::new(&compressed[..]);
                // 不按不可信的 uncompressed_size 预留容量；扩张超过硬上限即回退。
                let cap = usize::try_from(MAX_ENTRY_DATA_BYTES).unwrap_or(usize::MAX);
                let mut output = Vec::new();
                let mut chunk = [0u8; 16 * 1024];
                loop {
                    let read = decoder.read(&mut chunk).ok()?;
                    if read == 0 {
                        break;
                    }
                    if output.len() + read > cap {
                        return None;
                    }
                    output.extend_from_slice(&chunk[..read]);
                }
                (output.len() as u64 == entry.uncompressed_size && crc32(&output) == entry.crc32)
                    .then_some(output)
            }
            _ => None,
        }
    }
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffff_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn parse_xml_number(bytes: &[u8], element: &str) -> Option<usize> {
    let text = String::from_utf8_lossy(bytes);
    let open = format!("<{element}>");
    let close = format!("</{element}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    text[start..end].trim().parse().ok()
}

#[cfg(test)]
fn derived_config_json(fast: bool) -> Result<String, String> {
    let mut config: Value = serde_json::from_str(include_str!("../resources/markdown-xberg.json"))
        .map_err(|error| format!("内置 Xberg 配置无效：{error}"))?;
    if fast {
        // A1：内置配置已无 layout 顶层键（Xberg 按字段名拒绝未知顶层字段），
        // 此处不得写回 layout:null——IndexMut 在键不存在时会插入该键。
        // use_layout_for_markdown=false 已完整表达 fast 模式的版面降级。
        config["use_layout_for_markdown"] = Value::Bool(false);
        config["disable_ocr"] = Value::Bool(true);
        config["images"]["extract_images"] = Value::Bool(false);
        config["images"]["run_ocr_on_images"] = Value::Bool(false);
        config["pdf_options"]["extract_images"] = Value::Bool(false);
        config["pdf_options"]["ocr_inline_images"] = Value::Bool(false);
    }
    serde_json::to_string(&config).map_err(|error| format!("生成 Xberg 配置失败：{error}"))
}

fn build_document_output(envelope: &Value) -> Result<DocumentOutput, String> {
    let markdown = build_final_markdown(envelope)?;
    let result = envelope
        .get("result")
        .ok_or_else(|| "Xberg JSON 缺少 result 字段".to_string())?;
    let mut warnings = Vec::new();
    collect_warnings(result, "主文档", &mut warnings);
    collect_depth_limit_warnings(result.get("children"), &mut warnings, "主文档", 0);
    Ok(DocumentOutput { markdown, warnings })
}

fn collect_warnings(document: &Value, context: &str, warnings: &mut Vec<String>) {
    if let Some(items) = document
        .get("processing_warnings")
        .and_then(Value::as_array)
    {
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
            warnings.push(format!("{context} · {source}：{message}"));
        }
    }

    let Some(children) = document.get("children").and_then(Value::as_array) else {
        return;
    };
    for child in children {
        let path = child
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("<未知嵌入文档>");
        let child_context = format!("{context} / {path}");
        if let Some(result) = child.get("result") {
            collect_warnings(result, &child_context, warnings);
        } else {
            warnings.push(format!("{child_context} · extraction：缺少子文档结果"));
        }
    }
}

fn collect_depth_limit_warnings(
    children: Option<&Value>,
    warnings: &mut Vec<String>,
    prefix: &str,
    depth: usize,
) {
    let Some(children) = children.and_then(Value::as_array) else {
        return;
    };
    for child in children {
        let path = child
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("<未知嵌入文档>");
        let display = format!("{prefix} / {path}");
        let Some(result) = child.get("result") else {
            continue;
        };
        if depth >= MAX_CHILD_DEPTH {
            let skipped = result
                .get("children")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            if skipped > 0 {
                warnings.push(format!(
                    "{display} · extraction：嵌入内容递归深度已达到上限（{MAX_CHILD_DEPTH} 层），已跳过 {skipped} 个子文档"
                ));
            }
            continue;
        }
        collect_depth_limit_warnings(result.get("children"), warnings, &display, depth + 1);
    }
}

/// 校验 Xberg 信封的 result 结构（F19/T-16/T-24）：协议异常必须计失败，
/// 不得把 null/标量/缺必要结构的空壳当成功拼出空 Markdown。结构完整而正文
/// 为空的合法文档（如 `{"content": ""}`）与只有结构化 OCR 的结果仍然成功。
fn validate_result_shape(result: &Value) -> Result<(), String> {
    let Some(fields) = result.as_object() else {
        return Err(format!(
            "Xberg 输出协议异常：result 必须是对象，实际为 {}",
            value_type_name(result)
        ));
    };
    if let Some(content) = fields.get("content") {
        if !content.is_string() {
            return Err(format!(
                "Xberg 输出协议异常：result.content 必须是字符串，实际为 {}",
                value_type_name(content)
            ));
        }
    }
    if let Some(elements) = fields.get("ocr_elements") {
        if !elements.is_array() {
            return Err(format!(
                "Xberg 输出协议异常：result.ocr_elements 必须是数组，实际为 {}",
                value_type_name(elements)
            ));
        }
    }
    if let Some(children) = fields.get("children") {
        if !children.is_array() {
            return Err(format!(
                "Xberg 输出协议异常：result.children 必须是数组，实际为 {}",
                value_type_name(children)
            ));
        }
    }
    let has_body = fields.contains_key("content")
        || fields.contains_key("ocr_elements")
        || fields.contains_key("children");
    if !has_body {
        return Err(
            "Xberg 输出协议异常：result 缺少 content/ocr_elements/children 必要结构".to_string(),
        );
    }
    Ok(())
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

fn build_final_markdown(envelope: &Value) -> Result<String, String> {
    let result = envelope
        .get("result")
        .ok_or_else(|| "Xberg JSON 缺少 result 字段".to_string())?;
    validate_result_shape(result)?;
    let mut parts = vec![render_document_root(result)];
    let mut seen = HashSet::new();
    if let Some(digest) = content_digest(parts[0].as_str()) {
        seen.insert(digest);
    }
    let mut children = Vec::new();
    collect_children(result.get("children"), &mut children, &mut seen, "", 0);
    for (display, content) in children {
        parts.push(String::new());
        parts.push(format!("## Embedded document: {display}"));
        parts.push(String::new());
        parts.push(content.trim_end_matches('\n').to_string());
    }
    let mut output = parts.join("\n");
    output = output.trim_end_matches('\n').to_string();
    output.push('\n');
    Ok(output)
}

fn render_document_root(document: &Value) -> String {
    let root = document
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let rendered = if root.trim().is_empty() {
        spatial_ocr_markdown(document).unwrap_or_default()
    } else {
        root.to_string()
    };
    normalize_markdown(&rendered)
}

fn collect_children(
    children: Option<&Value>,
    output: &mut Vec<(String, String)>,
    seen: &mut HashSet<String>,
    prefix: &str,
    depth: usize,
) {
    if depth > MAX_CHILD_DEPTH {
        return;
    }
    let Some(children) = children.and_then(Value::as_array) else {
        return;
    };
    for child in children {
        let Some(path) = child.get("path").and_then(Value::as_str) else {
            continue;
        };
        let Some(result) = child.get("result") else {
            continue;
        };
        let display = if prefix.is_empty() {
            path.to_string()
        } else {
            format!("{prefix}/{path}")
        };
        let content = render_document_root(result);
        if is_visio_page_part(&display) {
            if let Some(visio) = extract_visio_page_text(&content) {
                add_unique(output, seen, display.clone(), visio);
            }
        } else if is_ooxml_internal_part(&display) {
            collect_children(result.get("children"), output, seen, &display, depth + 1);
            continue;
        } else if is_raw_archive_dump(&content) {
            add_unique(
                output,
                seen,
                display.clone(),
                format!(
                    "Embedded archive content was not parsed by Xberg; raw archive listing omitted for {display}."
                ),
            );
        } else {
            add_unique(output, seen, display.clone(), content);
        }
        collect_children(result.get("children"), output, seen, &display, depth + 1);
    }
}

fn add_unique(
    output: &mut Vec<(String, String)>,
    seen: &mut HashSet<String>,
    display: String,
    content: String,
) {
    if let Some(digest) = content_digest(&content) {
        if seen.insert(digest) {
            output.push((display, content));
        }
    }
}

fn content_digest(content: &str) -> Option<String> {
    let normalized = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }
    // A deterministic, dependency-free digest is sufficient for duplicate child
    // suppression inside one response. It is not persisted or used as a security hash.
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in normalized.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    Some(format!("{hash:016x}"))
}

fn is_ooxml_internal_part(path: &str) -> bool {
    let normalized_path = path.replace('\\', "/");
    let parts = normalized_path
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.is_empty() || parts.contains(&"[Content_Types].xml") {
        return true;
    }
    let roots = [
        "word",
        "ppt",
        "xl",
        "visio",
        "_rels",
        "docProps",
        "customXml",
        "glossary",
    ];
    let Some(index) = parts.iter().rposition(|part| roots.contains(part)) else {
        return false;
    };
    let package = &parts[index..];
    if package
        .iter()
        .any(|part| *part == "embeddings" || *part == "attachments")
    {
        return false;
    }
    if package.len() >= 2
        && (package[1] == "media"
            || package[1] == "diagrams"
            || package[1] == "theme"
            || package[1] == "_rels")
    {
        return true;
    }
    let extension = package.last().and_then(|name| {
        name.rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase())
    });
    matches!(
        extension.as_deref(),
        Some("xml" | "rels" | "vml" | "bin" | "dll" | "ttf" | "odttf" | "css")
    )
}

fn is_visio_page_part(path: &str) -> bool {
    let normalized = path.replace('\\', "/").to_ascii_lowercase();
    normalized.contains("/visio/pages/page")
        && normalized
            .rsplit_once('.')
            .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("xml"))
}

fn extract_visio_page_text(content: &str) -> Option<String> {
    let mut in_text = false;
    let mut values = Vec::new();
    for raw in content.lines() {
        let line = raw.trim();
        if line == "#### Text" {
            in_text = true;
            continue;
        }
        if !in_text {
            continue;
        }
        if line.starts_with('#') && line.chars().take(4).all(|ch| ch == '#') {
            in_text = false;
            continue;
        }
        if !line.is_empty() && !line.starts_with("#####") && !line.starts_with("######") {
            values.push(line);
        }
    }
    (!values.is_empty()).then(|| format!("```text\n{}\n```", values.join("\n")))
}

fn is_raw_archive_dump(text: &str) -> bool {
    let mut lines = text.trim_start().lines();
    let Some(first) = lines.next() else {
        return false;
    };
    first.starts_with("ZIP Archive (")
        && lines
            .take(3)
            .any(|line| line.trim_start().starts_with("Files:"))
}

fn normalize_markdown(text: &str) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    let mut output = Vec::with_capacity(lines.len());
    // 围栏状态机（T-17/F20）：记录开启行的字符、长度与缩进；关闭行必须是同字符、
    // 长度不小于开启行且行内仅剩空白；不同字符的围栏互不切换。围栏内逐字原样，
    // 不经 normalize_inline（否则四反引号围栏内的三反引号行会误关围栏）。
    let mut fence: Option<(char, usize)> = None;
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if let Some((open_char, open_len)) = fence {
            if closes_fence(line, open_char, open_len) {
                fence = None;
            }
            output.push(line.to_string());
            index += 1;
            continue;
        }
        if let Some((ch, run, _indent)) = opening_fence(line) {
            fence = Some((ch, run));
            output.push(line.to_string());
            index += 1;
            continue;
        }
        if line.trim().is_empty() {
            output.push(line.to_string());
            index += 1;
            continue;
        }
        let drop_cap = if line.len() == 1 && line.as_bytes()[0].is_ascii_alphabetic() {
            lines.get(index + 1).is_some_and(|next| {
                next.chars()
                    .next()
                    .is_some_and(|ch| ch.is_ascii_lowercase())
            })
        } else {
            false
        };
        if drop_cap {
            output.push(normalize_inline(&format!("{}{}", line, lines[index + 1])));
            index += 2;
        } else {
            output.push(normalize_inline(line));
            index += 1;
        }
    }
    output.join("\n")
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

fn normalize_inline(line: &str) -> String {
    let mut output = String::new();
    let mut rest = line;
    loop {
        let Some(start) = rest.find('`') else {
            output.push_str(&decode_entities(rest));
            break;
        };
        let run = rest[start..].chars().take_while(|ch| *ch == '`').count();
        let delimiter = "`".repeat(run);
        let after_start = &rest[start + run..];
        let Some(end) = after_start.find(&delimiter) else {
            output.push_str(&decode_entities(rest));
            break;
        };
        output.push_str(&decode_entities(&rest[..start]));
        let close_end = end + run;
        output.push_str(&rest[start..start + run]);
        output.push_str(&after_start[..close_end]);
        rest = &after_start[close_end..];
    }
    output
}

fn decode_entities(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("&#") {
        output.push_str(&rest[..start]);
        let Some(end) = rest[start..].find(';') else {
            output.push_str(&rest[start..]);
            return output;
        };
        let entity = &rest[start + 2..start + end];
        let value = if entity.starts_with('x') || entity.starts_with('X') {
            let hex = &entity[1..];
            u32::from_str_radix(hex, 16).ok()
        } else {
            entity.parse::<u32>().ok()
        };
        if let Some(code) = value.filter(|code| *code <= 0x0010_ffff) {
            output.push(match code {
                9 => '\t',
                10 => '\n',
                160 => ' ',
                32..=0x0010_ffff => char::from_u32(code).unwrap_or(' '),
                _ => ' ',
            });
        } else {
            output.push_str(&rest[start..=start + end]);
        }
        rest = &rest[start + end + 1..];
    }
    output.push_str(rest);
    output.replace("&nbsp;", " ")
}

fn spatial_ocr_markdown(document: &Value) -> Option<String> {
    let elements = document.get("ocr_elements").and_then(Value::as_array)?;
    let mut lines = Vec::new();
    for element in elements {
        let text = element
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !text.is_empty() {
            lines.push(text.to_string());
        }
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parses_markdown_and_embedded_documents() {
        let envelope = serde_json::json!({
            "result": {
                "content": "A&#160;B\n",
                "children": [{
                    "path": "word/embeddings/nested.docx",
                    "result": {"content": "Child"}
                }, {
                    "path": "word/document.xml",
                    "result": {"content": "internal"}
                }]
            }
        });
        let rendered = build_final_markdown(&envelope).expect("render should succeed");
        assert!(rendered.contains("A B"));
        assert!(rendered.contains("## Embedded document: word/embeddings/nested.docx"));
        assert!(!rendered.contains("internal"));
    }

    #[test]
    fn preserves_code_fence_entities() {
        assert_eq!(
            normalize_markdown("x&#160;y\n```text\n&#160;\n```"),
            "x y\n```text\n&#160;\n```"
        );
    }

    #[test]
    fn joins_drop_cap_paragraph() {
        assert_eq!(normalize_markdown("D\nrops are text"), "Drops are text");
    }

    // 覆盖 T-16/T-24（F19）：result 存在但为 null 时是协议异常，必须计失败，
    // 不得把空壳信封当成功拼出空结果。
    #[test]
    fn result_null_is_protocol_failure() {
        let error = build_final_markdown(&serde_json::json!({"result": null}))
            .expect_err("result 为 null 必须计失败");
        assert!(error.contains("Xberg 输出协议异常"), "{error}");
    }

    // 覆盖 T-16/T-24（F19）：result 为标量同样是协议异常。
    #[test]
    fn result_scalar_is_protocol_failure() {
        let error = build_final_markdown(&serde_json::json!({"result": 42}))
            .expect_err("result 为标量必须计失败");
        assert!(error.contains("Xberg 输出协议异常"), "{error}");
    }

    // 覆盖 T-16/T-24（F19）：空对象缺失必要结构（content/ocr_elements/children
    // 一个都没有）时必须计失败，不得静默产出空 Markdown。
    #[test]
    fn result_empty_object_is_protocol_failure() {
        let error = build_final_markdown(&serde_json::json!({"result": {}}))
            .expect_err("空 result 必须计失败");
        assert!(error.contains("Xberg 输出协议异常"), "{error}");
    }

    // 覆盖 T-16/T-24（F19）：content 类型错误是协议异常，错误信息须指出字段。
    #[test]
    fn result_content_wrong_type_is_protocol_failure() {
        let error = build_final_markdown(&serde_json::json!({"result": {"content": ["a"]}}))
            .expect_err("content 类型错误必须计失败");
        assert!(error.contains("Xberg 输出协议异常"), "{error}");
        assert!(error.contains("content"), "{error}");
    }

    // 覆盖 T-25/T-16（F19）：结构完整而正文为空的合法空白文档仍是成功。
    #[test]
    fn legal_empty_document_still_succeeds() {
        let output = build_document_output(&serde_json::json!({"result": {"content": ""}}))
            .expect("结构完整的空白文档应成功");
        assert_eq!(output.markdown, "\n");
        assert!(output.warnings.is_empty());
    }

    // 覆盖 T-16/T-17（F19）：只有结构化 OCR 数据的合法结果成功，且保持行序。
    #[test]
    fn structured_ocr_only_result_succeeds() {
        let envelope = serde_json::json!({"result": {
            "ocr_elements": [{"text": "第一行"}, {"text": ""}, {"text": "第二行"}]
        }});
        let output = build_document_output(&envelope).expect("结构化 OCR 结果应成功");
        assert_eq!(output.markdown, "第一行\n第二行\n");
    }

    // 覆盖 T-17（F20）：四反引号围栏内的三反引号行是内容，不得关闭围栏；
    // 围栏内的实体保持原样，围栏外正常解码。
    #[test]
    fn longer_fence_keeps_shorter_marker_lines_verbatim() {
        let input = "前&#35;\n````text\n```\n&#35;围栏内\n````\n后&#35;";
        assert_eq!(
            normalize_markdown(input),
            "前#\n````text\n```\n&#35;围栏内\n````\n后#"
        );
    }

    // 覆盖 T-17（F20）：反引号围栏内的波浪线行是内容，不得切换或关闭围栏。
    #[test]
    fn tilde_lines_inside_backtick_fence_are_content() {
        assert_eq!(
            normalize_markdown("```\n~~~\n```\n&#35;"),
            "```\n~~~\n```\n#"
        );
    }

    // 覆盖 T-17（F20）：不同字符的围栏互不切换，波浪线围栏独立开闭。
    #[test]
    fn tilde_fence_opens_and_closes_independently() {
        let input = "~~~\n```text\n&#35;\n~~~~~~\n后&#35;";
        assert_eq!(
            normalize_markdown(input),
            "~~~\n```text\n&#35;\n~~~~~~\n后#"
        );
    }

    // 覆盖 T-17（F20）：4 空格缩进不是围栏（后续行正常转换）；≤3 空格缩进的围栏有效。
    #[test]
    fn indented_fences_follow_commonmark_indent_rules() {
        assert_eq!(
            normalize_markdown("   ```\n&#35;\n   ```"),
            "   ```\n&#35;\n   ```"
        );
        assert_eq!(normalize_markdown("    ```\n&#35;"), "    ```\n#");
    }

    // 覆盖 T-17（F20）：关闭行必须只含围栏字符与空白；带尾随文本的围栏字符行是内容。
    #[test]
    fn fence_close_requires_bare_marker_line() {
        assert_eq!(
            normalize_markdown("```\n``` tail\n&#35;\n```"),
            "```\n``` tail\n&#35;\n```"
        );
    }

    // 覆盖 T-17（F20）：行内代码保护与围栏外实体解码互不影响（守护既有行为）。
    #[test]
    fn inline_code_and_entities_untouched_by_fence_fix() {
        assert_eq!(normalize_markdown("x `&#35;` y &#35;"), "x `&#35;` y #");
    }

    #[test]
    fn rejects_missing_result() {
        let error = build_final_markdown(&serde_json::json!({})).expect_err("result is required");
        assert!(error.contains("result"));
    }

    // 覆盖 T-16：实质提取警告保留；图片缺少 EXIF 元数据不算正文缺失。
    #[test]
    fn collects_root_and_embedded_processing_warnings() {
        let envelope = serde_json::json!({
            "result": {
                "content": "root",
                "processing_warnings": [
                    {"source": "ocr", "message": "模型未命中"},
                    {"source": "exif", "message": "EXIF metadata extraction failed: failed to parse EXIF block: no exif data found in this file"}
                ],
                "children": [{
                    "path": "word/embeddings/chart.xlsx",
                    "result": {
                        "content": "child",
                        "processing_warnings": [
                            {"source": "table", "message": "表格不完整"}
                        ]
                    }
                }, {
                    "path": "word/embeddings/missing.docx"
                }]
            }
        });
        let output = build_document_output(&envelope).expect("output should parse");
        assert_eq!(output.warnings.len(), 3);
        assert!(output.warnings[0].contains("主文档 · ocr"));
        assert!(output.warnings[1].contains("chart.xlsx · table"));
        assert!(output.warnings[2].contains("missing.docx · extraction"));
    }

    // 覆盖 T-16：嵌入递归达到固定上限时说明跳过范围，不能伪报完整转换。
    #[test]
    fn warns_when_embedded_depth_limit_skips_children() {
        let mut limited = serde_json::json!({
            "content": "depth-limit",
            "children": [{
                "path": "skipped.docx",
                "result": {"content": "skipped"}
            }]
        });
        for index in (0..MAX_CHILD_DEPTH).rev() {
            limited = serde_json::json!({
                "content": format!("depth-{index}"),
                "children": [{
                    "path": format!("embedded-{index}"),
                    "result": limited
                }]
            });
        }
        let envelope = serde_json::json!({
            "result": {
                "content": "root",
                "children": [{"path": "embedded-root", "result": limited}]
            }
        });

        let output = build_document_output(&envelope).expect("output should parse");
        assert!(output
            .warnings
            .iter()
            .any(|warning| warning.contains("递归深度已达到上限")
                && warning.contains("跳过 1 个子文档")));
        assert!(!output.markdown.contains("Embedded document: skipped.docx"));
    }

    // 覆盖 T-18：仅读 ZIP 结构，超过 200 张幻灯片才启用快速模式。
    #[test]
    fn powerpoint_page_count_reads_structure_without_extraction() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("slides.pptx");
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for index in 1..=201 {
            zip.start_file(
                format!("ppt/slides/slide{index}.xml"),
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            zip.write_all(b"<p:sld/>").unwrap();
        }
        zip.finish().unwrap();
        assert_eq!(page_count(&path, &generous_deadline()), Some(201));
    }

    // 覆盖 T-18：PDF 页树计数不依赖易误判的原始字节搜索。
    // ── F18：页数预检只做有界结构读取，不得按不可信 ZIP 元数据预留大额内存 ──

    /// 手工构造最小单条目 ZIP，所有尺寸/标志字段按参数写入中央目录与本地头，
    /// 供测试伪造不可信元数据（谎报尺寸、截断流、加密标志）。
    fn build_minimal_zip(
        name: &str,
        method: u16,
        flags: u16,
        data: &[u8],
        declared_uncompressed: u32,
        declared_crc: u32,
    ) -> Vec<u8> {
        let crc_bytes = declared_crc.to_le_bytes();
        let csize_bytes = u32::try_from(data.len()).unwrap_or(u32::MAX).to_le_bytes();
        let usize_bytes = declared_uncompressed.to_le_bytes();
        let name_bytes = name.as_bytes();
        let mut out = Vec::new();
        // 本地文件头（30 字节）
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc_bytes);
        out.extend_from_slice(&csize_bytes);
        out.extend_from_slice(&usize_bytes);
        out.extend_from_slice(
            &u16::try_from(name_bytes.len())
                .unwrap_or(u16::MAX)
                .to_le_bytes(),
        );
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(data);
        // 中央目录条目（46 字节）
        out.extend_from_slice(b"PK\x01\x02");
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc_bytes);
        out.extend_from_slice(&csize_bytes);
        out.extend_from_slice(&usize_bytes);
        out.extend_from_slice(
            &u16::try_from(name_bytes.len())
                .unwrap_or(u16::MAX)
                .to_le_bytes(),
        );
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(name_bytes);
        // EOCD（22 字节）
        out.extend_from_slice(b"PK\x05\x06");
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(
            &u32::try_from(46 + name_bytes.len())
                .unwrap_or(u32::MAX)
                .to_le_bytes(),
        );
        out.extend_from_slice(
            &u32::try_from(30 + name_bytes.len() + data.len())
                .unwrap_or(u32::MAX)
                .to_le_bytes(),
        );
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    fn deflate_bytes(data: &[u8]) -> Vec<u8> {
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn generous_deadline() -> Deadline {
        Deadline::new(Duration::from_secs(30))
    }

    // 覆盖 T-18/T-24/T-29/附录 D（F18）：docProps/app.xml 谎报巨大 uncompressed_size
    // 时不得按元数据预留内存，按损坏回退常规模式（None）且须快速返回。
    #[test]
    fn docx_metadata_with_lying_uncompressed_size_falls_back() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("lying.docx");
        let data = b"<Properties><Pages>5</Pages></Properties>";
        let bytes = build_minimal_zip(
            "docProps/app.xml",
            8,
            0,
            &deflate_bytes(data),
            0xFFFF_FFFF,
            crc32(data),
        );
        std::fs::write(&path, bytes).unwrap();
        let started = Instant::now();
        assert_eq!(page_count(&path, &generous_deadline()), None);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "不得按 4GiB 谎报尺寸预留后慢慢失败"
        );
    }

    // 覆盖 T-18（F18）：app.xml 声明极大但压缩流实际截断 → 回退常规模式。
    #[test]
    fn docx_metadata_truncated_stream_falls_back() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("truncated.docx");
        let full = deflate_bytes(b"<Properties><Pages>5</Pages></Properties>");
        let bytes = build_minimal_zip(
            "docProps/app.xml",
            8,
            0,
            &full[..full.len() / 2],
            5_000_000,
            crc32(b"<Properties>"),
        );
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(page_count(&path, &generous_deadline()), None);
    }

    // 覆盖 T-18（F18）：解压扩张超过硬上限（8 MiB）→ 回退，不得缓冲全部输出。
    #[test]
    fn docx_metadata_deflate_expansion_is_capped() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bomb.docx");
        let payload = vec![0u8; 16 * 1024 * 1024];
        let compressed = deflate_bytes(&payload);
        // 声明 4 MiB（低于预检线）以触发解压循环中的硬上限检查
        let bytes = build_minimal_zip(
            "docProps/app.xml",
            8,
            0,
            &compressed,
            4 * 1024 * 1024,
            crc32(&payload),
        );
        std::fs::write(&path, bytes).unwrap();
        let started = Instant::now();
        assert_eq!(page_count(&path, &generous_deadline()), None);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    // 覆盖 T-18（F18）：损坏输入、伪 ZIP64 占位与加密条目一律回退常规模式。
    #[test]
    fn corrupt_zip64_and_encrypted_metadata_fall_back() {
        let temp = tempfile::tempdir().unwrap();
        let deadline = generous_deadline();
        let corrupt = temp.path().join("corrupt.docx");
        std::fs::write(&corrupt, b"not a zip at all").unwrap();
        assert_eq!(page_count(&corrupt, &deadline), None);

        let data = b"<Properties><Pages>5</Pages></Properties>";
        let mut bytes = build_minimal_zip(
            "docProps/app.xml",
            0,
            0,
            data,
            u32::try_from(data.len()).unwrap_or(u32::MAX),
            crc32(data),
        );
        let eocd = bytes.len() - 22;
        bytes[eocd + 10..eocd + 12].copy_from_slice(&0xFFFFu16.to_le_bytes());
        let zip64 = temp.path().join("zip64.docx");
        std::fs::write(&zip64, bytes).unwrap();
        assert_eq!(page_count(&zip64, &deadline), None);

        let encrypted = temp.path().join("encrypted.docx");
        std::fs::write(
            &encrypted,
            build_minimal_zip(
                "docProps/app.xml",
                0,
                0x0001,
                data,
                u32::try_from(data.len()).unwrap_or(u32::MAX),
                crc32(data),
            ),
        )
        .unwrap();
        assert_eq!(page_count(&encrypted, &deadline), None);
    }

    // 覆盖 T-18（F18）：正常 200/201 页文档页数正确；无关大条目不拖累元数据读取。
    #[test]
    fn docx_page_count_reads_only_metadata_entry() {
        let temp = tempfile::tempdir().unwrap();
        let deadline = generous_deadline();
        for pages in [200usize, 201] {
            let path = temp.path().join(format!("doc{pages}.docx"));
            let file = std::fs::File::create(&path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file("big.bin", options).unwrap();
            zip.write_all(&vec![0u8; 2 * 1024 * 1024]).unwrap();
            zip.start_file("docProps/app.xml", options).unwrap();
            zip.write_all(format!("<Properties><Pages>{pages}</Pages></Properties>").as_bytes())
                .unwrap();
            zip.finish().unwrap();
            assert_eq!(page_count(&path, &deadline), Some(pages));
        }
    }

    // 覆盖 T-29/T-18（F21）：页数预检共享单文件预算，预算耗尽时立即回退。
    #[test]
    fn page_count_returns_promptly_when_deadline_expired() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("slides.pptx");
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for index in 1..=201 {
            zip.start_file(
                format!("ppt/slides/slide{index}.xml"),
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            zip.write_all(b"<p:sld/>").unwrap();
        }
        zip.finish().unwrap();
        let deadline = Deadline::new(Duration::ZERO);
        let started = Instant::now();
        assert_eq!(page_count(&path, &deadline), None);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn pdf_page_count_reads_page_tree() {
        use lopdf::{dictionary, Object};

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("pages.pdf");
        let mut document = lopdf::Document::with_version("1.5");
        let pages_tree_id = document.new_object_id();
        let mut kids = Vec::new();
        for _ in 0..201 {
            let page_object_id = document.add_object(Object::Dictionary(dictionary! {
                "Type" => "Page",
                "Parent" => pages_tree_id,
                "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            }));
            kids.push(Object::Reference(page_object_id));
        }
        document.objects.insert(
            pages_tree_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => kids,
                "Count" => 201,
            }),
        );
        let catalog_id = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_tree_id,
        }));
        document.trailer.set("Root", catalog_id);
        document.save(&path).unwrap();
        assert_eq!(page_count(&path, &generous_deadline()), Some(201));
    }

    // ── A1：fast 模式配置不得写回已移除的 layout 顶层键 ──

    // 覆盖 T-18（A1）：Xberg run49.1 按字段名拒绝未知顶层字段，内置配置已无
    // layout 键；fast 分支若经 IndexMut 写回 layout:null，>200 页文档必然被
    // Xberg 拒绝（IndexMut 在键不存在时会插入，而不是无害的空操作）。
    #[test]
    fn fast_config_omits_layout_and_applies_fast_overrides() {
        let fast: Value = serde_json::from_str(&derived_config_json(true).unwrap()).unwrap();
        assert!(
            fast.get("layout").is_none(),
            "fast 配置不得写回 layout 顶层键：{fast}"
        );
        assert_eq!(fast["use_layout_for_markdown"], Value::Bool(false));
        assert_eq!(fast["disable_ocr"], Value::Bool(true));
        assert_eq!(fast["images"]["extract_images"], Value::Bool(false));
        assert_eq!(fast["pdf_options"]["extract_images"], Value::Bool(false));

        let full: Value = serde_json::from_str(&derived_config_json(false).unwrap()).unwrap();
        assert!(
            full.get("layout").is_none(),
            "常规配置同样不得引入 layout 键：{full}"
        );
        assert_eq!(full["use_layout_for_markdown"], Value::Bool(true));
    }
}
