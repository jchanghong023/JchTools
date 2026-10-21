use std::io::{self, Read, Seek, SeekFrom};

pub(super) fn bytes<const N: usize>(reader: &mut impl Read) -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}
pub(super) fn little32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

pub(super) fn length64(length: usize) -> io::Result<u64> {
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
pub(super) fn rar5_extra_fields_valid(
    reader: &mut (impl Read + Seek),
    length: u64,
) -> io::Result<bool> {
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
