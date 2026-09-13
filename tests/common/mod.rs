//! 集成测试共享 mock：core.rs 与 archive.rs 复用，避免重复定义。
use jchtools::platform::{RecycleFailure, Recycler};
use std::path::Path;

/// 总是失败的回收站 mock：模拟回收站容量满等失败场景。
pub struct FailRecycle;
impl Recycler for FailRecycle {
    fn recycle(&self, _: &Path) -> Result<(), RecycleFailure> {
        Err(RecycleFailure::Failed("mock capacity full".into()))
    }
}
