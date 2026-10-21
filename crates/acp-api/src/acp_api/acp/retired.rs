use super::{error, FutureExt, ServiceError, ServiceErrorKind};

// 每次后台被正常命令/轮次唤醒时，仅收割已完成项；不等待活终端，不另分配完成列表。
// retain_mut 保持退休顺序，首个收尾错误只缓存一份，交由最终 Stop 完整 drain 交付。
pub(super) fn reap_retired(
    retired: &mut Vec<tokio::task::JoinHandle<Result<(), agent_client_protocol::Error>>>,
    first_failure: &mut Option<ServiceError>,
) {
    retired.retain_mut(|task| {
        if task.is_finished() {
            if let Some(result) = task.now_or_never() {
                if !matches!(result, Ok(Ok(()))) && first_failure.is_none() {
                    *first_failure =
                        Some(error(ServiceErrorKind::Io, "ACP 旧回调或终端资源收尾失败"));
                }
                return false;
            }
        }
        true
    });
}

pub(super) async fn finish_retired(
    retired: &mut Vec<tokio::task::JoinHandle<Result<(), agent_client_protocol::Error>>>,
    first_failure: &mut Option<ServiceError>,
) -> Result<(), ServiceError> {
    crate::acp_api::diagnostics::async_call("acp_agent", "finish_retired", async {
        let mut result = first_failure.take().map_or(Ok(()), Err);
        for task in retired.drain(..) {
            if !matches!(task.await, Ok(Ok(()))) && result.is_ok() {
                result = Err(error(ServiceErrorKind::Io, "ACP 旧回调或终端资源收尾失败"));
            }
        }
        result
    })
    .await
}
