use anyhow::Result;

pub(super) fn observe_archive<T>(
    operation: &'static str,
    control: &jchtools_file_core::control::Control,
    run: impl FnOnce(&mut &'static str) -> Result<T>,
) -> Result<T> {
    let span = jchtools_foundation::logging::operation_span("archive", operation);
    let _entered = span.enter();
    let started = std::time::Instant::now();
    let mut stage = "archive_boundary";
    tracing::info!(
        event = "archive_operation_started",
        operation,
        "归档操作开始"
    );
    let result = run(&mut stage);
    let elapsed_ms = jchtools_foundation::logging::elapsed_ms(started);
    match &result {
        Ok(_) => tracing::info!(
            event = "archive_operation_completed",
            operation,
            stage,
            elapsed_ms,
            "归档操作完成"
        ),
        Err(_) if control.is_cancelled() => tracing::info!(
            event = "archive_operation_cancelled",
            operation,
            stage,
            elapsed_ms,
            "归档操作已取消"
        ),
        Err(error) => {
            let io = error
                .chain()
                .find_map(|cause| cause.downcast_ref::<std::io::Error>());
            tracing::error!(
                event = "archive_operation_failed",
                operation,
                stage,
                elapsed_ms,
                error_type = if io.is_some() {
                    "io"
                } else if stops_extraction(error) {
                    "extraction_stop"
                } else {
                    "archive_boundary"
                },
                error_code = io.and_then(std::io::Error::raw_os_error),
                "归档操作失败"
            );
        }
    }
    result
}
pub(super) fn member_rejected(code: &'static str, member: &str) {
    tracing::error!(event = "archive_member_rejected", stage = "member_validation",
        error_type = "archive_validation", error_code = code,
        member = %jchtools_foundation::logging::safe_error(member), "归档成员安全校验拒绝");
}

/// 任务级中止信号（X-05/X-06/X-08）：空间不足、以及成功原包/分卷删除失败都不是包
/// 损坏——保留当前源包（含未删除的分卷）、报错并停止整个解压任务，不隔离本包，也不
/// 继续处理后续包。错误在解压回调里会被 `with_context` 包装，判定走错误链而不是顶层
/// downcast（见 [`stops_extraction`]）。
#[derive(Debug)]
pub(super) struct StopExtraction(pub(super) String);

impl std::fmt::Display for StopExtraction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StopExtraction {}

/// 错误链里是否带「停止任务」信号（空间不足无法继续）：命中即整次解压中止，
/// 不做隔离处置。
pub(super) fn stops_extraction(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<StopExtraction>().is_some()
            || cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| matches!(error.raw_os_error(), Some(112 | 39)))
    })
}
