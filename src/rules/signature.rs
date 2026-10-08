//! C-08 / 附录 B：先验证格式结构，再返回可确定的类型；截断或歧义内容不改后缀。
use crate::control::Control;
use anyhow::Result;
use std::{
    borrow::Cow,
    fs::File,
    io::{self, BufReader, Read, Seek, SeekFrom},
    path::Path,
    process::Command,
    time::Duration,
};

pub fn detect_extension(path: &Path, control: &Control) -> Result<Option<&'static str>> {
    control.check_cancelled()?;
    let file = File::open(path)?;
    let length = file.metadata()?.len();
    let mut reader = BufReader::new(file);
    let mut prefix = [0u8; 32];
    let count = usize::try_from(length.min(32))?;
    reader.read_exact(&mut prefix[..count])?;
    reader.rewind()?;
    if let Ok(format) = image::guess_format(&prefix[..count]) {
        let extension = match format {
            image::ImageFormat::Png => "png",
            image::ImageFormat::Jpeg => "jpg",
            image::ImageFormat::Gif => "gif",
            image::ImageFormat::Bmp => "bmp",
            image::ImageFormat::Tiff => "tif",
            image::ImageFormat::WebP => "webp",
            _ => return Ok(None),
        };
        // 不凭尺寸头判型：让现成解码器检查图像数据。仅在明确启用签名修正时调用。
        let image = image::ImageReader::with_format(reader, format);
        let detected = image.decode().is_ok().then_some(extension);
        control.check_cancelled()?;
        return Ok(detected);
    }
    let detected = if prefix.starts_with(b"PK\x03\x04") || prefix.starts_with(b"PK\x05\x06") {
        zip::ZipArchive::new(reader)
            .ok()
            .and_then(|archive| zip_extension(archive, length))
    } else if prefix.starts_with(b"%PDF-") {
        lopdf::Document::load_from_with_options(
            reader,
            lopdf::LoadOptions {
                strict: true,
                ..lopdf::LoadOptions::default()
            },
        )
        .ok()
        .filter(|document| document.catalog().is_ok())
        .map(|_| "pdf")
    } else if prefix.starts_with(b"7z\xbc\xaf\x27\x1c") {
        archive_oracle(path, control, "-t7z", "7z")?.then_some("7z")
    } else if prefix.starts_with(b"Rar!\x1a\x07\0") {
        archive_oracle(path, control, "-trar", "Rar")?.then_some("rar")
    } else if prefix.starts_with(b"Rar!\x1a\x07\x01\0") {
        (rar5_extra_fields_valid(&mut reader, length).unwrap_or(false)
            && archive_oracle(path, control, "-trar5", "Rar5")?)
        .then_some("rar")
    } else if prefix.starts_with(b"RIFF") && &prefix[8..12] == b"WAVE" {
        valid_wav(&mut reader, length)
            .unwrap_or(false)
            .then_some("wav")
    } else if prefix.starts_with(b"fLaC") {
        valid_flac(&mut reader, length, control)
            .unwrap_or(false)
            .then_some("flac")
    } else {
        None
    };
    control.check_cancelled()?;
    Ok(detected)
}

// XML 类型元数据只作判型，不为此展开无关数据；过大的元数据无法安全确认时保留后缀。
const MAX_TYPE_METADATA: u64 = 16 * 1024 * 1024;
fn zip_text<R: Read + Seek>(archive: &mut zip::ZipArchive<R>, name: &str) -> Option<String> {
    let entry = archive.by_name(name).ok()?;
    let size = entry.size();
    if size > MAX_TYPE_METADATA {
        return None;
    }
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(usize::try_from(size).ok()?).ok()?;
    entry
        .take(MAX_TYPE_METADATA + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if u64::try_from(bytes.len()).ok()? != size {
        return None;
    }
    if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        if bytes.len() % 2 != 0 {
            return None;
        }
        let little = bytes[0] == 0xff;
        char::decode_utf16(bytes[2..].as_chunks::<2>().0.iter().map(|pair| {
            if little {
                u16::from_le_bytes([pair[0], pair[1]])
            } else {
                u16::from_be_bytes([pair[0], pair[1]])
            }
        }))
        .collect::<std::result::Result<String, _>>()
        .ok()
    } else {
        String::from_utf8(bytes).ok()
    }
}

fn zip_extension<R: Read + Seek>(
    mut archive: zip::ZipArchive<R>,
    length: u64,
) -> Option<&'static str> {
    // 目录索引之外还检查实际本地头及数据边界；不为判型解压无关的大型成员。
    for index in 0..archive.len() {
        let entry = archive.by_index_raw(index).ok()?;
        if entry.data_start().checked_add(entry.compressed_size())? > length {
            return None;
        }
    }
    if archive.index_for_name("mimetype").is_some() {
        let mime = zip_text(&mut archive, "mimetype")?;
        let (extension, required) = match mime.as_str() {
            "application/vnd.oasis.opendocument.text" => ("odt", "content.xml"),
            "application/vnd.oasis.opendocument.spreadsheet" => ("ods", "content.xml"),
            "application/vnd.oasis.opendocument.presentation" => ("odp", "content.xml"),
            "application/epub+zip" => ("epub", "META-INF/container.xml"),
            // 存在未知的内部文档类型标识时不能降级成普通 ZIP。
            _ => return None,
        };
        let xml = zip_text(&mut archive, required)?;
        let document = roxmltree::Document::parse(&xml).ok()?;
        if extension == "epub" {
            let namespace = "urn:oasis:names:tc:opendocument:xmlns:container";
            if !document
                .root_element()
                .has_tag_name((namespace, "container"))
            {
                return None;
            }
            let package = document
                .descendants()
                .find(|node| {
                    node.has_tag_name((namespace, "rootfile"))
                        && node.attribute("media-type") == Some("application/oebps-package+xml")
                })?
                .attribute("full-path")?;
            let package_xml = zip_text(&mut archive, package)?;
            let package_document = roxmltree::Document::parse(&package_xml).ok()?;
            if !package_document
                .root_element()
                .has_tag_name(("http://www.idpf.org/2007/opf", "package"))
            {
                return None;
            }
        } else if !document.root_element().has_tag_name((
            "urn:oasis:names:tc:opendocument:xmlns:office:1.0",
            "document-content",
        )) {
            return None;
        }
        return Some(extension);
    }
    if archive.index_for_name("[Content_Types].xml").is_some() {
        let xml = zip_text(&mut archive, "[Content_Types].xml")?;
        let document = roxmltree::Document::parse(&xml).ok()?;
        let namespace = "http://schemas.openxmlformats.org/package/2006/content-types";
        let root = document.root_element();
        if !root.has_tag_name((namespace, "Types")) {
            return None;
        }
        let mut detected = None;
        for node in root
            .children()
            .filter(|node| node.has_tag_name((namespace, "Override")))
        {
            let mime = node.attribute("ContentType")?;
            let extension = match mime {
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml" => "docx",
                "application/vnd.ms-word.document.macroEnabled.main+xml" => "docm",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml" => "xlsx",
                "application/vnd.ms-excel.sheet.macroEnabled.main+xml" => "xlsm",
                "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml" => "pptx",
                "application/vnd.ms-powerpoint.presentation.macroEnabled.main+xml" => "pptm",
                _ => continue,
            };
            let member = node.attribute("PartName")?.strip_prefix('/')?;
            // 必须有可解析的主部件，而不只是声称某 MIME 的空 ZIP 目录项。
            let main_xml = zip_text(&mut archive, member)?;
            let main_document = roxmltree::Document::parse(&main_xml).ok()?;
            let (family, root_name) = match extension {
                "docx" | "docm" => ("wordprocessingml", "document"),
                "xlsx" | "xlsm" => ("spreadsheetml", "workbook"),
                "pptx" | "pptm" => ("presentationml", "presentation"),
                _ => return None,
            };
            let namespace = format!("http://schemas.openxmlformats.org/{family}/2006/main");
            let strict_namespace = format!("http://purl.oclc.org/ooxml/{family}/main");
            if !main_document
                .root_element()
                .has_tag_name((namespace.as_str(), root_name))
                && !main_document
                    .root_element()
                    .has_tag_name((strict_namespace.as_str(), root_name))
            {
                return None;
            }
            if detected.is_some_and(|previous| previous != extension) {
                return None;
            }
            detected = Some(extension);
        }
        return detected;
    }
    Some("zip")
}

fn bytes<const N: usize>(reader: &mut impl Read) -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}
fn little32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

// 成熟官方引擎只解析档案头；不做解压或全体 payload test。排除成员输出使捕获量
// 不随文件数增长；不完整输出、超时、引擎不可用都不能授权改扩展名。
fn archive_oracle(path: &Path, control: &Control, format: &str, expected: &str) -> Result<bool> {
    let Ok(executable) = crate::engine_bundle::resolve_executable() else {
        return Ok(false);
    };
    let source = if path.is_absolute() {
        Cow::Borrowed(path)
    } else {
        Cow::Owned(std::env::current_dir()?.join(path))
    };
    let mut command = Command::new(&executable);
    if let Some(directory) = executable.parent() {
        command.current_dir(directory);
    }
    command
        .args([
            "l",
            "-slt",
            "-xr!*",
            "-sccUTF-8",
            "-p-",
            "-bd",
            "-bb0",
            format,
            "--",
        ])
        .arg(source.as_ref());
    let Ok(output) = crate::process::run_with_timeout_control_limit(
        &mut command,
        Duration::from_secs(10),
        control,
        64 * 1024,
    ) else {
        control.check_cancelled()?;
        return Ok(false);
    };
    if !output.status.success() || output.stdout_truncated || output.stderr_truncated {
        return Ok(false);
    }
    let Ok(text) = std::str::from_utf8(&output.stdout) else {
        return Ok(false);
    };
    let mut lines = text.lines().skip_while(|line| *line != "--");
    if lines.next().is_none() {
        return Ok(false);
    }
    let mut detected = None;
    for line in lines {
        if line.is_empty() || line == "----------" {
            break;
        }
        if let Some(value) = line.strip_prefix("Type = ") {
            if detected.replace(value).is_some() {
                return Ok(false);
            }
        }
    }
    Ok(detected.is_some_and(|value| value.eq_ignore_ascii_case(expected)))
}
fn length64(length: usize) -> io::Result<u64> {
    u64::try_from(length)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "格式长度超出范围"))
}

fn vint(reader: &mut impl Read) -> io::Result<u64> {
    let mut result = 0u64;
    for shift in (0..70).step_by(7) {
        let value = bytes::<1>(reader)?[0];
        if shift == 63 && value > 1 {
            break;
        }
        result |= u64::from(value & 0x7f) << shift;
        if value & 0x80 == 0 {
            return Ok(result);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "RAR 整数编码非法",
    ))
}
// 7-Zip 的 listing 可忽略不认识的 RAR5 extra；先验证通用 TLV 边界，避免把截断
// record 当有效结构。主字段、CRC 与压缩 header 仍由官方 parser 确认，且不读数据区。
fn rar5_extra_fields_valid(reader: &mut (impl Read + Seek), length: u64) -> io::Result<bool> {
    reader.seek(SeekFrom::Start(8))?;
    while reader.stream_position()? < length {
        bytes::<4>(reader)?; // CRC 由官方 parser 校验
        let size = vint(reader)?;
        if size > 0x001f_ffff {
            return Ok(false);
        }
        let Some(header_end) = reader.stream_position()?.checked_add(size) else {
            return Ok(false);
        };
        if header_end > length {
            return Ok(false);
        }
        let (kind, extra, data_size) = {
            let mut fields = reader.by_ref().take(size);
            let kind = vint(&mut fields)?;
            let flags = vint(&mut fields)?;
            let extra = if flags & 1 != 0 {
                vint(&mut fields)?
            } else {
                0
            };
            let data_size = if flags & 2 != 0 {
                vint(&mut fields)?
            } else {
                0
            };
            if extra > fields.limit() {
                return Ok(false);
            }
            (kind, extra, data_size)
        };
        reader.seek(SeekFrom::Start(header_end - extra))?;
        while reader.stream_position()? < header_end {
            let remaining = header_end - reader.stream_position()?;
            let (field_end, field_type) = {
                let mut fields = reader.by_ref().take(remaining);
                let field_size = vint(&mut fields)?;
                if field_size == 0 || field_size > fields.limit() {
                    return Ok(false);
                }
                let field_end = header_end - fields.limit() + field_size;
                let field_type = vint(&mut fields)?;
                if header_end - fields.limit() > field_end {
                    return Ok(false);
                }
                (field_end, field_type)
            };
            if field_type == 0 {
                return Ok(false);
            }
            reader.seek(SeekFrom::Start(field_end))?;
        }
        let Some(end) = header_end.checked_add(data_size) else {
            return Ok(false);
        };
        if end > length {
            return Ok(false);
        }
        if kind == 5 {
            return Ok(true);
        }
        reader.seek(SeekFrom::Start(end))?;
    }
    Ok(false)
}

fn valid_wav(reader: &mut (impl Read + Seek), length: u64) -> io::Result<bool> {
    let header = bytes::<12>(reader)?;
    let end = u64::from(little32(&header[4..8])) + 8;
    if end < 12 || end > length {
        return Ok(false);
    }
    let mut format_seen = false;
    let mut data_seen = false;
    while reader.stream_position()? < end {
        let chunk = bytes::<8>(reader)?;
        let size = u64::from(little32(&chunk[4..]));
        let start = reader.stream_position()?;
        let Some(next) = start
            .checked_add(size)
            .and_then(|value| value.checked_add(size % 2))
        else {
            return Ok(false);
        };
        if next > end {
            return Ok(false);
        }
        if &chunk[..4] == b"fmt " {
            if size < 16 {
                return Ok(false);
            }
            let format = bytes::<16>(reader)?;
            if u16::from_le_bytes([format[0], format[1]]) == 0
                || u16::from_le_bytes([format[2], format[3]]) == 0
                || little32(&format[4..8]) == 0
                || u16::from_le_bytes([format[12], format[13]]) == 0
            {
                return Ok(false);
            }
            format_seen = true;
        } else if &chunk[..4] == b"data" {
            data_seen = true;
        }
        reader.seek(SeekFrom::Start(next))?;
    }
    Ok(format_seen && data_seen)
}

fn valid_flac(reader: &mut (impl Read + Seek), length: u64, control: &Control) -> io::Result<bool> {
    reader.seek(SeekFrom::Start(4))?;
    let first = bytes::<4>(reader)?;
    if first[0] & 0x7f != 0 || first[1..] != [0, 0, 34] {
        return Ok(false);
    }
    let stream = bytes::<34>(reader)?;
    let packed = u64::from_be_bytes(stream[10..18].try_into().map_err(io::Error::other)?);
    let info = FlacInfo {
        minimum: u32::from(u16::from_be_bytes([stream[0], stream[1]])),
        maximum: u32::from(u16::from_be_bytes([stream[2], stream[3]])),
        depth: u8::try_from(((packed >> 36) & 31) + 1).map_err(io::Error::other)?,
        channels: u8::try_from(((packed >> 41) & 7) + 1).map_err(io::Error::other)?,
        rate: u32::try_from(packed >> 44).map_err(io::Error::other)?,
        total: packed & ((1u64 << 36) - 1),
    };
    if info.minimum < 16
        || info.maximum < info.minimum
        || !(4..=32).contains(&info.depth)
        || info.rate == 0
        || info.rate > 655_350
    {
        return Ok(false);
    }
    let mut last = first[0] & 0x80 != 0;
    while !last {
        let block = bytes::<4>(reader)?;
        let kind = block[0] & 0x7f;
        if kind == 0 || kind == 127 {
            return Ok(false);
        }
        last = block[0] & 0x80 != 0;
        let size = u64::from(u32::from_be_bytes([0, block[1], block[2], block[3]]));
        if (kind == 2 && size < 4) || (kind == 3 && size % 18 != 0) {
            return Ok(false);
        }
        let Some(end) = reader.stream_position()?.checked_add(size) else {
            return Ok(false);
        };
        if end > length {
            return Ok(false);
        }
        reader.seek(SeekFrom::Start(end))?;
    }
    let mut samples = 0u64;
    let mut frames = 0u64;
    let mut strategy = None;
    while reader.stream_position()? < length {
        let Some(count) = flac_frame(reader, &info, control, frames, samples, &mut strategy)?
        else {
            return Ok(false);
        };
        if count < info.minimum && reader.stream_position()? != length {
            return Ok(false);
        }
        let Some(next) = samples.checked_add(u64::from(count)) else {
            return Ok(false);
        };
        samples = next;
        frames += 1;
    }
    Ok(info.total == 0 || info.total == samples)
}

struct FlacInfo {
    minimum: u32,
    maximum: u32,
    depth: u8,
    channels: u8,
    rate: u32,
    total: u64,
}
// 只解析编码位结构及逐帧 CRC，不分配样本数组、不做音频重建。
struct FlacFrame<'a, R> {
    reader: &'a mut R,
    control: &'a Control,
    current: u8,
    available: u8,
    crc: u16,
    header_crc: u8,
    in_header: bool,
}
impl<R: Read> FlacFrame<'_, R> {
    fn check_cancelled(&self) -> io::Result<()> {
        if self.control.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "用户取消"));
        }
        Ok(())
    }
    fn byte(&mut self) -> io::Result<u8> {
        let value = bytes::<1>(self.reader)?[0];
        self.crc = (self.crc << 8) ^ FLAC_CRC16[usize::from(self.crc.to_be_bytes()[0] ^ value)];
        if self.in_header {
            self.header_crc ^= value;
            for _ in 0..8 {
                self.header_crc = if self.header_crc & 0x80 != 0 {
                    (self.header_crc << 1) ^ 7
                } else {
                    self.header_crc << 1
                };
            }
        }
        Ok(value)
    }
    fn word(&mut self) -> io::Result<u16> {
        Ok(u16::from_be_bytes([self.byte()?, self.byte()?]))
    }
    fn bits(&mut self, mut count: u8) -> io::Result<u64> {
        let mut value = 0;
        while count != 0 {
            if self.available == 0 {
                self.current = self.byte()?;
                self.available = 8;
            }
            let take = count.min(self.available);
            value = (value << take)
                | u64::from((self.current >> (self.available - take)) & (u8::MAX >> (8 - take)));
            self.available -= take;
            count -= take;
        }
        Ok(value)
    }
    fn skip(&mut self, mut count: u64) -> io::Result<()> {
        let mut buffer = [0; 8192];
        while count != 0 {
            self.check_cancelled()?;
            if self.available == 0 && count >= 8 {
                let size = usize::try_from((count / 8).min(8192)).map_err(io::Error::other)?;
                self.reader.read_exact(&mut buffer[..size])?;
                for value in &buffer[..size] {
                    self.crc = (self.crc << 8)
                        ^ FLAC_CRC16[usize::from(self.crc.to_be_bytes()[0] ^ value)];
                }
                count -= length64(size)? * 8;
            } else {
                let take = u8::try_from(count.min(8)).map_err(io::Error::other)?;
                self.bits(take)?;
                count -= u64::from(take);
            }
        }
        Ok(())
    }
    fn unary(&mut self, limit: u64) -> io::Result<Option<u64>> {
        let mut zeroes = 0;
        while self.bits(1)? == 0 {
            if zeroes == limit {
                return Ok(None);
            }
            zeroes += 1;
            if zeroes.trailing_zeros() >= 16 {
                self.check_cancelled()?;
            }
        }
        Ok(Some(zeroes))
    }
    fn number(&mut self) -> io::Result<Option<u64>> {
        let first = self.byte()?;
        if first < 0x80 {
            return Ok(Some(u64::from(first)));
        }
        let count = first.leading_ones();
        if !(2..=7).contains(&count) {
            return Ok(None);
        }
        let mut value = u64::from(first & ((1u8 << (7 - count)) - 1));
        for _ in 1..count {
            let continuation = self.byte()?;
            if continuation & 0xc0 != 0x80 {
                return Ok(None);
            }
            value = (value << 6) | u64::from(continuation & 0x3f);
        }
        let minimum = match count {
            2 => 1 << 7,
            3 => 1 << 11,
            4 => 1 << 16,
            5 => 1 << 21,
            6 => 1 << 26,
            _ => 1 << 31,
        };
        Ok((value >= minimum).then_some(value))
    }
    fn finish(mut self) -> io::Result<bool> {
        let padding = self.available;
        if self.bits(padding)? != 0 {
            return Ok(false);
        }
        Ok(u16::from_be_bytes(bytes::<2>(self.reader)?) == self.crc)
    }
}
fn flac_frame(
    reader: &mut impl Read,
    info: &FlacInfo,
    control: &Control,
    frame_number: u64,
    samples: u64,
    strategy: &mut Option<bool>,
) -> io::Result<Option<u32>> {
    let mut frame = FlacFrame {
        reader,
        control,
        current: 0,
        available: 0,
        crc: 0,
        header_crc: 0,
        in_header: true,
    };
    frame.check_cancelled()?;
    let sync = [frame.byte()?, frame.byte()?];
    let sizes = frame.byte()?;
    let channels = frame.byte()?;
    if sync[0] != 0xff || sync[1] & 0xfe != 0xf8 || channels & 1 != 0 {
        return Ok(None);
    }
    let variable = sync[1] & 1 != 0;
    if strategy.is_some_and(|old| old != variable) {
        return Ok(None);
    }
    *strategy = Some(variable);
    let Some(number) = frame.number()? else {
        return Ok(None);
    };
    if number != if variable { samples } else { frame_number }
        || (!variable && number > 0x7fff_ffff)
    {
        return Ok(None);
    }
    let count = match sizes >> 4 {
        0 => return Ok(None),
        1 => 192,
        code @ 2..=5 => 576u32 << (code - 2),
        6 => u32::from(frame.byte()?) + 1,
        7 => u32::from(frame.word()?) + 1,
        code => 256u32 << (code - 8),
    };
    if count > info.maximum {
        return Ok(None);
    }
    let rate = match sizes & 15 {
        0 => info.rate,
        1 => 88200,
        2 => 176_400,
        3 => 192_000,
        4 => 8000,
        5 => 16000,
        6 => 22050,
        7 => 24000,
        8 => 32000,
        9 => 44100,
        10 => 48000,
        11 => 96000,
        12 => u32::from(frame.byte()?) * 1000,
        13 => u32::from(frame.word()?),
        14 => u32::from(frame.word()?) * 10,
        _ => return Ok(None),
    };
    let depth = match (channels >> 1) & 7 {
        0 => info.depth,
        1 => 8,
        2 => 12,
        4 => 16,
        5 => 20,
        6 => 24,
        7 => 32,
        _ => return Ok(None),
    };
    let assignment = channels >> 4;
    let channel_count = if assignment < 8 { assignment + 1 } else { 2 };
    if assignment > 10 || channel_count != info.channels || depth != info.depth || rate != info.rate
    {
        return Ok(None);
    }
    frame.byte()?; // 包括 CRC8 的 header 余数必须为零
    if frame.header_crc != 0 {
        return Ok(None);
    }
    frame.in_header = false;
    for channel in 0..channel_count {
        let width = depth
            + u8::from(
                (assignment == 8 || assignment == 10) && channel == 1
                    || assignment == 9 && channel == 0,
            );
        if !flac_subframe(&mut frame, count, width)? {
            return Ok(None);
        }
    }
    Ok(frame.finish()?.then_some(count))
}
fn flac_subframe(
    frame: &mut FlacFrame<'_, impl Read>,
    samples: u32,
    width: u8,
) -> io::Result<bool> {
    let header = frame.bits(8)?;
    if header & 0x80 != 0 {
        return Ok(false);
    }
    let mut width = u64::from(width);
    if header & 1 != 0 {
        let Some(zeroes) = frame.unary(width - 1)? else {
            return Ok(false);
        };
        if zeroes + 1 >= width {
            return Ok(false);
        }
        width -= zeroes + 1;
    }
    let kind = (header >> 1) & 63;
    let order = match kind {
        0 => {
            frame.skip(width)?;
            return Ok(true);
        }
        1 => {
            frame.skip(width * u64::from(samples))?;
            return Ok(true);
        }
        8..=12 => kind - 8,
        32..=63 => kind - 31,
        _ => return Ok(false),
    };
    if order > u64::from(samples) {
        return Ok(false);
    }
    frame.skip(width * order)?;
    if kind >= 32 {
        let precision = frame.bits(4)?;
        if precision == 15 {
            return Ok(false);
        }
        frame.skip(5 + order * (precision + 1))?; // 有符号 shift 与 LPC 系数
    }
    let parameter_bits = match frame.bits(2)? {
        0 => 4,
        1 => 5,
        _ => return Ok(false),
    };
    let partitions = 1u32 << frame.bits(4)?;
    if !samples.is_multiple_of(partitions) {
        return Ok(false);
    }
    let partition_samples = u64::from(samples / partitions);
    for partition in 0..partitions {
        let count = if partition == 0 {
            let Some(count) = partition_samples.checked_sub(order) else {
                return Ok(false);
            };
            count
        } else {
            partition_samples
        };
        let parameter = frame.bits(parameter_bits)?;
        if parameter == (1u64 << parameter_bits) - 1 {
            let raw_width = frame.bits(5)?;
            frame.skip(count * raw_width)?;
        } else {
            for _ in 0..count {
                // FLAC residual 为有符号 32 位，折叠后的 Rice 数值不得超出 u32。
                if frame.unary(u64::from(u32::MAX) >> parameter)?.is_none() {
                    return Ok(false);
                }
                frame.skip(parameter)?;
            }
        }
    }
    Ok(true)
}
const FLAC_CRC16: [u16; 256] = flac_crc_table();
const fn flac_crc_table() -> [u16; 256] {
    let mut table = [0; 256];
    let mut index = 0;
    let mut byte = 0u16;
    while index < 256 {
        let mut value = byte << 8;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 0x8000 != 0 {
                (value << 1) ^ 0x8005
            } else {
                value << 1
            };
            bit += 1;
        }
        table[index] = value;
        index += 1;
        byte += 1;
    }
    table
}
