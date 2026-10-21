use base64::Engine as _;
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use super::cli::{local_bytes, Failure, Input};
use super::download::fetch_image;

fn selected_xberg() -> Result<PathBuf, Failure> {
    let root = jchtools_foundation::xberg_settings::effective_directory()
        .map_err(|error| {
            Failure::new(
                1,
                "runtime",
                format!("读取 JchTools Xberg 设置失败：{error}"),
            )
        })?
        .ok_or_else(|| {
            Failure::new(
                3,
                "xberg",
                "未找到有效的 Xberg。请在 JchTools 设置页检查状态，并从安装包修复随包资产。",
            )
        })?;
    if !root.is_absolute() {
        return Err(Failure::new(
            3,
            "xberg",
            "所选 Xberg 目录无效。请在 JchTools 设置页重新配置 Xberg。",
        ));
    }
    jchtools_xberg::xberg_runtime::validate_assets(&root, "snapshot").map_err(|error| {
        Failure::new(
            3,
            "xberg",
            format!("{error}。请在 JchTools 设置页检查状态，并从安装包修复 Xberg 截图资产。"),
        )
    })?;
    Ok(root)
}

pub(super) fn recognize(input: Input) -> Result<String, Failure> {
    let (root, bytes) = match input {
        Input::Url(url) => {
            let root = selected_xberg()?;
            (root, fetch_image(url)?)
        }
        input => {
            let bytes = local_bytes(input)?;
            if bytes.is_empty() {
                return Err(Failure::input("图片输入为空"));
            }
            (selected_xberg()?, bytes)
        }
    };
    if bytes.is_empty() {
        return Err(Failure::input("图片输入为空"));
    }
    // 显式 CLI 调用可恢复托盘停止后的共享代理，但不启动托盘或 GUI，
    // 也不改变已保存的 Xberg 选择。
    jchtools_xberg::xberg_runtime::resume_background()
        .map_err(|error| Failure::new(1, "runtime", error))?;
    let mut response = jchtools_xberg::xberg_runtime::request(&root,
        json!({"command":"ocr_snapshot","image_base64":base64::engine::general_purpose::STANDARD.encode(&bytes)}),
        Duration::from_secs(600), &AtomicBool::new(false))
        .map_err(|error| Failure::new(1, "runtime", error))?;
    if response["ok"] != true {
        let (code, kind) = match response["error_kind"].as_str() {
            Some("asset_invalid") => {
                return Err(Failure::new(
                    3,
                    "xberg",
                    format!(
                        "{}。请在 JchTools 设置页检查状态，并从安装包修复 Xberg 截图资产。",
                        response["error"].as_str().unwrap_or("Xberg 截图资产不可用")
                    ),
                ));
            }
            Some("input_invalid" | "invalid_request") => (2, "input"),
            _ => (1, "inference"),
        };
        return Err(Failure::new(
            code,
            kind,
            response["error"].as_str().unwrap_or("Xberg 截图推理失败"),
        ));
    }
    if response["error_kind"] == "no_text" {
        return Ok(String::new());
    }
    match response.get_mut("text").map(serde_json::Value::take) {
        Some(serde_json::Value::String(text)) => Ok(text),
        _ => Err(Failure::new(1, "runtime", "Xberg 截图响应缺少 text 字段")),
    }
}
