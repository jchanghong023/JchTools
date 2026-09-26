//! Xberg 文档到 Markdown 的本地 Rust 适配器。
//!
//! 这个模块只调用已初始化的 `xberg.exe`，不启动 HTTP 服务，也不依赖 Python。
//! 转换进程的标准输出是 `xberg extract --format json` 的 JSON 信封；适配器只取
//! `result.content`，并把 Xberg 返回的嵌入文档按旧工具的规则合并到结果中。

use std::collections::HashSet;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

const MAX_CHILD_DEPTH: usize = 5;
const DEFAULT_MAX_REQUEST_BODY_BYTES: &str = "104857600";

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
    timeout: Duration,
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

    let mut command = Command::new(&executable);
    command
        .arg("extract")
        .arg(path)
        .arg("--format")
        .arg("json")
        .arg("--no-config-discovery")
        .current_dir(runtime_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Xberg 是 GUI 后台任务；Windows 不应为它创建额外的控制台窗口。
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    apply_offline_environment(&mut command, runtime_dir);
    command.arg("--config-json").arg(derived_config_json(fast)?);

    let output = run_with_timeout(command, timeout)?;
    if !output.status.success() {
        return Err(format_process_failure(&output));
    }

    let value: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
        format!(
            "Xberg JSON 输出解析失败：{}；stdout={}；stderr={}",
            error,
            truncate_for_error(&output.stdout),
            truncate_for_error(&output.stderr)
        )
    })?;
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
/// The function reads the PDF page tree or ZIP central directory and a small
/// DOCX metadata part. It never renders a page, runs OCR, or opens a model.
/// Malformed, encrypted, or otherwise unsupported inputs return `None`, so the
/// caller uses normal conversion mode.
pub fn page_count(path: &Path) -> Option<usize> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "pdf" => {
            let document = lopdf::Document::load(path).ok()?;
            if document.is_encrypted() {
                return None;
            }
            let count = document.get_pages().len();
            (count > 0).then_some(count)
        }
        "docx" | "docm" | "dotx" | "dotm" => {
            let bytes = std::fs::read(path).ok()?;
            let entries = zip_entries(&bytes)?;
            let entry = entries
                .iter()
                .find(|entry| entry.name.eq_ignore_ascii_case("docProps/app.xml"))?;
            let app = zip_entry_bytes(&bytes, entry)?;
            parse_xml_number(&app, "Pages")
        }
        "pptx" | "pptm" | "ppsx" | "potx" | "potm" => {
            let bytes = std::fs::read(path).ok()?;
            let entries = zip_entries(&bytes)?;
            let count = entries
                .iter()
                .filter(|entry| {
                    let name = entry.name.to_ascii_lowercase();
                    name.starts_with("ppt/slides/slide")
                        && name
                            .rsplit_once('.')
                            .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("xml"))
                })
                .count();
            (count > 0).then_some(count)
        }
        _ => None,
    }
}

#[derive(Debug, Clone)]
struct ZipEntry {
    name: String,
    method: u16,
    crc32: u32,
    compressed_size: usize,
    uncompressed_size: usize,
    local_header_offset: usize,
}

fn zip_entries(bytes: &[u8]) -> Option<Vec<ZipEntry>> {
    let start = bytes.len().saturating_sub(65_557);
    let eocd = (start..bytes.len().saturating_sub(3))
        .rev()
        .find(|index| bytes.get(*index..*index + 4) == Some(b"PK\x05\x06"))?;
    let count = u16::from_le_bytes(bytes.get(eocd + 10..eocd + 12)?.try_into().ok()?) as usize;
    let size = u32::from_le_bytes(bytes.get(eocd + 12..eocd + 16)?.try_into().ok()?) as usize;
    let offset = u32::from_le_bytes(bytes.get(eocd + 16..eocd + 20)?.try_into().ok()?) as usize;
    // ZIP64 needs a proper ZIP parser; treating placeholder values as ordinary
    // offsets would silently produce an incorrect count.
    if count == 0xffff || size == 0xffff_ffff || offset == 0xffff_ffff {
        return None;
    }
    let end = offset.checked_add(size)?;
    if end > bytes.len() {
        return None;
    }
    let mut entries = Vec::with_capacity(count);
    let mut cursor = offset;
    while cursor + 46 <= end && entries.len() < count {
        if bytes.get(cursor..cursor + 4) != Some(b"PK\x01\x02") {
            return None;
        }
        let method = u16::from_le_bytes(bytes.get(cursor + 10..cursor + 12)?.try_into().ok()?);
        let crc32 = u32::from_le_bytes(bytes.get(cursor + 16..cursor + 20)?.try_into().ok()?);
        let compressed_size =
            u32::from_le_bytes(bytes.get(cursor + 20..cursor + 24)?.try_into().ok()?) as usize;
        let uncompressed_size =
            u32::from_le_bytes(bytes.get(cursor + 24..cursor + 28)?.try_into().ok()?) as usize;
        let name_len =
            u16::from_le_bytes(bytes.get(cursor + 28..cursor + 30)?.try_into().ok()?) as usize;
        let extra_len =
            u16::from_le_bytes(bytes.get(cursor + 30..cursor + 32)?.try_into().ok()?) as usize;
        let comment_len =
            u16::from_le_bytes(bytes.get(cursor + 32..cursor + 34)?.try_into().ok()?) as usize;
        let local_header_offset =
            u32::from_le_bytes(bytes.get(cursor + 42..cursor + 46)?.try_into().ok()?) as usize;
        let name_start = cursor + 46;
        let name_end = name_start.checked_add(name_len)?;
        let next = name_end.checked_add(extra_len)?.checked_add(comment_len)?;
        if next > end {
            return None;
        }
        let name = String::from_utf8_lossy(bytes.get(name_start..name_end)?).into_owned();
        entries.push(ZipEntry {
            name,
            method,
            crc32,
            compressed_size,
            uncompressed_size,
            local_header_offset,
        });
        cursor = next;
    }
    (entries.len() == count).then_some(entries)
}

fn zip_entry_bytes(bytes: &[u8], entry: &ZipEntry) -> Option<Vec<u8>> {
    let offset = entry.local_header_offset;
    if bytes.get(offset..offset + 4) != Some(b"PK\x03\x04") {
        return None;
    }
    let name_len =
        u16::from_le_bytes(bytes.get(offset + 26..offset + 28)?.try_into().ok()?) as usize;
    let extra_len =
        u16::from_le_bytes(bytes.get(offset + 28..offset + 30)?.try_into().ok()?) as usize;
    let data_start = offset
        .checked_add(30)?
        .checked_add(name_len)?
        .checked_add(extra_len)?;
    let data_end = data_start.checked_add(entry.compressed_size)?;
    let compressed = bytes.get(data_start..data_end)?;
    match entry.method {
        0 => {
            let output = compressed.to_vec();
            (output.len() == entry.uncompressed_size && crc32(&output) == entry.crc32)
                .then_some(output)
        }
        8 => {
            let mut decoder = flate2::read::DeflateDecoder::new(compressed);
            let mut output = Vec::with_capacity(entry.uncompressed_size);
            decoder.read_to_end(&mut output).ok()?;
            (output.len() == entry.uncompressed_size && crc32(&output) == entry.crc32)
                .then_some(output)
        }
        _ => None,
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

pub(crate) fn apply_offline_environment(command: &mut Command, runtime_dir: &Path) {
    let model_dir = runtime_dir.join("models");
    let cache_dir = runtime_dir.join("cache");
    command
        .env("HF_HOME", runtime_dir)
        .env("HF_HUB_CACHE", &model_dir)
        .env("XBERG_CACHE_DIR", &cache_dir)
        .env("HF_HUB_OFFLINE", "1")
        .env("HUGGINGFACE_HUB_OFFLINE", "1")
        .env("TRANSFORMERS_OFFLINE", "1")
        .env("HF_DATASETS_OFFLINE", "1")
        .env("XBERG_ORT_EP", "cpu")
        .env("XBERG_MAX_CONCURRENT_REQUESTS", "1")
        .env(
            "XBERG_MAX_REQUEST_BODY_BYTES",
            DEFAULT_MAX_REQUEST_BODY_BYTES,
        )
        .env("XBERG_API_ALLOW_LOCAL_URI_INPUTS", "1")
        .env("NO_COLOR", "1");

    let ort = runtime_dir.join(if cfg!(windows) {
        "onnxruntime.dll"
    } else {
        "libonnxruntime.so"
    });
    if ort.is_file() {
        command.env("ORT_DYLIB_PATH", ort);
    }
}

fn derived_config_json(fast: bool) -> Result<String, String> {
    let mut config: Value = serde_json::from_str(include_str!("../resources/markdown-xberg.json"))
        .map_err(|error| format!("内置 Xberg 配置无效：{error}"))?;
    if fast {
        config["layout"] = Value::Null;
        config["use_layout_for_markdown"] = Value::Bool(false);
        config["disable_ocr"] = Value::Bool(true);
        config["images"]["extract_images"] = Value::Bool(false);
        config["images"]["run_ocr_on_images"] = Value::Bool(false);
        config["pdf_options"]["extract_images"] = Value::Bool(false);
        config["pdf_options"]["ocr_inline_images"] = Value::Bool(false);
    }
    serde_json::to_string(&config).map_err(|error| format!("生成 Xberg 配置失败：{error}"))
}

struct ProcessOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn run_with_timeout(mut command: Command, timeout: Duration) -> Result<ProcessOutput, String> {
    let mut child = command
        .spawn()
        .map_err(|error| format!("启动 Xberg 失败：{error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Xberg stdout 管道创建失败".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Xberg stderr 管道创建失败".to_string())?;
    let stdout_thread = thread::spawn(move || read_pipe(stdout));
    let stderr_thread = thread::spawn(move || read_pipe(stderr));
    let started = Instant::now();

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(format!("Xberg 单文件转换超时（{} 秒）", timeout.as_secs()));
            }
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(format!("等待 Xberg 进程失败：{error}"));
            }
        }
    };

    let stdout = stdout_thread
        .join()
        .map_err(|_| "读取 Xberg stdout 的线程异常退出".to_string())??;
    let stderr = stderr_thread
        .join()
        .map_err(|_| "读取 Xberg stderr 的线程异常退出".to_string())??;
    Ok(ProcessOutput {
        status,
        stdout,
        stderr,
    })
}

fn read_pipe<R: Read>(mut reader: R) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .map_err(|error| format!("读取 Xberg 输出失败：{error}"))?;
    Ok(bytes)
}

fn format_process_failure(output: &ProcessOutput) -> String {
    let code = output
        .status
        .code()
        .map_or_else(|| "被系统终止".to_string(), |code| code.to_string());
    format!(
        "Xberg 转换失败（退出码 {}）：{}",
        code,
        truncate_for_error(&output.stderr)
    )
}

fn truncate_for_error(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes).trim().to_string();
    const LIMIT: usize = 4096;
    if text.chars().count() <= LIMIT {
        return text;
    }
    text.chars().take(LIMIT).collect::<String>() + "…"
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

fn build_final_markdown(envelope: &Value) -> Result<String, String> {
    let result = envelope
        .get("result")
        .ok_or_else(|| "Xberg JSON 缺少 result 字段".to_string())?;
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
    let mut fence: Option<&str> = None;
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if let Some(marker) = fence_marker(line) {
            fence = if fence == Some(marker) {
                None
            } else {
                Some(marker)
            };
            output.push(line.to_string());
            index += 1;
            continue;
        }
        if fence.is_some() || line.trim().is_empty() {
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

fn fence_marker(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("```") {
        Some("```")
    } else if trimmed.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
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
        assert_eq!(page_count(&path), Some(201));
    }

    // 覆盖 T-18：PDF 页树计数不依赖易误判的原始字节搜索。
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
        assert_eq!(page_count(&path), Some(201));
    }
}
