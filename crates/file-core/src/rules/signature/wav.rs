use super::readers::{bytes, little32};
use std::io::{self, Read, Seek, SeekFrom};

pub(super) fn valid_wav(reader: &mut (impl Read + Seek), length: u64) -> io::Result<bool> {
    let header = bytes::<12>(reader)?;
    let end = u64::from(little32(&header[4..8])) + 8;
    if end < 12 || end > length {
        return Ok(false);
    }
    let mut format_seen = false;
    let mut data_seen = false;
    let mut sample_alignment = None;
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
            let mut kind = u16::from_le_bytes([format[0], format[1]]);
            let channels = u16::from_le_bytes([format[2], format[3]]);
            let rate = little32(&format[4..8]);
            let alignment = u16::from_le_bytes([format[12], format[13]]);
            if kind == 0 || channels == 0 || rate == 0 || alignment == 0 {
                return Ok(false);
            }
            if kind == 0xfffe {
                // WAVE_FORMAT_EXTENSIBLE 必须包含完整扩展和可确认的采样子类型。
                if size < 40 {
                    return Ok(false);
                }
                let extra = bytes::<24>(reader)?;
                let cb_size = u16::from_le_bytes([extra[0], extra[1]]);
                let valid_bits = u16::from_le_bytes([extra[2], extra[3]]);
                let mask = little32(&extra[4..8]);
                let depth = u16::from_le_bytes([format[14], format[15]]);
                let subformat = &extra[8..24];
                if cb_size < 22
                    || u64::from(cb_size) + 18 > size
                    || valid_bits == 0
                    || valid_bits > depth
                    || depth % 8 != 0
                    || mask & !0x3ffff != 0
                    || (mask != 0 && mask.count_ones() != u32::from(channels))
                    || subformat[1..]
                        != [
                            0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71,
                        ]
                    || !matches!(subformat[0], 1 | 3)
                    || (subformat[0] == 3 && valid_bits != depth)
                {
                    return Ok(false);
                }
                kind = u16::from(subformat[0]);
            }
            // PCM / IEEE float 的位宽、采样帧大小及字节率必须相互一致；
            // 仅有 fmt/data 块和完整 RIFF 长度不能证明有效音频结构。
            if matches!(kind, 1 | 3) {
                let depth = u16::from_le_bytes([format[14], format[15]]);
                let expected_alignment = u32::from(channels) * u32::from(depth).div_ceil(8);
                if depth == 0
                    || (kind == 3 && !matches!(depth, 32 | 64))
                    || expected_alignment != u32::from(alignment)
                    || rate.checked_mul(expected_alignment) != Some(little32(&format[8..12]))
                {
                    return Ok(false);
                }
                sample_alignment = Some(u64::from(alignment));
            }
            format_seen = true;
        } else if &chunk[..4] == b"data" {
            if !format_seen || sample_alignment.is_some_and(|alignment| size % alignment != 0) {
                return Ok(false);
            }
            data_seen = true;
        }
        reader.seek(SeekFrom::Start(next))?;
    }
    Ok(format_seen && data_seen)
}
