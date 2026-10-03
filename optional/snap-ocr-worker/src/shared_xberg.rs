//! 截图服务仅是共享引擎的客户端，退出/取消不得结束其他场景。
use crate::xberg_worker::{ClientError, SnapshotState};
use base64::Engine as _;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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
