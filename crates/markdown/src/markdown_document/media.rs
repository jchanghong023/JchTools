use serde_json::Value;

use super::convert::MediaFile;
use super::links::scan_image_links;

/// T-14 媒体落盘（2026-10-04 引擎重编号口径）：引擎已把嵌入子文档图片围栏
/// 感知地重编号进宿主 `images`（不再使用 docNNN- 前缀），引用与字节一一对应；
/// 本函数执行 FORK.md 公布的唯一调用方步骤——把正文中的裸引用 `image_N.ext`
/// 改写为 `<媒体目录>/image_N.ext` 并登记待落盘字节。围栏与行内代码内的
/// `image_N.ext` 字样是字面文本，不改写也不据此落盘；正文未引用的图片不落盘
/// （与引擎引用↔字节对应口径一致）。字节缺失或解码失败时保留占位引用并告警。
pub(super) fn collect_media(
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
pub(super) fn image_bytes(image: &Value) -> Option<Vec<u8>> {
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

/// 标准 base64（RFC 4648 字母表）解码，自足实现——不为单图解码引入新依赖。
/// 遇到非法字符返回 None（调用方保持占位原样）。
pub(super) fn decode_base64(input: &str) -> Option<Vec<u8>> {
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
