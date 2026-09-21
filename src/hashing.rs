use crate::{control::Control, convert, fsutil, model::Snapshot};
use anyhow::{bail, Result};
use std::{io::Read, path::Path, sync::atomic::Ordering};
const BUFFER: usize = 1024 * 1024;

/// 本地内容判定只关心「是否相同」，固定 BLAKE3：不选算法、不做兼容外部 Hash 清单；
/// 单个文件在一次整理中只计算一次完整哈希（C-12），不再有预哈希采样阶段。
/// P-08：假定处理期间文件不被其他程序改动，故不做读取前后的快照比对。
pub fn full_hash(path: &Path, expected: &Snapshot, ctl: &Control) -> Result<String> {
    ctl.checkpoint()?;
    let mut file = fsutil::open_stable_read(path)?;
    let mut buffer = vec![0u8; BUFFER];
    let mut hash = blake3::Hasher::new();
    let mut total = 0u64;
    loop {
        ctl.checkpoint()?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        total = total
            .checked_add(count as u64)
            .ok_or_else(|| anyhow::anyhow!("文件大小溢出"))?;
        ctl.read_bytes.fetch_add(count as u64, Ordering::Relaxed);
    }
    if total != expected.size {
        bail!("读取的长度与扫描记录不一致");
    }
    Ok(format!("blake3:{}", hash.finalize().to_hex()))
}
/// 逐字节比较两个文件是否相同（解压冲突裁决用：内容相同的目标视为已合入）。
/// 去重删除路径不使用本函数（S-03/C-12：删除前不重读文件内容）。
/// P-08：假定比较期间两侧都不被其他程序改动，故不做快照比对与二次增长检查。
pub fn equal_bytes(
    a: &Path,
    sa: &Snapshot,
    b: &Path,
    sb: &Snapshot,
    ctl: &Control,
) -> Result<bool> {
    if sa.size != sb.size {
        return Ok(false);
    }
    let mut fa = fsutil::open_stable_read(a)?;
    let mut fb = fsutil::open_stable_read(b)?;
    let mut ba = vec![0u8; BUFFER];
    let mut bb = vec![0u8; BUFFER];
    let mut left = sa.size;
    while left > 0 {
        ctl.checkpoint()?;
        let count = convert::u64_as_usize(left.min(BUFFER as u64));
        fa.read_exact(&mut ba[..count])?;
        fb.read_exact(&mut bb[..count])?;
        ctl.read_bytes
            .fetch_add(count as u64 * 2, Ordering::Relaxed);
        if ba[..count] != bb[..count] {
            return Ok(false);
        }
        left -= count as u64;
    }
    Ok(true)
}
