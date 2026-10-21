use super::moveops::log_filesystem_result;
use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions};
use std::path::Path;

pub struct RootGuard(File);
impl RootGuard {
    pub fn acquire(state: &Path) -> Result<Self> {
        let span = jchtools_foundation::logging::operation_span("fsutil", "task_lock");
        let _entered = span.enter();
        let started = std::time::Instant::now();
        tracing::debug!(event = "filesystem_lock_started", "任务锁获取开始");
        let result: Result<Self> = (|| {
            fs::create_dir_all(state)?;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(state.join("organizer.lock"))?;
            fs2::FileExt::try_lock_exclusive(&file)
                .context("另一个递归解压或目录整理任务正在运行，请先结束它")?;
            Ok(Self(file))
        })();
        log_filesystem_result("lock", started, &result);
        result
    }
}
impl Drop for RootGuard {
    fn drop(&mut self) {
        if let Err(error) = fs2::FileExt::unlock(&self.0) {
            tracing::warn!(
                event = "filesystem_unlock_failed",
                error_type = "io",
                error_code = error.raw_os_error(),
                "任务锁显式释放失败，仍关闭文件句柄"
            );
        }
    }
}
