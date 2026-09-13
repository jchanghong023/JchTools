use crate::{control::Control, fsutil, model::Snapshot};
use anyhow::{bail, Result};
use std::{io::{Read, Seek, SeekFrom}, path::Path, sync::atomic::Ordering};
const BUFFER: usize = 1024 * 1024;
const SAMPLE: usize = 64 * 1024;

pub fn prehash(path: &Path, expected: &Snapshot, ctl: &Control) -> Result<String> {
    ctl.checkpoint()?; fsutil::unchanged(path, expected)?;
    let mut file = fsutil::open_stable_read(path)?;
    let mut buffer = vec![0u8; SAMPLE];
    let mut hash = blake3::Hasher::new();
    hash.update(&expected.size.to_le_bytes());
    let first = expected.size.min(SAMPLE as u64) as usize;
    file.read_exact(&mut buffer[..first])?;
    hash.update(&buffer[..first]);
    ctl.read_bytes.fetch_add(first as u64, Ordering::Relaxed);
    if expected.size > SAMPLE as u64 {
        ctl.checkpoint()?;
        file.seek(SeekFrom::End(-(SAMPLE as i64)))?;
        file.read_exact(&mut buffer)?; hash.update(&buffer);
        ctl.read_bytes.fetch_add(SAMPLE as u64, Ordering::Relaxed);
    }
    fsutil::unchanged(path, expected)?;
    Ok(hash.finalize().to_hex().to_string())
}
/// 本地内容判定只关心「是否相同」，固定 BLAKE3：不选算法、不做兼容外部 Hash 清单；
/// 防误删由删除前的逐字节复核（verify_bytes）承担，不依赖摘要算法强度。
pub fn full_hash(path: &Path, expected: &Snapshot, ctl: &Control) -> Result<String> {
    ctl.checkpoint()?; fsutil::unchanged(path, expected)?;
    let mut file = fsutil::open_stable_read(path)?;
    let mut buffer = vec![0u8; BUFFER];
    let mut hash = blake3::Hasher::new();
    let mut total = 0u64;
    loop {
        ctl.checkpoint()?;
        let count = file.read(&mut buffer)?;
        if count == 0 { break; }
        hash.update(&buffer[..count]);
        total = total.checked_add(count as u64).ok_or_else(|| anyhow::anyhow!("文件大小溢出"))?;
        ctl.read_bytes.fetch_add(count as u64, Ordering::Relaxed);
    }
    if total != expected.size { bail!("文件读取期间大小发生变化"); }
    fsutil::unchanged(path, expected)?;
    Ok(format!("blake3:{}", hash.finalize().to_hex()))
}
pub fn equal_bytes(a: &Path, sa: &Snapshot, b: &Path, sb: &Snapshot, ctl: &Control) -> Result<bool> {
    if sa.size != sb.size { return Ok(false); }
    fsutil::unchanged(a,sa)?; fsutil::unchanged(b,sb)?;
    let mut fa = fsutil::open_stable_read(a)?;
    let mut fb = fsutil::open_stable_read(b)?;
    let mut ba = vec![0u8; BUFFER]; let mut bb = vec![0u8; BUFFER];
    let mut left = sa.size;
    while left > 0 {
        ctl.checkpoint()?;
        let count = left.min(BUFFER as u64) as usize;
        fa.read_exact(&mut ba[..count])?; fb.read_exact(&mut bb[..count])?;
        ctl.read_bytes.fetch_add(count as u64 * 2, Ordering::Relaxed);
        if ba[..count] != bb[..count] { return Ok(false); }
        left -= count as u64;
    }
    // Detect growth, not just changes to the expected prefix.
    let mut extra = [0u8; 1];
    if fa.read(&mut extra)? != 0 || fb.read(&mut extra)? != 0 { bail!("文件比较期间长度发生变化"); }
    fsutil::unchanged(a,sa)?; fsutil::unchanged(b,sb)?;
    Ok(true)
}
