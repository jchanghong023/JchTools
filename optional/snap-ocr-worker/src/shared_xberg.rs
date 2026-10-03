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
        let result =
            crate::xberg_runtime::request(&self.root, value, Duration::from_secs(600), cancel)
                .map_err(ClientError::Io)?;
        match result["error_kind"].as_str() {
            Some("cancelled") => return Err(ClientError::Cancelled),
            Some("timeout" | "timed_out") => return Err(ClientError::Timeout),
            Some("process_exited") => return Err(ClientError::ProcessExited(None)),
            _ => {}
        }
        if result["ok"] != true {
            return Err(ClientError::Backend {
                message: result["error"]
                    .as_str()
                    .unwrap_or("共享 Xberg 请求失败")
                    .to_owned(),
                kind: result["error_kind"].as_str().map(str::to_owned),
            });
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
