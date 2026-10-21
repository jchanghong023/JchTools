//! C-08 / 附录 B：先验证格式结构，再返回可确定的类型；截断或歧义内容不改后缀。
mod flac;
mod readers;
mod wav;

use self::flac::valid_flac;
use self::readers::rar5_extra_fields_valid;
use self::wav::valid_wav;
use crate::control::Control;
use anyhow::Result;
use std::borrow::Cow;
use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

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
        pdf_extension(reader, length)
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

// 判型内部预算：ZIP 的 XML 元数据或完整 PDF 输入最多 16MiB；不是产品文件大小限制。
const MAX_TYPE_METADATA: u64 = 16 * 1024 * 1024;

fn pdf_extension(reader: impl Read, length: u64) -> Option<&'static str> {
    if length > MAX_TYPE_METADATA {
        return None;
    }
    let limit = usize::try_from(MAX_TYPE_METADATA).ok()?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(usize::try_from(length).ok()?)
        .ok()?;
    // 元数据检查后文件仍可能增长；多读一个探测字节，不能把有界前缀当成完整 PDF。
    reader
        .take(MAX_TYPE_METADATA + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > limit {
        return None;
    }
    lopdf::Document::load_mem_with_options(
        &bytes,
        lopdf::LoadOptions {
            strict: true,
            // 限制加载期单个 ObjStm/XRef 解压流，不代表所有 PDF 对象的累计内存上界。
            max_decompressed_size: Some(limit),
            ..lopdf::LoadOptions::default()
        },
    )
    .ok()
    .filter(|document| document.catalog().is_ok())
    .map(|_| "pdf")
}

fn zip_text<R: Read + Seek>(archive: &mut zip::ZipArchive<R>, name: &str) -> Option<String> {
    let index = archive.index_for_name(name)?;
    zip_text_by_index(archive, index)
}

fn zip_text_by_index<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
    index: usize,
) -> Option<String> {
    let entry = archive.by_index(index).ok()?;
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
        let data_start = entry.data_start()?;
        if data_start.checked_add(entry.compressed_size())? > length {
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
        // OPC 必须由包级 officeDocument 关联确定主部件；孤立 XML/MIME 声明
        // 不能证明它是可使用的 Office 文档，也不能跟随外部关联猜测类型。
        let relationships_xml = zip_text(&mut archive, "_rels/.rels")?;
        let relationships = roxmltree::Document::parse(&relationships_xml).ok()?;
        let relationship_namespace = "http://schemas.openxmlformats.org/package/2006/relationships";
        let relationship_root = relationships.root_element();
        if !relationship_root.has_tag_name((relationship_namespace, "Relationships")) {
            return None;
        }
        let mut office_relationships = relationship_root.children().filter(|node| {
            node.has_tag_name((relationship_namespace, "Relationship"))
                && matches!(
                    node.attribute("Type"),
                    Some("http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument"
                        | "http://purl.oclc.org/ooxml/officeDocument/relationships/officeDocument")
                )
        });
        let relationship = office_relationships.next()?;
        if office_relationships.next().is_some()
            || !matches!(
                relationship.attribute("TargetMode"),
                None | Some("Internal")
            )
        {
            return None;
        }
        relationship.attribute("Id").filter(|id| !id.is_empty())?;
        let target = relationship.attribute("Target")?;
        // RFC 3986 的相对 path-noscheme 仅首段禁止冒号；绝对包路径与
        // 后续段中的冒号是部件名字符，不能一概当成外部 scheme。
        if target.contains(['\\', '?', '#'])
            || target.split('/').next()?.contains(':')
            || target.contains("//")
            || matches!(target.rsplit('/').next(), Some("" | "." | ".."))
        {
            return None;
        }
        // 根级关联按 URI 路径解析，允许绝对包路径和合法的相对点段。
        // 空段、URI authority 和以目录结尾的引用不能折叠成普通部件名。
        let mut segments = Vec::new();
        for segment in target.split('/') {
            match segment {
                "" | "." => {}
                ".." => {
                    segments.pop()?;
                }
                segment => segments.push(segment),
            }
        }
        let office_member = segments.join("/");
        // OPC 部件按 ASCII 忽略大小写比较，保留 ZIP 中的真实名称与字节。
        // 同一等价名称对应多个真实成员时不能任选其一授权修正扩展名。
        let main_index = {
            let mut members = archive
                .file_names()
                .filter_map(Result::ok)
                .filter(|member| member.eq_ignore_ascii_case(&office_member));
            let member = members.next()?;
            if members.next().is_some() {
                return None;
            }
            archive.index_for_name(&member)?
        };
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
            if !member.eq_ignore_ascii_case(&office_member) {
                continue;
            }
            // 必须有可解析的主部件，而不只是声称某 MIME 的空 ZIP 目录项。
            let main_xml = zip_text_by_index(&mut archive, main_index)?;
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

#[cfg(test)]
mod tests;
