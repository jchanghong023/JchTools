//! XB-14～XB-17：所有调用通过当前用户会话的唯一代理访问一个 Xberg worker。
//! 代理独立于 GUI / 截图服务；客户端断开不关闭引擎 stdin，不终止其他请求。
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static NEXT: AtomicU64 = AtomicU64::new(1);

#[cfg(windows)]
#[path = "xberg_runtime_windows.rs"]
mod platform;

/// XB-22/XB-23：新 GUI 会话允许启动；托盘退出后当前会话不自动复活。
pub fn resume_background() -> Result<(), String> {
    #[cfg(windows)]
    {
        platform::resume_background()
    }
    #[cfg(not(windows))]
    {
        Ok(())
    }
}
pub fn background_allowed() -> Result<(), String> {
    #[cfg(windows)]
    {
        platform::background_allowed()
    }
    #[cfg(not(windows))]
    {
        Err("后台服务仅支持 Windows".into())
    }
}
/// 查询/退出代理不启动缺失的引擎；仅用于应用拥有的后台生命周期。
pub fn background_control(stop: bool) -> Result<Value, String> {
    #[cfg(windows)]
    {
        platform::background_control(stop)
    }
    #[cfg(not(windows))]
    {
        let _ = stop;
        Err("后台服务仅支持 Windows".into())
    }
}

/// O-16/XB-23：仅用户显式强退且没有其他场景任务时终结本应用引擎。
pub fn force_background_exit() -> Result<Value, String> {
    #[cfg(windows)]
    {
        platform::force_background_exit()
    }
    #[cfg(not(windows))]
    {
        Err("后台服务仅支持 Windows".into())
    }
}

pub fn serve() -> Result<(), String> {
    #[cfg(windows)]
    {
        platform::serve()
    }
    #[cfg(not(windows))]
    {
        Err("共享 Xberg 仅支持 Windows".into())
    }
}

/// 每次请求独立连接代理，响应使用全局唯一 ID；图像与正文仅存在于内存。
pub fn request(
    root: &Path,
    mut request: Value,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<Value, String> {
    if cancel.load(Ordering::Acquire) {
        return Err("请求已取消".into());
    }
    if timeout.is_zero() {
        return Err("Xberg 请求已超时".into());
    }
    let started = std::time::Instant::now();
    if matches!(
        request["command"].as_str(),
        Some("extract" | "ocr_snapshot" | "transcribe")
    ) {
        let capabilities = self::request(
            root,
            json!({"command":"capabilities"}),
            timeout.min(Duration::from_secs(15)),
            cancel,
        )
        .and_then(checked)
        .map_err(|error| {
            format!("Xberg 共享接口不可用：能力握手失败（不会启动备用引擎）：{error}")
        })?;
        validate_capabilities(
            &capabilities,
            request["command"].as_str().unwrap_or_default(),
            request["mode"] == "fast",
        )?;
    }
    let timeout = timeout.saturating_sub(started.elapsed());
    if timeout.is_zero() {
        return Err("Xberg 请求已超时（能力查询占用预算）".into());
    }
    let id = format!(
        "{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    request["id"] = json!(id);
    request["timeout_ms"] = json!(u64::try_from(timeout.as_millis())
        .unwrap_or(u64::MAX)
        .max(1));
    #[cfg(windows)]
    {
        platform::request(root, &request, timeout, cancel)
    }
    #[cfg(not(windows))]
    {
        let _ = (root, request);
        Err("共享 Xberg 仅支持 Windows".into())
    }
}

fn validate_capabilities(capabilities: &Value, command: &str, fast: bool) -> Result<(), String> {
    let supports = |name: &str| {
        capabilities["commands"]
            .as_array()
            .is_some_and(|commands| commands.iter().any(|command| command == name))
    };
    if capabilities["protocol_version"].as_u64().unwrap_or(0) < 2
        || capabilities["cancellation"] != "cooperative"
        || capabilities["timeout_ms"] != true
        || capabilities["document_snapshot_concurrent"] != true
        || !supports("cancel")
        || !supports(command)
    {
        return Err("Xberg 共享接口阻塞：需要协议 v2、跨场景并发、请求级取消和超时；当前引擎不满足，未提交推理".into());
    }
    if fast
        && !capabilities["extract_modes"]
            .as_array()
            .is_some_and(|modes| modes.iter().any(|mode| mode == "fast"))
    {
        return Err("Xberg 共享接口阻塞：不支持请求级快速模式，未改用常规模式".into());
    }
    Ok(())
}

pub fn checked(response: Value) -> Result<Value, String> {
    if response["ok"] == true {
        return Ok(response);
    }
    Err(format!(
        "Xberg {}：{}",
        response["error_kind"].as_str().unwrap_or("请求失败"),
        response["error"].as_str().unwrap_or("无详细信息")
    ))
}

/// 启动时同时声明三条独立能力，文件级快速模式由请求选择，不改全局配置。
pub fn startup_config(root: &Path) -> Result<Value, String> {
    let mut config: Value = serde_json::from_str(include_str!("../resources/markdown-xberg.json"))
        .map_err(|e| e.to_string())?;
    config["snapshot_ocr"] = json!({"models_dir": root.join("models/snapshot-ocr")});
    config["transcription"] = json!({"enabled": root.join("models/sense_voice_zh_en_ja_ko_yue_2024_07_17/model.int8.onnx").is_file()});
    Ok(config)
}

/// XB-19：每个场景只要求自己的模型与运行库。公共 EXE/运行库仍按固定清单校验。
#[must_use]
pub fn asset_for_scenario(path: &str, scenario: &str) -> bool {
    if path.starts_with("samples/") || path == "xberg.cmd" {
        return false;
    }
    if path.starts_with("models/snapshot-ocr/") {
        return scenario == "snapshot";
    }
    if path.starts_with("models/sense_voice_")
        || path.starts_with("models/vad/")
        || path.starts_with("sherpa-onnx/")
        || path.starts_with("ffmpeg/")
    {
        return scenario == "media";
    }
    if path.starts_with("models/") {
        return scenario == "document";
    }
    true
}

/// 运行时对共享 Xberg 目录的场景成员检查（XB-19；XB-09 2026-10-02 修订后
/// 口径）：只检查该场景所需成员是否在场，不比对大小与 SHA-256——用户可
/// 自行替换或更新引擎版本；成员缺失时明确报错指认，不冒称就绪。截图服务
/// 独立启动时仍走同一检查，不能因绕过 GUI 而只检查同名文件。
pub fn validate_assets(root: &Path, scenario: &str) -> Result<(), String> {
    let manifest: Value = serde_json::from_str(include_str!("../resources/markdown-assets.json"))
        .map_err(|e| e.to_string())?;
    for member in manifest["xberg"]["members"]
        .as_array()
        .ok_or("Xberg 固定清单缺少成员")?
    {
        let path = member["path"].as_str().ok_or("Xberg 固定清单路径无效")?;
        if !asset_for_scenario(path, scenario) {
            continue;
        }
        if !root.join(path).is_file() {
            return Err(format!("Xberg 资产 {path} 缺失（目录 {}）", root.display()));
        }
    }
    Ok(())
}

#[cfg(any(test, feature = "test-hooks"))]
#[cfg(test)]
mod tests {
    use super::*;

    // 覆盖 T-13/T-14：共享 worker 必须渲染 Markdown，否则 PPTX 图片占位和
    // 跟随占位的 OCR 正文会被纯文本模式吞掉，得到被误报成功的空产物。
    #[test]
    fn document_startup_uses_markdown_and_keeps_image_ocr() {
        let root = tempfile::tempdir().expect("创建隔离运行目录");
        let config = startup_config(root.path()).expect("生成共享配置");
        assert_eq!(config["output_format"], "markdown");
        assert_eq!(config["images"]["extract_images"], true);
        assert_eq!(config["images"]["inject_placeholders"], true);
        assert_eq!(config["images"]["run_ocr_on_images"], true);
        assert_eq!(config["images"]["append_ocr_text"], true);
        assert_eq!(config["images"]["include_data_base64"], true);
    }

    #[test]
    fn legacy_capabilities_without_handshake_fields_are_rejected() {
        let legacy = json!({
            "ok": true,
            "commands": ["extract", "ocr_snapshot", "cancel"]
        });
        let error = validate_capabilities(&legacy, "extract", false)
            .expect_err("缺少能力握手字段不得按旧成员清单放行");
        assert!(error.contains("协议 v2"));
    }

    #[test]
    fn unsupported_capabilities_response_is_explicitly_rejected() {
        let error = checked(json!({
            "ok": false,
            "error": "unsupported command 'capabilities'"
        }))
        .expect_err("旧 worker 不支持 capabilities 时必须明确失败");
        assert!(error.contains("unsupported command 'capabilities'"));
    }
}
