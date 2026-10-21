use super::readers::{bytes, length64};
use crate::control::Control;
use std::io::{self, Read, Seek, SeekFrom};

pub(super) fn valid_flac(
    reader: &mut (impl Read + Seek),
    length: u64,
    control: &Control,
) -> io::Result<bool> {
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
