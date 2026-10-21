//! 合并与拆分共用的任务进度事件与逐项结果日志（U-03/U-12/P-10）。
use anyhow::Result;

use jchtools_file_core::control::Control;

pub(super) fn log_md_item_result(
    operation: &'static str,
    stage: &'static str,
    started: std::time::Instant,
    result: &Result<()>,
    control: &Control,
) {
    let elapsed_ms = jchtools_foundation::logging::elapsed_ms(started);
    match result {
        Ok(()) => tracing::debug!(
            event = "md_item_completed",
            operation,
            elapsed_ms,
            "MD处理项完成"
        ),
        Err(_) if control.is_cancelled() => tracing::info!(
            event = "md_item_cancelled",
            operation,
            elapsed_ms,
            "MD处理项已取消"
        ),
        Err(error) => {
            let io = error
                .chain()
                .find_map(|cause| cause.downcast_ref::<std::io::Error>());
            tracing::error!(
                event = "md_item_failed",
                operation,
                stage,
                elapsed_ms,
                error_type = if io.is_some() {
                    "io"
                } else {
                    "md_item_boundary"
                },
                error_code = io.and_then(std::io::Error::raw_os_error),
                "MD处理项失败"
            );
        }
    }
}

/// MD 任务的单项进度事件（U-03/U-12）：`FileStarted` 在开始处理某项之前发出，
/// `FileCompleted` 在该项全部写完之后发出；界面的完成计数只应由 `FileCompleted` 驱动，
/// 否则单文件任务开局就会显示 1/1（100%）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MdProgress {
    /// 第 `index` 项（1 起）开始处理；`total` 为本次任务的总项数（合并=文件数，拆分=分片数）
    FileStarted(usize, usize),
    /// 第 `index` 项已全部写完
    FileCompleted(usize, usize),
}
