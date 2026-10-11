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
    let operation_id = diagnostic_id(request["diagnostic_id"].as_str())
        .map_or_else(crate::logging::new_operation_id, str::to_owned);
    // 仅在 JchTools 内部消费诊断元数据；外部 Xberg worker 协议保持不变。
    if let Some(fields) = request.as_object_mut() {
        fields.remove("diagnostic_id");
    }
    let span = crate::logging::operation_span_with_id("xberg_runtime", "request", &operation_id);
    let _entered = span.enter();
    let started = std::time::Instant::now();
    let command = diagnostic_command(request["command"].as_str());
    if command == "keepalive" {
        tracing::debug!(
            event = "xberg_request_started",
            command,
            timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            "共享引擎请求开始"
        );
    } else {
        tracing::info!(
            event = "xberg_request_started",
            command,
            timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            "共享引擎请求开始"
        );
    }
    let mut stage = "preflight";
    let result = (|| {
        if cancel.load(Ordering::Acquire) {
            stage = "preflight_cancelled";
            return Err("请求已取消".into());
        }
        if timeout.is_zero() {
            stage = "preflight_timeout";
            return Err("Xberg 请求已超时".into());
        }
        let budget_started = std::time::Instant::now();
        if matches!(
            request["command"].as_str(),
            Some("extract" | "ocr_snapshot" | "transcribe")
        ) {
            stage = "capabilities_handshake";
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
            stage = "capabilities_validate";
            validate_capabilities(
                &capabilities,
                request["command"].as_str().unwrap_or_default(),
            )?;
        }
        stage = "request_budget";
        let timeout = timeout.saturating_sub(budget_started.elapsed());
        if timeout.is_zero() {
            return Err("Xberg 请求已超时（能力查询占用预算）".into());
        }
        stage = "request_id";
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
        stage = "broker_request";
        #[cfg(windows)]
        {
            platform::request(root, &request, timeout, cancel, &operation_id)
        }
        #[cfg(not(windows))]
        {
            let _ = (root, request);
            Err("共享 Xberg 仅支持 Windows".into())
        }
    })();
    let elapsed_ms = crate::logging::elapsed_ms(started);
    match &result {
        Ok(response) if response["ok"] == true => {
            if command == "keepalive" {
                tracing::debug!(
                    event = "xberg_request_completed",
                    command,
                    elapsed_ms,
                    result = "ok",
                    "共享引擎请求完成"
                );
            } else {
                tracing::info!(
                    event = "xberg_request_completed",
                    command,
                    elapsed_ms,
                    result = "ok",
                    "共享引擎请求完成"
                );
            }
        }
        Ok(response) => tracing::warn!(
            event = "xberg_request_failed",
            command,
            elapsed_ms,
            stage = "remote",
            error_kind = diagnostic_error_kind(response),
            "共享引擎返回失败终态"
        ),
        Err(_) => tracing::warn!(
            event = "xberg_request_failed",
            command,
            elapsed_ms,
            stage,
            "共享引擎请求失败"
        ),
    }
    result
}

pub(super) fn diagnostic_id(value: Option<&str>) -> Option<&str> {
    value.filter(|value| {
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

// 只允许协议已知的枚举；引擎字段不能作为任意正文进入诊断日志。
pub(super) fn diagnostic_command(command: Option<&str>) -> &'static str {
    match command {
        Some("extract") => "extract",
        Some("transcribe") => "transcribe",
        Some("ocr_snapshot") => "ocr_snapshot",
        Some("capabilities") => "capabilities",
        Some("formats") => "formats",
        Some("keepalive") => "keepalive",
        Some("snapshot_state") => "snapshot_state",
        Some("cancel") => "cancel",
        Some("broker-state") => "broker_state",
        Some("broker-stop") => "broker_stop",
        Some("broker-force-stop") => "broker_force_stop",
        _ => "unknown",
    }
}

pub(super) fn diagnostic_error_kind(response: &Value) -> &'static str {
    match response["error_kind"].as_str() {
        Some("cancelled" | "canceled") => "cancelled",
        Some("timeout" | "timed_out") => "timeout",
        Some("process_exited") => "process_exited",
        Some("response_too_large") => "response_too_large",
        Some("shared_runtime") => "shared_runtime",
        Some("unsupported") => "unsupported",
        Some("invalid_request") => "invalid_request",
        _ => "remote_error",
    }
}

fn validate_capabilities(capabilities: &Value, command: &str) -> Result<(), String> {
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

/// 启动基线只声明与引擎默认不同的文档场景差异（Markdown 输出、中文 OCR、
/// 图片字节通道），并按模型在位声明媒体转录可用性；截图模型目录交还引擎
/// exe 旁回退，其余等价键一律不下发（2026-10-04 零配置改造）。
pub fn startup_config(root: &Path) -> Result<Value, String> {
    let mut config: Value = serde_json::from_str(include_str!("../resources/markdown-xberg.json"))
        .map_err(|e| e.to_string())?;
    config["transcription"] = json!({"enabled": root.join("models/sense_voice_zh_en_ja_ko_yue_2024_07_17/model.int8.onnx").is_file()});
    Ok(config)
}

/// XB-19：每个场景只要求自己的模型与运行库。公共 EXE/运行库仍按固定清单校验。
#[must_use]
pub fn asset_for_scenario(path: &str, scenario: &str) -> bool {
    if path.starts_with("samples/") || path == "xberg.cmd" {
        return false;
    }
    if path == "models/paddleocr-onnx-models-LICENSE.txt" {
        return matches!(scenario, "document" | "snapshot");
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

    /// 覆盖 O-09/T-06/XB-19：共同 PaddleOCR 许可也是截图必需成员，
    /// 缺失时不可报告就绪；该许可不扩大为媒体场景依赖。
    #[test]
    fn snapshot_requires_shared_paddleocr_license() {
        let root = tempfile::tempdir().expect("创建隔离运行目录");
        let license = "models/paddleocr-onnx-models-LICENSE.txt";
        let manifest: Value =
            serde_json::from_str(include_str!("../resources/markdown-assets.json"))
                .expect("读取共享资产清单");
        for member in manifest["xberg"]["members"].as_array().expect("资产成员") {
            let path = member["path"].as_str().expect("成员路径");
            if path == license || !asset_for_scenario(path, "snapshot") {
                continue;
            }
            let target = root.path().join(path);
            std::fs::create_dir_all(target.parent().expect("成员父目录")).expect("创建成员目录");
            std::fs::write(target, b"synthetic asset").expect("写入存在性夹具");
        }
        let error = validate_assets(root.path(), "snapshot")
            .expect_err("缺少共享 PaddleOCR 许可不得报告截图资产就绪");
        assert!(error.contains(license), "必须明确指出缺失许可：{error}");
        assert!(asset_for_scenario(license, "document"));
        assert!(!asset_for_scenario(license, "media"));
        std::fs::write(root.path().join(license), b"synthetic license")
            .expect("补齐许可存在性夹具");
        validate_assets(root.path(), "snapshot").expect("补齐共同许可后截图资产就绪");
    }

    // 覆盖 T-13（2026-10-04 零配置改造）：启动基线只携带与引擎默认不同的
    // 关键差异——Markdown 输出、中文 OCR、图片字节通道；等价键交还引擎默认，
    // 截图模型目录交还 exe 旁回退。
    #[test]
    fn document_startup_baseline_pins_only_engine_differences() {
        let root = tempfile::tempdir().expect("创建隔离运行目录");
        let config = startup_config(root.path()).expect("生成共享配置");
        assert_eq!(config["output_format"], "markdown");
        assert_eq!(config["ocr"]["language"][0], "ch");
        assert_eq!(config["images"]["include_data_base64"], true);
        assert!(config.get("snapshot_ocr").is_none());
    }

    #[test]
    fn legacy_capabilities_without_handshake_fields_are_rejected() {
        let legacy = json!({
            "ok": true,
            "commands": ["extract", "ocr_snapshot", "cancel"]
        });
        let error = validate_capabilities(&legacy, "extract")
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
