use crate::control::Control;
use anyhow::Result;
use std::{fs::File, io::Read, path::Path, sync::atomic::Ordering};
const BUFFER: usize = 1024 * 1024;

// 哈希读缓冲的线程局部复用：并行哈希的每个工作线程按文件反复到达 full_hash，
// 逐文件分配/释放 1MB 大块（Windows 堆走直接分配）只是纯开销。缓冲随线程存活、
// 长度恒为 BUFFER；本函数是线程内唯一使用者且读循环串行，take/归还式借用即可。
thread_local! {
    static READ_BUFFER: std::cell::RefCell<Vec<u8>> =
        std::cell::RefCell::new(vec![0u8; BUFFER]);
}
fn with_read_buffer<R>(f: impl FnOnce(&mut [u8]) -> Result<R>) -> Result<R> {
    READ_BUFFER.with(|slot| f(&mut slot.borrow_mut()[..]))
}

/// 本地内容判定只关心「是否相同」，固定 BLAKE3：不选算法、不做兼容外部 Hash 清单；
/// 单个文件在一次整理中只计算一次完整哈希（C-12），不再有预哈希采样阶段。
/// P-08：假定处理期间文件不被其他程序改动，故不做读取前后的快照比对。
pub fn full_hash(path: &Path, ctl: &Control) -> Result<String> {
    ctl.checkpoint()?;
    let mut file = File::open(path)?;
    let mut hash = blake3::Hasher::new();
    let hashed = with_read_buffer(|buffer| {
        loop {
            ctl.checkpoint()?;
            let count = file.read(buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
            ctl.read_bytes.fetch_add(count as u64, Ordering::Relaxed);
        }
        Ok(())
    });
    hashed?;
    Ok(format!("blake3:{}", hash.finalize().to_hex()))
}
