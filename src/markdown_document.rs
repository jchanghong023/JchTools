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

/// EOCD 搜索窗口：EOCD 固定 22 字节 + 注释最长 65535 字节（F18）。
const EOCD_WINDOW_BYTES: u64 = 65_557;
/// 单条目数据硬上限：docProps/app.xml 是小元数据文件，压缩或解压声明超过该值
/// 即按元数据损坏回退常规模式（T-18），不得按不可信元数据预留大额内存。
const MAX_ENTRY_DATA_BYTES: u64 = 8 * 1024 * 1024;

/// 单文件处理预算（T-29/F21）：进入该文件时一次性建立，页数预检与转换共用。
#[derive(Clone, Copy, Debug)]
pub struct Deadline {
    total: Duration,
    at: Option<Instant>,
}

impl Deadline {
    pub fn new(total: Duration) -> Self {
        let now = Instant::now();
        Self {
            total,
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

    pub fn expired(&self) -> bool {
        self.at.is_some_and(|at| Instant::now() >= at)
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
    /// T-14：随产物写入 `<产物名>_media/` 的图片本体（相对产物目录的路径 + 字节）。
    pub media: Vec<MediaFile>,
}

/// 一张待落盘图片：`relative` 为相对产物所在目录的路径（`<media 目录>/文件名`），
/// 分隔符统一 `/`；字节为上游 `images[].data_base64` 的解码结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaFile {
    pub relative: String,
    pub bytes: Vec<u8>,
}

/// T-14 逐层媒体装配器：目录名固定为调用方给定的 `<产物主干>_media`；
/// 主文档图片保持上游命名 `image_N.扩展名`，嵌入子文档加 `docNNN-` 前缀
/// 防止跨文档同名碰撞。正文未引用的图片不落盘（与上游 CLI 同口径）。
struct MediaCollector {
    dir: String,
    files: Vec<MediaFile>,
    warnings: Vec<String>,
    child_seq: u32,
}

impl MediaCollector {
    fn new(dir: &str) -> Self {
        Self {
            dir: dir.to_string(),
            files: Vec::new(),
            warnings: Vec::new(),
            child_seq: 0,
        }
    }

    fn next_child_tag(&mut self) -> String {
        self.child_seq += 1;
        format!("doc{:03}-", self.child_seq)
    }

    /// 试装配用的空白清单（同目录、不共享文件列表与序号）：
    /// 先往 probe 里装配，正文确定被采纳后再用 [`Self::adopt`] 并入，
    /// 判重丢弃的重复子文档就不会留下孤儿图片。
    fn probe(&self) -> Self {
        Self {
            dir: self.dir.clone(),
            files: Vec::new(),
            warnings: Vec::new(),
            child_seq: self.child_seq,
        }
    }

    fn adopt(&mut self, probe: Self) {
        self.files.extend(probe.files);
        self.warnings.extend(probe.warnings);
    }

    fn warning(&mut self, message: impl Into<String>) {
        self.warnings.push(message.into());
    }
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

    // T-18：已知超过 200 页时必须使用快速模式。引擎拒绝快速模式属于能力/配置
    // 错误，不能静默改用常规模式，否则 GUI 的分流日志与实际产物会不一致。
    let response = crate::xberg_runtime::request(
        runtime_dir,
        serde_json::json!({
            "command": "extract", "path": path, "mode": if fast { "fast" } else { "normal" }
        }),
        deadline.remaining(),
        &std::sync::atomic::AtomicBool::new(false),
    )
    .and_then(crate::xberg_runtime::checked)
    .map_err(|error| {
        if fast && error.contains("unsupported mode 'fast'") {
            format!("大文档快速模式不可用，未改用常规模式：{error}")
        } else {
            error
        }
    })?;
    let value = serde_json::json!({"result": response["document"]});
    let mut document = build_document_output_with_media(&value, media_dir, !fast)?;
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
        "pdf" => pdf_page_count(path, deadline),
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

/// PDF 页数的有界结构探测（T-18/附录 D：页数探测不得把整个文件读入内存，
/// 也不先执行 OCR）。只支持传统 xref 表 + 明文对象布局：尾部窗口找
/// `startxref` → 定位 xref 表 → 跳过子段行读取 trailer → `/Root` 间接引用
/// → catalog 的 `/Pages` → Pages 的 `/Count`。xref 流（PDF 1.5+ 压缩交叉
/// 引用）、对象流内引用、加密或间接 `/Count` 一律返回 `None`（调用方按
/// T-18 回常规模式，行为不劣于探测失败）；每个阶段检查单文件预算。
fn pdf_page_count(path: &Path, deadline: &Deadline) -> Option<usize> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len == 0 {
        return None;
    }
    // 尾部有界窗口定位 startxref（增量更新链的最后一个 xref 是最新 trailer）。
    let window_start = len.saturating_sub(64 * 1024);
    let window_len = usize::try_from(len - window_start).ok()?;
    let mut window = vec![0u8; window_len];
    file.seek(SeekFrom::Start(window_start)).ok()?;
    file.read_exact(&mut window).ok()?;
    let marker = find_last_subslice(&window, b"startxref")?;
    let after = &window[marker + b"startxref".len()..];
    let offset_line = after
        .split(|&byte| byte == b'\n' || byte == b'\r')
        .find(|line| !line.is_empty())?;
    let xref_offset = parse_leading_uint(offset_line)?;
    // xref 段必须是传统表（`xref` 关键字）；xref 流对象走 None 回常规模式。
    file.seek(SeekFrom::Start(xref_offset)).ok()?;
    if read_line_bounded(&mut file, 32)? != b"xref" {
        return None;
    }
    // 逐子段跳过固定 20 字节/行的表项，直到 `trailer`；表可能很大（百万对象），
    // 这里只按行数 seek 跳过，不把表读入内存。
    let trailer: Vec<u8> = loop {
        if deadline.expired() {
            return None;
        }
        let line = read_line_bounded(&mut file, 64)?;
        if line == b"trailer" {
            // trailer 字典到 startxref/EOF 结束；正常远小于窗口上限。
            let remaining = len.saturating_sub(file.stream_position().ok()?);
            let take = remaining.min(64 * 1024);
            let mut buffer = vec![0u8; usize::try_from(take).ok()?];
            file.read_exact(&mut buffer).ok()?;
            break buffer;
        }
        let text = String::from_utf8_lossy(&line).into_owned();
        let mut parts = text.split_whitespace();
        let (Some(_start), Some(count)) = (parts.next(), parts.next()) else {
            return None;
        };
        let count: u64 = count.parse().ok()?;
        file.seek(SeekFrom::Current(
            i64::try_from(count.checked_mul(20)?).ok()?,
        ))
        .ok()?;
    };
    if find_pdf_name(&trailer, b"/Encrypt").is_some() {
        return None;
    }
    let (root_num, _) = indirect_reference_after(&trailer, b"/Root")?;
    if deadline.expired() {
        return None;
    }
    // catalog 对象：按偏移读小窗口，找 /Pages 间接引用。
    let root_offset = xref_object_offset(&mut file, xref_offset, root_num, deadline)?;
    let catalog = read_object_window(&mut file, len, root_offset)?;
    let (pages_num, _) = indirect_reference_after(&catalog, b"/Pages")?;
    if deadline.expired() {
        return None;
    }
    let pages_offset = xref_object_offset(&mut file, xref_offset, pages_num, deadline)?;
    let pages = read_object_window(&mut file, len, pages_offset)?;
    let count_start = find_pdf_name(&pages, b"/Count")? + b"/Count".len();
    // 间接 `/Count N G R`（pdftk 等工具的产出形态）不受支持：只取第一个整数
    // 会把对象号当页数误报，必须识别出引用形态并回 None（T-18 回常规模式）。
    let rest = skip_space(&pages[count_start..]);
    if indirect_reference_after(&pages, b"/Count").is_some() {
        return None;
    }
    let count = parse_leading_uint(rest)?;
    let count = usize::try_from(count).ok()?;
    (count > 0).then_some(count)
}

/// 传统 xref 表中对象号 → 字节偏移：解析子段头与 20 字节行（`offset gen n|f`）。
/// 对象号不在表内或条目为 free（对象流/损坏/压缩对象）返回 None；子段遍历
/// 每轮检查单文件预算（构造的海量小子节文件不得越过 Deadline）。
fn xref_object_offset(
    file: &mut std::fs::File,
    xref_offset: u64,
    wanted: u64,
    deadline: &Deadline,
) -> Option<u64> {
    // trailer 内的 /Prev 链不去追：最新表找不到就按不支持处理（回常规模式）。
    file.seek(SeekFrom::Start(xref_offset + b"xref".len() as u64))
        .ok()?;
    loop {
        if deadline.expired() {
            return None;
        }
        let line = read_line_bounded(file, 64)?;
        // `xref` 关键字后紧跟的行尾会先读出一个空行，跳过。
        if line.is_empty() {
            continue;
        }
        if line == b"trailer" {
            return None;
        }
        let text = String::from_utf8_lossy(&line).into_owned();
        let mut parts = text.split_whitespace();
        let start: u64 = parts.next()?.parse().ok()?;
        let count: u64 = parts.next()?.parse().ok()?;
        if wanted >= start && wanted < start.checked_add(count)? {
            let skip = wanted - start;
            file.seek(SeekFrom::Current(
                i64::try_from(skip.checked_mul(20)?).ok()?,
            ))
            .ok()?;
            let entry = read_line_bounded(file, 20)?;
            let text = String::from_utf8_lossy(&entry).into_owned();
            // 条目固定三段：`offset generation n|f`——kind 是第三段。
            let mut fields = text.split_whitespace();
            let offset: u64 = fields.next()?.parse().ok()?;
            let _generation = fields.next()?;
            let kind = fields.next()?;
            if kind.starts_with('n') && offset > 0 {
                return Some(offset);
            }
            return None;
        }
        file.seek(SeekFrom::Current(
            i64::try_from(count.checked_mul(20)?).ok()?,
        ))
        .ok()?;
    }
}

/// 在对象偏移处读有界窗口（8 KiB）：跳过 `N G obj` 头，返回其后内容供键查找。
fn read_object_window(file: &mut std::fs::File, len: u64, offset: u64) -> Option<Vec<u8>> {
    if offset >= len {
        return None;
    }
    file.seek(SeekFrom::Start(offset)).ok()?;
    let head = read_line_bounded(file, 64)?;
    if !head.ends_with(b"obj") {
        return None;
    }
    let take = len
        .saturating_sub(file.stream_position().ok()?)
        .min(8 * 1024);
    let mut buffer = vec![0u8; usize::try_from(take).ok()?];
    file.read_exact(&mut buffer).ok()?;
    Some(buffer)
}

/// 读一行（到 \n 或 \r），含去 CR；空行返回空切片。超长或 EOF 返回 None。
fn read_line_bounded(file: &mut std::fs::File, max: usize) -> Option<Vec<u8>> {
    let mut line = Vec::with_capacity(32);
    let mut byte = [0u8; 1];
    while line.len() < max {
        match file.read(&mut byte) {
            Ok(1) => {}
            _ => return if line.is_empty() { None } else { Some(line) },
        }
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
    }
    while line.last() == Some(&b'\r') {
        line.pop();
    }
    Some(line)
}

fn find_last_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
}

/// Find a PDF name outside comments, literal strings, and hexadecimal strings.
/// This is intentionally small and bounded for the page probe; it prevents text such as
/// `(/Pages 2 0 R)` in a catalog metadata string from being mistaken for a dictionary key.
fn find_pdf_name(bytes: &[u8], needle: &[u8]) -> Option<usize> {
    let mut index = 0;
    while index + needle.len() <= bytes.len() {
        match bytes[index] {
            b'%' => {
                while index < bytes.len() && bytes[index] != b'\n' && bytes[index] != b'\r' {
                    index += 1;
                }
            }
            b'(' => {
                let mut depth = 1usize;
                index += 1;
                while index < bytes.len() && depth != 0 {
                    if bytes[index] == b'\\' {
                        index = index.saturating_add(2);
                    } else if bytes[index] == b'(' {
                        depth += 1;
                        index += 1;
                    } else if bytes[index] == b')' {
                        depth -= 1;
                        index += 1;
                    } else {
                        index += 1;
                    }
                }
            }
            b'<' if bytes.get(index + 1) == Some(&b'<') => {
                index += 2;
            }
            b'<' => {
                index += 1;
                while index < bytes.len() && bytes[index] != b'>' {
                    index += 1;
                }
                index = index.saturating_add(1);
            }
            _ if bytes[index..].starts_with(needle) => {
                let before = index.checked_sub(1).and_then(|i| bytes.get(i)).copied();
                let after = bytes.get(index + needle.len()).copied();
                let is_name_char = |byte: Option<u8>| {
                    byte.is_some_and(|byte| {
                        !byte.is_ascii_whitespace()
                            && !matches!(
                                byte,
                                b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
                            )
                    })
                };
                let slash_name = needle.first() == Some(&b'/');
                if (slash_name || !is_name_char(before)) && !is_name_char(after) {
                    return Some(index);
                }
                index += needle.len();
            }
            _ => index += 1,
        }
    }
    None
}

fn skip_space(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    &bytes[start..]
}

/// 解析开头的 ASCII 十进制数（跳过前导空白）。
fn parse_leading_uint(bytes: &[u8]) -> Option<u64> {
    let digits = skip_space(bytes);
    let end = digits
        .iter()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(digits.len());
    if end == 0 {
        return None;
    }
    std::str::from_utf8(&digits[..end]).ok()?.parse().ok()
}

/// 查找 `key` 后的间接引用 `N G R`，返回 (对象号, 代号)。
fn indirect_reference_after(bytes: &[u8], key: &[u8]) -> Option<(u64, u64)> {
    let start = find_pdf_name(bytes, key)? + key.len();
    let number = parse_leading_uint(&bytes[start..])?;
    let rest = skip_uint(&bytes[start..])?;
    let generation = parse_leading_uint(rest)?;
    let rest = skip_space(skip_uint(rest)?);
    (rest.first() == Some(&b'R')).then_some((number, generation))
}

/// 跳过开头的 ASCII 十进制数字段（先跳空白），返回其余部分；没有数字段返回 None。
fn skip_uint(bytes: &[u8]) -> Option<&[u8]> {
    let rest = skip_space(bytes);
    let end = rest
        .iter()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(rest.len());
    (end > 0).then(|| &rest[end..])
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
        config["images"]["include_data_base64"] = Value::Bool(false);
        config["pdf_options"]["extract_images"] = Value::Bool(false);
        config["pdf_options"]["ocr_inline_images"] = Value::Bool(false);
    }
    serde_json::to_string(&config).map_err(|error| format!("生成 Xberg 配置失败：{error}"))
}

#[cfg(test)]
fn build_document_output(envelope: &Value, media_dir: &str) -> Result<DocumentOutput, String> {
    build_document_output_with_media(envelope, media_dir, true)
}

fn build_document_output_with_media(
    envelope: &Value,
    media_dir: &str,
    include_media: bool,
) -> Result<DocumentOutput, String> {
    let mut media = MediaCollector::new(media_dir);
    let markdown = build_final_markdown_with_media(envelope, &mut media, include_media)?;
    let result = envelope
        .get("result")
        .ok_or_else(|| "Xberg JSON 缺少 result 字段".to_string())?;
    let mut warnings = Vec::new();
    collect_warnings(result, "主文档", &mut warnings);
    if markdown.trim().is_empty()
        && result
            .get("ocr_elements")
            .and_then(Value::as_array)
            .is_some()
    {
        warnings.push("主文档 · ocr：未识别到文字".to_string());
    }
    warnings.extend(media.warnings.iter().cloned());
    Ok(DocumentOutput {
        markdown,
        warnings,
        media: media.files,
    })
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
    // 字段名 → 中文类型名 → 类型判定。新增协议字段时在表中加一行即可。
    type FieldTypeCheck = (&'static str, &'static str, fn(&Value) -> bool);
    const EXPECTED_TYPES: [FieldTypeCheck; 3] = [
        ("content", "字符串", Value::is_string),
        ("ocr_elements", "数组", Value::is_array),
        ("children", "数组", Value::is_array),
    ];
    for (field, expected, is_type) in EXPECTED_TYPES {
        if let Some(value) = fields.get(field) {
            if !is_type(value) {
                return Err(format!(
                    "Xberg 输出协议异常：result.{field} 必须是{expected}，实际为 {}",
                    value_type_name(value)
                ));
            }
        }
    }
    let has_body = EXPECTED_TYPES
        .iter()
        .any(|(field, _, _)| fields.contains_key(*field));
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

#[cfg(test)]
fn build_final_markdown(envelope: &Value, media: &mut MediaCollector) -> Result<String, String> {
    build_final_markdown_with_media(envelope, media, true)
}

fn build_final_markdown_with_media(
    envelope: &Value,
    media: &mut MediaCollector,
    include_media: bool,
) -> Result<String, String> {
    let result = envelope
        .get("result")
        .ok_or_else(|| "Xberg JSON 缺少 result 字段".to_string())?;
    validate_result_shape(result)?;
    let mut root = render_document_root(result);
    if include_media {
        attach_media(result, &mut root, media, None);
    } else {
        root = strip_image_links(&root);
    }
    let mut parts = vec![root];
    let mut seen = HashSet::new();
    if let Some(digest) = content_digest(parts[0].as_str()) {
        seen.insert(digest);
    }
    let mut children = Vec::new();
    // T-17（2026-10-04 收缩为引擎原生优先）：root 最终正文作为包含性判重基准
    // 传入，引擎已并入宿主正文的嵌入对象不再重复分节。
    collect_children(
        result.get("children"),
        &mut children,
        &mut seen,
        "",
        parts[0].as_str(),
        media,
        include_media,
    );
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
    root_content: &str,
    media: &mut MediaCollector,
    include_media: bool,
) {
    let Some(children) = children.and_then(Value::as_array) else {
        return;
    };
    for child in children {
        let Some(path) = child.get("path").and_then(Value::as_str) else {
            media.warning("嵌入文档结果缺少 path，已跳过该子文档");
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
        if let Err(error) = validate_result_shape(result) {
            media.warning(format!("嵌入文档 {display} · extraction：{error}"));
            continue;
        }
        let content = render_document_root(result);
        let content = if include_media {
            content
        } else {
            strip_image_links(&content)
        };
        if is_raw_archive_dump(&content) {
            add_unique(
                output,
                seen,
                display.clone(),
                format!(
                    "Embedded archive content was not parsed; raw archive listing omitted for {display}."
                ),
                None,
            );
        } else if is_contained_in_root(root_content, &content) {
            // T-17（2026-10-04 收缩为引擎原生优先）：引擎已把该嵌入对象并入宿主
            // 正文，不再重复分节，也不登记媒体；其子层级同随引擎原生正文，不再
            // 递归展开。
            continue;
        } else {
            let tag = media.next_child_tag();
            let dedup_content = content.clone();
            let mut landed = content;
            // 试装配:正文判重被丢弃的重复子文档不登记媒体(无孤儿文件)。
            let mut probe = media.probe();
            if include_media {
                attach_media(result, &mut landed, &mut probe, Some(&tag));
            }
            if add_unique(output, seen, display.clone(), landed, Some(&dedup_content)) {
                media.adopt(probe);
            } else {
                media.warnings.extend(probe.warnings);
            }
        }
        collect_children(
            result.get("children"),
            output,
            seen,
            &display,
            root_content,
            media,
            include_media,
        );
    }
}

fn add_unique(
    output: &mut Vec<(String, String)>,
    seen: &mut HashSet<String>,
    display: String,
    content: String,
    digest_source: Option<&str>,
) -> bool {
    if let Some(digest) = content_digest(digest_source.unwrap_or(&content)) {
        if seen.insert(digest) {
            output.push((display, content));
            return true;
        }
    }
    false
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

/// T-17（2026-10-04 收缩为引擎原生优先）：两侧正文各做「连续空白折叠为单空格」
/// 的归一后，判定宿主正文是否已包含子文档正文——包含即引擎已原生并入，不再
/// 重复分节。该归一只用于判重，不改写产物正文。
fn is_contained_in_root(root_content: &str, child_content: &str) -> bool {
    let root_normalized = root_content
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let child_normalized = child_content
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    !child_normalized.is_empty() && root_normalized.contains(&child_normalized)
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
        output.push(normalize_inline(line));
        index += 1;
    }
    output.join("\n")
}

/// T-14：把本层 `images` 的图片字节登记进待落盘清单，并把该层正文中的
/// `image_N.<扩展名>` 引用改写为指向 `<media 目录>` 的相对路径。与上游同口径：
/// 主文档保持 `image_N.扩展名`，嵌入子文档用 `child_tag` 加前缀防碰撞；正文
/// 未引用的图片不落盘；围栏内字面 `image_N.ext`（清单、OCR 误像）不重写；
/// 字节缺失或损坏时保持占位引用原样（降级如实，不改写）。
fn attach_media(
    document: &Value,
    content: &mut String,
    media: &mut MediaCollector,
    child_tag: Option<&str>,
) {
    let Some(images_value) = document.get("images") else {
        return;
    };
    let Some(images) = images_value.as_array() else {
        media.warning("图片资源字段类型无效，已保留占位引用");
        return;
    };
    for image in images {
        let Some(index) = image.get("image_index").and_then(Value::as_u64) else {
            media.warning("图片资源缺少 image_index，已保留占位引用");
            continue;
        };
        let Some(format) = image.get("format").and_then(Value::as_str) else {
            media.warning(format!("图片 image_{index} 缺少格式，已保留占位引用"));
            continue;
        };
        // 扩展名只接受字母数字：它既进文件名也进引用，异常值宁可保占位不改写。
        if format.is_empty() || !format.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            media.warning(format!("图片 image_{index} 的格式无效，已保留占位引用"));
            continue;
        }
        let source = format!("image_{index}.{format}");
        let file_name = match child_tag {
            Some(tag) => format!("{tag}{source}"),
            None => source.clone(),
        };
        let target_ref = format!("{}/{file_name}", media.dir);
        let Some(data) = image.get("data_base64").and_then(Value::as_str) else {
            if rewrite_image_links(content, Some(&source), None).1 {
                media.warning(format!("图片 {source} 缺少数据，已保留占位引用"));
            }
            continue;
        };
        let Some(bytes) = decode_base64(data) else {
            if rewrite_image_links(content, Some(&source), None).1 {
                media.warning(format!("图片 {source} 的 Base64 数据损坏，已保留占位引用"));
            }
            continue;
        };
        let (rewritten, referenced) =
            rewrite_image_links(content, Some(&source), Some(&target_ref));
        if !referenced {
            // 正文未引用该图片：不落盘（上游 CLI 对未引用图片同样不入产物）。
            continue;
        }
        *content = rewritten;
        media.files.push(MediaFile {
            relative: target_ref,
            bytes,
        });
    }
}

/// 围栏和行内代码感知的图片链接目标改写（T-14）。只处理 Markdown 图片链接的
/// destination，不碰普通文字、alt text、行内代码或已改写媒体目录中的同名片段。
/// `from=None` 用于快速模式移除所有 `image_N.ext` 图片链接并保留 alt 文本。
fn rewrite_image_links(text: &str, from: Option<&str>, to: Option<&str>) -> (String, bool) {
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

fn rewrite_image_links_in_line(line: &str, from: Option<&str>, to: Option<&str>) -> (String, bool) {
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
            let matches = match from {
                Some(from) => token == from,
                None => token.starts_with("image_") && token.contains('.'),
            };
            if matches {
                if let Some(to) = to {
                    out.push_str(&line[i..destination_start + leading]);
                    out.push_str(to);
                    out.push_str(&line[destination_start + leading + token.len()..close_link]);
                    out.push(')');
                } else {
                    out.push_str(&line[i + 2..close_alt]);
                }
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

fn strip_image_links(text: &str) -> String {
    rewrite_image_links(text, None, None).0
}

/// 标准 base64（RFC 4648 字母表）解码，自足实现与本模块 crc32 同风格——
/// 不为单图解码引入新依赖。遇到非法字符返回 None（调用方保持占位原样）。
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

fn normalize_inline(line: &str) -> String {
    let mut output = String::new();
    let mut rest = line;
    loop {
        let Some(start) = rest.find('`') else {
            output.push_str(&decode_entities(rest));
            break;
        };
        let run = rest[start..].chars().take_while(|ch| *ch == '`').count();
        let after_start = &rest[start + run..];
        let Some((end, close_len)) = find_inline_code_close(after_start, run) else {
            output.push_str(&decode_entities(rest));
            break;
        };
        output.push_str(&decode_entities(&rest[..start]));
        let close_end = end + close_len;
        output.push_str(&rest[start..start + run]);
        output.push_str(&after_start[..close_end]);
        rest = &after_start[close_end..];
    }
    output
}

fn find_inline_code_close(text: &str, open_len: usize) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'`' {
            index += text[index..].chars().next()?.len_utf8();
            continue;
        }
        let run = bytes[index..]
            .iter()
            .take_while(|&&byte| byte == b'`')
            .count();
        if run == open_len {
            return Some((index, run));
        }
        index += run;
    }
    None
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
            let character = match code {
                9 => '\t',
                10 => '\n',
                160 => ' ',
                32..=0x0010_ffff => char::from_u32(code).unwrap_or(' '),
                _ => ' ',
            };
            let at_line_start = output.is_empty() || output.ends_with('\n');
            let next = rest[start + end + 1..].chars().next();
            let escape_marker = match character {
                '#' | '-' | '+' => at_line_start && next.is_some_and(char::is_whitespace),
                '>' => at_line_start,
                '*' | '_' => {
                    at_line_start || output.chars().last().is_some_and(char::is_whitespace)
                }
                _ => false,
            };
            if escape_marker {
                output.push('\\');
            }
            output.push(character);
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
    #[derive(Clone)]
    struct OcrItem {
        text: String,
        page: u64,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        order: usize,
    }

    fn level(value: &Value) -> &str {
        value.get("level").and_then(Value::as_str).unwrap_or("")
    }
    fn geometry(value: &Value) -> Option<(f64, f64, f64, f64)> {
        let geometry = value.get("geometry").or_else(|| value.get("bbox"))?;
        if geometry.get("type").and_then(Value::as_str) == Some("rectangle") {
            let left = geometry.get("left").and_then(Value::as_f64)?;
            let top = geometry.get("top").and_then(Value::as_f64)?;
            let width = geometry.get("width").and_then(Value::as_f64)?;
            let height = geometry.get("height").and_then(Value::as_f64)?;
            return Some((left, top, width, height));
        }
        if let Some(points) = geometry.get("points").and_then(Value::as_array) {
            let points = points
                .iter()
                .filter_map(|point| {
                    Some((
                        point.get("x").and_then(Value::as_f64)?,
                        point.get("y").and_then(Value::as_f64)?,
                    ))
                })
                .collect::<Vec<_>>();
            if !points.is_empty() {
                let min_x = points
                    .iter()
                    .map(|point| point.0)
                    .fold(f64::INFINITY, f64::min);
                let max_x = points
                    .iter()
                    .map(|point| point.0)
                    .fold(f64::NEG_INFINITY, f64::max);
                let min_y = points
                    .iter()
                    .map(|point| point.1)
                    .fold(f64::INFINITY, f64::min);
                let max_y = points
                    .iter()
                    .map(|point| point.1)
                    .fold(f64::NEG_INFINITY, f64::max);
                return Some((min_x, min_y, max_x - min_x, max_y - min_y));
            }
        }
        None
    }

    let preferred = ["line", "paragraph", "word", "block", "page"]
        .into_iter()
        .find(|candidate| elements.iter().any(|element| level(element) == *candidate));
    let mut items = Vec::new();
    for (order, element) in elements.iter().enumerate() {
        let text = element
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if text.is_empty() || preferred.is_some_and(|candidate| level(element) != candidate) {
            continue;
        }
        let fallback_y = f64::from(u32::try_from(order).unwrap_or(u32::MAX)) * 1000.0;
        let (x, y, width, height) = geometry(element).unwrap_or((0.0, fallback_y, 0.0, 0.0));
        items.push(OcrItem {
            text: text.to_string(),
            page: element
                .get("page_number")
                .and_then(Value::as_u64)
                .unwrap_or(1),
            x,
            y,
            width,
            height,
            order,
        });
    }
    items.sort_by(|left, right| {
        left.page
            .cmp(&right.page)
            .then_with(|| left.y.total_cmp(&right.y))
            .then_with(|| left.x.total_cmp(&right.x))
            .then_with(|| left.order.cmp(&right.order))
    });
    let mut lines: Vec<(u64, f64, f64, f64, String)> = Vec::new();
    for item in items {
        let same_line = lines.last().is_some_and(|(page, y, height, _, _)| {
            if *page != item.page {
                return false;
            }
            let tolerance = 4.0_f64.max(height.max(item.height) * 0.5);
            (item.y - *y).abs() <= tolerance
        });
        if same_line {
            let Some((_, _, _, previous_right, line)) = lines.last_mut() else {
                continue;
            };
            let gap = item.x - *previous_right;
            line.push_str(if gap > item.height.max(4.0) {
                "    "
            } else {
                " "
            });
            line.push_str(&item.text);
            *previous_right = item.x + item.width;
        } else {
            lines.push((
                item.page,
                item.y,
                item.height,
                item.x + item.width,
                item.text,
            ));
        }
    }
    (!lines.is_empty()).then(|| {
        lines
            .into_iter()
            .map(|(_, _, _, _, text)| text)
            .collect::<Vec<_>>()
            .join("\n")
    })
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
                    // T-17（2026-10-04 收缩为引擎原生优先）：引擎已并入宿主正文的
                    // 嵌入对象（正文含于 root）不再重复分节；"A" 的摘要与 root
                    // 不同，只有包含性判重能跳过它。
                    "path": "word/embeddings/merged.docx",
                    "result": {"content": "A"}
                }]
            }
        });
        let rendered = build_final_markdown(&envelope, &mut MediaCollector::new("x_media"))
            .expect("render should succeed");
        assert!(rendered.contains("A B"));
        assert!(rendered.contains("## Embedded document: word/embeddings/nested.docx"));
        assert!(!rendered.contains("Embedded document: word/embeddings/merged.docx"));
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
        assert_eq!(normalize_markdown("D\nrops are text"), "D\nrops are text");
    }

    // 覆盖 T-16/T-24（F19）：result 存在但为 null 时是协议异常，必须计失败，
    // 不得把空壳信封当成功拼出空结果。
    #[test]
    fn result_null_is_protocol_failure() {
        let error = build_final_markdown(
            &serde_json::json!({"result": null}),
            &mut MediaCollector::new("x_media"),
        )
        .expect_err("result 为 null 必须计失败");
        assert!(error.contains("Xberg 输出协议异常"), "{error}");
    }

    // 覆盖 T-16/T-24（F19）：result 为标量同样是协议异常。
    #[test]
    fn result_scalar_is_protocol_failure() {
        let error = build_final_markdown(
            &serde_json::json!({"result": 42}),
            &mut MediaCollector::new("x_media"),
        )
        .expect_err("result 为标量必须计失败");
        assert!(error.contains("Xberg 输出协议异常"), "{error}");
    }

    // 覆盖 T-16/T-24（F19）：空对象缺失必要结构（content/ocr_elements/children
    // 一个都没有）时必须计失败，不得静默产出空 Markdown。
    #[test]
    fn result_empty_object_is_protocol_failure() {
        let error = build_final_markdown(
            &serde_json::json!({"result": {}}),
            &mut MediaCollector::new("x_media"),
        )
        .expect_err("空 result 必须计失败");
        assert!(error.contains("Xberg 输出协议异常"), "{error}");
    }

    // 覆盖 T-16/T-24（F19）：content 类型错误是协议异常，错误信息须指出字段。
    #[test]
    fn result_content_wrong_type_is_protocol_failure() {
        let error = build_final_markdown(
            &serde_json::json!({"result": {"content": ["a"]}}),
            &mut MediaCollector::new("x_media"),
        )
        .expect_err("content 类型错误必须计失败");
        assert!(error.contains("Xberg 输出协议异常"), "{error}");
        assert!(error.contains("content"), "{error}");
    }

    // 覆盖 T-25/T-16（F19）：结构完整而正文为空的合法空白文档仍是成功。
    #[test]
    fn legal_empty_document_still_succeeds() {
        let output =
            build_document_output(&serde_json::json!({"result": {"content": ""}}), "x_media")
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
        let output = build_document_output(&envelope, "x_media").expect("结构化 OCR 结果应成功");
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
        let error =
            build_final_markdown(&serde_json::json!({}), &mut MediaCollector::new("x_media"))
                .expect_err("result is required");
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
        let output = build_document_output(&envelope, "x_media").expect("output should parse");
        assert_eq!(output.warnings.len(), 3);
        assert!(output.warnings[0].contains("主文档 · ocr"));
        assert!(output.warnings[1].contains("chart.xlsx · table"));
        assert!(output.warnings[2].contains("missing.docx · extraction"));
    }

    #[test]
    fn empty_embedded_results_are_reported_with_nested_context() {
        let envelope = serde_json::json!({
            "result": {
                "content": "root",
                "children": [{
                    "path": "outer.docx",
                    "result": {
                        "content": "outer",
                        "children": [{
                            "path": "nested.xlsx",
                            "result": {}
                        }]
                    }
                }, {
                    "path": "empty.docx",
                    "result": {}
                }]
            }
        });
        let output = build_document_output(&envelope, "x_media").expect("output should parse");
        assert!(output
            .warnings
            .iter()
            .any(|warning| warning.contains("empty.docx") && warning.contains("extraction")));
        assert!(output
            .warnings
            .iter()
            .any(|warning| warning.contains("outer.docx/nested.xlsx")
                && warning.contains("extraction")));
        assert!(!output.markdown.contains("Embedded document: empty.docx"));
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
        // 传统 xref 表布局（本解析器的支持形态；lopdf 0.45 的 Document::new
        // 默认 cross_reference_type = CrossReferenceStream 且 save 选项只在
        // 开启时改写类型，必须显式切回传统表并关闭对象流才能生成明文表）。
        document.reference_table.cross_reference_type = lopdf::xref::XrefType::CrossReferenceTable;
        let mut buffer = Vec::new();
        document
            .save_with_options(
                &mut buffer,
                lopdf::SaveOptions {
                    use_object_streams: false,
                    use_xref_streams: false,
                    ..Default::default()
                },
            )
            .unwrap();
        std::fs::write(&path, &buffer).unwrap();
        assert_eq!(page_count(&path, &generous_deadline()), Some(201));
        // xref 流布局（lopdf 默认形态）：不在本解析器支持范围，按 T-18 回退
        // 常规模式而不是误报页数。
        document.reference_table.cross_reference_type = lopdf::xref::XrefType::CrossReferenceStream;
        let modern = temp.path().join("pages-modern.pdf");
        document.save(&modern).unwrap();
        assert_eq!(page_count(&modern, &generous_deadline()), None);
    }

    // ── A1：fast 模式配置不得写回已移除的 layout 顶层键 ──

    // 覆盖 T-18（独立审查发现：`/Count N G R` 间接引用形态被当页数误报——只取
    // 第一个整数会把对象号当页数；修复后识别引用形态回 None 走常规模式）。
    #[test]
    fn pdf_page_count_rejects_indirect_count_reference() {
        use lopdf::{dictionary, Object};

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("indirect-count.pdf");
        let mut document = lopdf::Document::with_version("1.5");
        let count_id = document.add_object(Object::Integer(201));
        let pages_tree_id = document.new_object_id();
        let mut kids = Vec::new();
        for _ in 0..3 {
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
                "Count" => Object::Reference(count_id),
            }),
        );
        let catalog_id = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_tree_id,
        }));
        document.trailer.set("Root", catalog_id);
        document.reference_table.cross_reference_type = lopdf::xref::XrefType::CrossReferenceTable;
        let mut buffer = Vec::new();
        document
            .save_with_options(
                &mut buffer,
                lopdf::SaveOptions {
                    use_object_streams: false,
                    use_xref_streams: false,
                    ..Default::default()
                },
            )
            .unwrap();
        std::fs::write(&path, &buffer).unwrap();
        assert_eq!(
            page_count(&path, &generous_deadline()),
            None,
            "间接 /Count 必须回退常规模式，不得把对象号（201 的引用号）当页数"
        );
    }

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
        // T-14（2026-10-04）：常规配置走上游图片口径——图片本体随结果携带，
        // 占位引用保留、OCR 文本跟随；不再丢弃图片数据。
        assert_eq!(full["images"]["ocr_text_only"], Value::Bool(false));
        assert_eq!(full["images"]["include_data_base64"], Value::Bool(true));
        assert_eq!(full["images"]["append_ocr_text"], Value::Bool(true));
        // 快速模式（T-18）不提取图片，也不得携带 base64 载荷。
        assert_eq!(fast["images"]["include_data_base64"], Value::Bool(false));
    }

    // 覆盖 T-14：用自含确定信封固定真实装配协议，不读取仓库临时诊断文件。
    #[test]
    fn tmp_real_envelope_media_diagnostic() {
        let envelope = serde_json::json!({
            "result": {
                "content": "![image](image_0.jpeg)",
                "images": [{
                    "image_index": 0,
                    "format": "jpeg",
                    "data_base64": "/9j/4AAQSkZJRg=="
                }]
            }
        });
        let output = build_document_output(&envelope, "media").unwrap();
        assert_eq!(output.media.len(), 1);
        assert!(output.markdown.contains("](media/image_0.jpeg)"));
    }

    // 覆盖 T-14（2026-10-04）：正文引用改写为 `<media 目录>/image_N.ext`，
    // 图片字节进入待落盘清单；围栏内的字面 `image_N.ext` 不重写。
    #[test]
    fn media_refs_rewrite_and_collect_bytes() {
        let envelope = serde_json::json!({
            "result": {
                "content": "before ![a](image_0.png) after\n```text\nimage_0.png literal\n```\n",
                "images": [
                    {"image_index": 0, "format": "png", "data_base64": "aGk="}
                ]
            }
        });
        let output = build_document_output(&envelope, "doc_media").unwrap();
        assert!(
            output.markdown.contains("](doc_media/image_0.png)"),
            "引用必须指向媒体目录：{}",
            output.markdown
        );
        assert!(
            output
                .markdown
                .contains("```text\nimage_0.png literal\n```"),
            "围栏内字面文本不得重写：{}",
            output.markdown
        );
        assert_eq!(output.media.len(), 1);
        assert_eq!(output.media[0].relative, "doc_media/image_0.png");
        assert_eq!(output.media[0].bytes, b"hi");
    }

    // 覆盖 T-14：主文档与嵌入子文档的同序号图片互不碰撞——子文档加 docNNN-
    // 前缀；根文档保持上游命名。
    #[test]
    fn media_child_images_get_collision_prefix() {
        let envelope = serde_json::json!({
            "result": {
                "content": "root ![r](image_0.png)",
                "images": [
                    {"image_index": 0, "format": "png", "data_base64": "cm9vdA=="}
                ],
                "children": [
                    {"path": "embedded.docx", "result": {
                        "content": "child ![c](image_0.png)",
                        "images": [
                            {"image_index": 0, "format": "png", "data_base64": "Y2hpbGQ="}
                        ]
                    }}
                ]
            }
        });
        let output = build_document_output(&envelope, "m").unwrap();
        assert!(
            output.markdown.contains("](m/image_0.png)"),
            "{}",
            output.markdown
        );
        assert!(
            output.markdown.contains("](m/doc001-image_0.png)"),
            "{}",
            output.markdown
        );
        assert_eq!(output.media.len(), 2);
        assert_eq!(output.media[0].relative, "m/image_0.png");
        assert_eq!(output.media[0].bytes, b"root");
        assert_eq!(output.media[1].relative, "m/doc001-image_0.png");
        assert_eq!(output.media[1].bytes, b"child");
    }

    #[test]
    fn duplicate_children_with_images_are_deduplicated_before_media_prefixing() {
        let envelope = serde_json::json!({
            "result": {
                "content": "root",
                "children": [
                    {"path": "a.docx", "result": {
                        "content": "child ![x](image_0.png)",
                        "images": [{"image_index": 0, "format": "png", "data_base64": "YQ=="}]
                    }},
                    {"path": "b.docx", "result": {
                        "content": "child ![x](image_0.png)",
                        "images": [{"image_index": 0, "format": "png", "data_base64": "YQ=="}]
                    }}
                ]
            }
        });
        let output = build_document_output(&envelope, "m").unwrap();
        assert_eq!(output.markdown.matches("## Embedded document:").count(), 1);
        assert_eq!(output.media.len(), 1);
    }

    #[test]
    fn image_rewrite_only_changes_link_destinations_and_avoids_media_dir_self_hits() {
        let envelope = serde_json::json!({
            "result": {
                "content": "literal image_0.png\n![a](image_0.png) ![b](image_1.png)",
                "images": [
                    {"image_index": 0, "format": "png", "data_base64": "YQ=="},
                    {"image_index": 1, "format": "png", "data_base64": "Yg=="}
                ]
            }
        });
        let output = build_document_output(&envelope, "report_image_1.png_media").unwrap();
        assert!(output.markdown.contains("literal image_0.png"));
        assert!(output
            .markdown
            .contains("report_image_1.png_media/image_0.png"));
        assert!(output
            .markdown
            .contains("report_image_1.png_media/image_1.png"));
        assert!(!output.markdown.contains("report_report_image_1.png_media"));
    }

    #[test]
    fn fast_output_removes_image_links_and_media_files() {
        let envelope = serde_json::json!({
            "result": {
                "content": "![alt](image_0.png)",
                "images": [{"image_index": 0, "format": "png", "data_base64": "YQ=="}]
            }
        });
        let output = build_document_output_with_media(&envelope, "m", false).unwrap();
        assert_eq!(output.markdown, "alt\n");
        assert!(output.media.is_empty());
    }

    #[test]
    fn strict_base64_rejects_bad_padding_and_empty_payloads() {
        assert!(decode_base64("").is_none());
        assert!(decode_base64("a=").is_none());
        assert!(decode_base64("YQ==junk").is_none());
        assert_eq!(decode_base64("aGk="), Some(b"hi".to_vec()));
        assert_eq!(decode_base64("aGVsbG8="), Some(b"hello".to_vec()));
        assert_eq!(decode_base64("/////w=="), Some(vec![255; 4]));
        assert!(decode_base64("YQ==YQ==").is_none());
    }

    #[test]
    fn malformed_referenced_image_is_reported_as_partial() {
        let envelope = serde_json::json!({
            "result": {
                "content": "![broken](image_0.png)",
                "images": [{"image_index": 0, "format": "png", "data_base64": "%%%"}]
            }
        });
        let output = build_document_output(&envelope, "m").unwrap();
        assert!(output.markdown.contains("image_0.png"));
        assert!(output
            .warnings
            .iter()
            .any(|warning| warning.contains("Base64")));
    }

    #[test]
    fn structured_ocr_uses_geometry_and_avoids_parent_word_duplication() {
        let envelope = serde_json::json!({"result": {
            "ocr_elements": [
                {"text": "整行", "level": "line", "page_number": 1,
                 "geometry": {"type": "rectangle", "left": 0, "top": 0, "width": 100, "height": 20}},
                {"text": "整", "level": "word", "page_number": 1, "parent_id": "line-1",
                 "geometry": {"type": "rectangle", "left": 0, "top": 0, "width": 20, "height": 20}},
                {"text": "行", "level": "word", "page_number": 1, "parent_id": "line-1",
                 "geometry": {"type": "rectangle", "left": 80, "top": 0, "width": 20, "height": 20}},
                {"text": "下一行", "level": "line", "page_number": 1,
                 "geometry": {"type": "rectangle", "left": 0, "top": 40, "width": 100, "height": 20}}
            ]
        }});
        let output = build_document_output(&envelope, "m").unwrap();
        assert_eq!(output.markdown, "整行\n下一行\n");
    }

    #[test]
    fn inline_code_keeps_longer_inner_backtick_runs_literal() {
        assert_eq!(normalize_markdown("`a `` &#35;`"), "`a `` &#35;`");
    }

    #[test]
    fn entity_at_markdown_marker_position_is_escaped_without_decoding_code() {
        assert_eq!(
            normalize_markdown("&#35; 标题\n&#42;文字&#42;"),
            "\\# 标题\n\\*文字*"
        );
    }

    #[test]
    fn huge_deadline_creation_does_not_panic() {
        let deadline = Deadline::new(Duration::MAX);
        assert!(!deadline.expired() || deadline.remaining().is_zero());
    }

    #[test]
    fn pdf_name_probe_ignores_names_inside_literal_strings() {
        let object = b"<< /Title (/Pages 2 0 R) /Type /Catalog /Pages 3 0 R >>";
        let first = find_pdf_name(object, b"/Pages").expect("real PDF name should exist");
        assert_eq!(&object[first..first + 6], b"/Pages");
        assert_eq!(indirect_reference_after(object, b"/Pages"), Some((3, 0)));
        let named_prefix = b"<< /Pages-tree 1 0 R /Pages 2 0 R >>";
        assert_eq!(
            indirect_reference_after(named_prefix, b"/Pages"),
            Some((2, 0))
        );
    }

    // 覆盖 T-14：正文未引用的图片不落盘；base64 损坏的条目保持占位引用原样
    //（如实降级，不改写也不产生半字节文件）。
    #[test]
    fn media_unreferenced_or_corrupt_entries_are_skipped() {
        let envelope = serde_json::json!({
            "result": {
                "content": "only ![b](image_1.png)",
                "images": [
                    {"image_index": 0, "format": "png", "data_base64": "aGk="},
                    {"image_index": 1, "format": "png", "data_base64": "%%%not-base64%%%"}
                ]
            }
        });
        let output = build_document_output(&envelope, "m").unwrap();
        assert!(
            output.markdown.contains("](image_1.png)"),
            "损坏条目必须保持占位引用：{}",
            output.markdown
        );
        assert_eq!(output.media.len(), 0);
    }
}
