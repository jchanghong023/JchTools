//! 集成测试共享 mock：core.rs 与 archive.rs 复用，避免重复定义。
// 每个集成测试二进制单独编译本模块：未被该二进制使用的 mock 会报 dead_code，
// 属共享模块的预期形态，不是遗漏。
// [quality-baseline approved 2026-09-19] pub 化实验证实不可消除（rustc 按二进制分析死代码）
#![allow(dead_code)]
use jchtools::platform::{RecycleFailure, Recycler};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

/// 总是失败的回收站 mock：模拟回收站容量满等失败场景。
pub(crate) struct FailRecycle;
impl Recycler for FailRecycle {
    fn recycle(&self, _: &Path) -> Result<(), RecycleFailure> {
        Err(RecycleFailure::Failed("mock capacity full".into()))
    }
}

/// 把回收对象移入指定「假回收站」目录：断言不污染真实回收站，且可计数。
pub(crate) struct MoveRecycle {
    pub bin: PathBuf,
    pub bin_count: AtomicUsize,
}
impl MoveRecycle {
    pub(crate) fn new(bin: PathBuf) -> Self {
        let _ = fs::create_dir_all(&bin);
        Self {
            bin,
            bin_count: AtomicUsize::new(0),
        }
    }
}
impl Recycler for MoveRecycle {
    fn recycle(&self, path: &Path) -> Result<(), RecycleFailure> {
        let name = path
            .file_name()
            .map(std::ffi::OsStr::to_os_string)
            .unwrap_or_default();
        let dest = self.bin.join(format!(
            "{}-{}",
            self.bin_count.fetch_add(1, Ordering::SeqCst),
            name.to_string_lossy()
        ));
        fs::rename(path, dest).map_err(|e| RecycleFailure::Failed(e.to_string()))?;
        Ok(())
    }
}
