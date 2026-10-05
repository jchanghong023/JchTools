//! 截图服务仅是共享引擎的客户端，退出/取消不得结束其他场景。
//!
//! 错误与模型状态类型（O-30 分类、SNAP-17 状态）原属直连子进程客户端模块，
//! 2026-10-04 经用户确认删除旧直连路径（XB-14 唯一共享引擎）后迁入此处。
use base64::Engine as _;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// 客户端错误（O-30 分类：取消 / 推理失败 / 子进程退出 / 通信失败 / 超时）。
#[derive(Debug, Clone)]
pub(crate) enum ClientError {
    /// 用户取消：调用方应随后终止引擎并重载（XB-08）。
    Cancelled,
    /// Xberg 返回的失败响应（`ok:false`；消息不含图像内容）。`kind` 是响应的
    /// 结构化 `error_kind`，与 Xberg `snapshot_ocr.rs` 的取值全集对齐：
    /// `asset_invalid` / `input_invalid` / `no_text` / `cancelled` / `internal`
    /// （SNAP-15）；旧版 Xberg 或非快照失败响应可能缺失（`None`）。注意
    /// `ok:true` + `error_kind:"no_text"` 是无文字图片的成功响应，不走本变体。
    Backend {
        /// 一行用户可读的错误摘要（不含图像内容）。
        message: String,
        /// 结构化错误类别（缺失为 `None`）。
        kind: Option<String>,
    },
    /// 请求级超时：引擎在超时上限内未完成响应。
    Timeout,
    /// 推理进程已退出（携带已知时的退出码）。
    ProcessExited(Option<i32>),
    /// 与引擎的通信失败。
    Io(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => write!(formatter, "用户取消识别"),
            Self::Backend { message, .. } => write!(formatter, "{message}"),
            Self::Timeout => write!(formatter, "识别超时：推理子进程未在超时上限内响应"),
            Self::ProcessExited(code) => match code {
                Some(code) => write!(formatter, "推理子进程已退出（退出码 {code}）"),
                None => write!(formatter, "推理子进程已退出"),
            },
            Self::Io(message) => write!(formatter, "与推理子进程的通信失败：{message}"),
        }
    }
}

impl std::error::Error for ClientError {}

/// `snapshot_state` 报告的截图通道状态（Xberg 侧 SNAP-17）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SnapshotState {
    /// 尚未发起过识别请求（模型懒加载前）。
    Uninitialized,
    /// 模型正在加载（仅在请求处理中可观察）。
    Loading,
    /// 模型已就绪且常驻。
    Ready,
    /// 上次加载失败；携带一行错误摘要。
    Error(String),
}

impl SnapshotState {
    /// O-13 的模型状态字符串（与主程序/管道协议的取值一致）。
    #[must_use]
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Uninitialized => "uninitialized",
            Self::Loading => "loading",
            Self::Ready => "ready",
            Self::Error(_) => "error",
        }
    }
}

fn classify_backend_response(result: &Value) -> Option<ClientError> {
    if result["ok"] == true {
        return None;
    }
    let message = result["error"]
        .as_str()
        .unwrap_or("共享 Xberg 请求失败")
        .to_owned();
    let kind = result["error_kind"].as_str().map(str::to_owned);
    if kind.as_deref() == Some("cancelled") {
        return Some(ClientError::Cancelled);
    }
    if matches!(kind.as_deref(), Some("timeout" | "timed_out"))
        || (kind.as_deref() == Some("shared_runtime") && message.contains("未返回请求终态"))
    {
        return Some(ClientError::Timeout);
    }
    if kind.as_deref() == Some("process_exited") {
        return Some(ClientError::ProcessExited(None));
    }
    Some(ClientError::Backend { message, kind })
}

fn classify_runtime_error(message: String) -> ClientError {
    let lower = message.to_ascii_lowercase();
    // S8-03：请求入口即取消的文案（「请求已取消」，不含超时字样）归入取消，
    // 不得落入 Io 按通信失败处置（那会把用户取消降级成错误并触发模型降级）。
    if message.contains("请求已取消") {
        ClientError::Cancelled
    } else if message.contains("超时")
        || message.contains("未返回请求终态")
        || lower.contains("timeout")
        || lower.contains("timed out")
    {
        ClientError::Timeout
    } else if message.contains("进程已退出")
        || message.contains("响应线程退出")
        || lower.contains("process exited")
    {
        ClientError::ProcessExited(None)
    } else {
        ClientError::Io(message)
    }
}

/// 传输/响应错误与取消标志叠加时的裁决（S8-03）：取消优先于通信与超时
/// 分类——用户意图先于故障归类；子进程退出是引擎真实死亡，不被取消掩盖
/// （服务侧须按退出路径降级模型，而非当作「取消后引擎无恙」）。
fn prioritize_cancel(error: ClientError, cancelled: bool) -> ClientError {
    if cancelled && matches!(error, ClientError::Timeout | ClientError::Io(_)) {
        return ClientError::Cancelled;
    }
    error
}

pub(crate) struct SharedXbergClient {
    root: PathBuf,
}
impl SharedXbergClient {
    pub(crate) fn connect(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }
    fn request(&self, value: Value, cancel: &AtomicBool) -> Result<Value, ClientError> {
        // S8-03：出错路径先查取消标志——取消叠加传输错误/超时类失败响应时
        // 一律按取消处置，不再先分类后查标志（旧序会把「请求入口即取消」
        // 归为 Io、「取消叠加超时」归为超时，偏离取消语义）。
        let result =
            crate::xberg_runtime::request(&self.root, value, Duration::from_secs(600), cancel)
                .map_err(|message| {
                    prioritize_cancel(
                        classify_runtime_error(message),
                        cancel.load(Ordering::Acquire),
                    )
                })?;
        if let Some(error) = classify_backend_response(&result) {
            return Err(prioritize_cancel(error, cancel.load(Ordering::Acquire)));
        }
        if cancel.load(Ordering::Acquire) {
            return Err(ClientError::Cancelled);
        }
        Ok(result)
    }
    pub(crate) fn recognize(
        &mut self,
        png: &[u8],
        cancel: &AtomicBool,
    ) -> Result<Option<String>, ClientError> {
        let response = self.request(json!({"command":"ocr_snapshot","image_base64":base64::engine::general_purpose::STANDARD.encode(png)}), cancel)?;
        if response["error_kind"] == "no_text" {
            return Ok(None);
        }
        let text = response["text"]
            .as_str()
            .ok_or_else(|| ClientError::Io("截图响应缺少 text 字段".into()))?;
        Ok((!text.trim().is_empty()).then(|| text.to_owned()))
    }
    pub(crate) fn snapshot_state(&mut self) -> Result<SnapshotState, ClientError> {
        let response =
            self.request(json!({"command":"snapshot_state"}), &AtomicBool::new(false))?;
        Ok(match response["state"].as_str() {
            Some("ready") => SnapshotState::Ready,
            Some("loading") => SnapshotState::Loading,
            Some("uninitialized") => SnapshotState::Uninitialized,
            _ => SnapshotState::Error(
                response["error"]
                    .as_str()
                    .unwrap_or("未知截图模型状态")
                    .into(),
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{classify_backend_response, classify_runtime_error, ClientError};
    use serde_json::json;

    #[test]
    fn preserves_asset_invalid_response_kind() {
        let error = classify_backend_response(&json!({
            "ok": false,
            "error_kind": "asset_invalid",
            "error": "snapshot model asset missing"
        }))
        .expect("失败响应必须分类");
        assert!(matches!(
            error,
            ClientError::Backend {
                kind: Some(kind),
                ..
            } if kind == "asset_invalid"
        ));
    }

    #[test]
    fn classifies_unresolved_broker_timeout_as_timeout() {
        let error = classify_backend_response(&json!({
            "ok": false,
            "error_kind": "shared_runtime",
            "error": "共享 Xberg 未返回请求终态；未终止其他任务"
        }))
        .expect("失败响应必须分类");
        assert!(matches!(error, ClientError::Timeout));
    }

    #[test]
    fn classifies_runtime_process_and_timeout_errors() {
        assert!(matches!(
            classify_runtime_error("Xberg 请求已超时".into()),
            ClientError::Timeout
        ));
        assert!(matches!(
            classify_runtime_error("共享 Xberg 进程已退出".into()),
            ClientError::ProcessExited(None)
        ));
    }

    // 覆盖 S8-03：请求入口即取消时，传输层错误文案是「请求已取消」（不含
    // 超时字样）——必须归入取消分类，不得落入 Io 按通信失败处置（那会把
    // 用户取消降级成错误并触发模型降级）。
    #[test]
    fn entry_cancel_message_classifies_as_cancelled() {
        assert!(
            matches!(
                classify_runtime_error("请求已取消".into()),
                ClientError::Cancelled
            ),
            "「请求已取消」必须归入取消分类（S8-03：取消优先于通信分类）"
        );
    }

    // 覆盖 S8-03：取消标志叠加传输/超时类错误时，取消优先——「取消叠加超时」
    // 不得按超时处置；引擎真实退出不被取消掩盖；未取消时保持原分类。
    #[test]
    fn cancel_flag_overrides_timeout_and_io_classification() {
        use super::prioritize_cancel;
        assert!(matches!(
            prioritize_cancel(ClientError::Timeout, true),
            ClientError::Cancelled
        ));
        assert!(matches!(
            prioritize_cancel(ClientError::Io("管道断开".into()), true),
            ClientError::Cancelled
        ));
        assert!(matches!(
            prioritize_cancel(ClientError::Timeout, false),
            ClientError::Timeout
        ));
        assert!(matches!(
            prioritize_cancel(ClientError::Io("管道断开".into()), false),
            ClientError::Io(_)
        ));
        assert!(
            matches!(
                prioritize_cancel(ClientError::ProcessExited(None), true),
                ClientError::ProcessExited(_)
            ),
            "引擎真实死亡不被取消标志掩盖"
        );
        assert!(
            matches!(
                prioritize_cancel(
                    ClientError::Backend {
                        message: "推理失败".into(),
                        kind: None
                    },
                    true
                ),
                ClientError::Backend { .. }
            ),
            "引擎侧推理失败响应不受取消标志改写"
        );
    }
}
