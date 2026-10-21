use super::download::{announce_certificate_bypass, download_request, is_read_timeout};
use crate::asset_util::ensure_not_cancelled;
use std::io::Read;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// 发起一个不自动跟随重定向的 GET；调用方按新 URL 重新应用系统代理策略。
/// XB-10：查询固定发布源的元数据，沿用 P-09 系统代理及一次直连回退。
pub fn fetch_release_json(
    url: &str,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(String),
) -> Result<serde_json::Value, String> {
    let proxy = jchtools_foundation::system_proxy::read();
    fetch_release_json_with_proxy(url, cancel, progress, &proxy)
}

// 测试注入代理策略，仍消费真实 GET 响应、读取边界与 JSON 解析路径。
pub(super) fn fetch_release_json_with_proxy(
    url: &str,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(String),
    proxy: &jchtools_foundation::system_proxy::SystemProxy,
) -> Result<serde_json::Value, String> {
    const MAX_METADATA_BYTES: usize = 2 * 1024 * 1024;
    let span =
        jchtools_foundation::logging::operation_span("snap_ocr_assets", "fetch_release_json");
    let _entered = span.enter();
    let started = std::time::Instant::now();
    tracing::info!(event = "release_metadata_started", url = %jchtools_foundation::logging::safe_url(url), max_bytes = MAX_METADATA_BYTES, "发布元数据查询开始");
    announce_certificate_bypass(progress);
    let endpoint = proxy.endpoint_for_url(url);
    let mut direct = false;
    let mut attempt_number = 0;
    loop {
        attempt_number += 1;
        let request_id = jchtools_foundation::logging::new_operation_id();
        let attempt_span = tracing::info_span!("release_metadata_attempt", attempt = attempt_number, request_id = %request_id, route = if direct { "direct_fallback" } else if endpoint.is_some() { "proxy" } else { "direct" });
        let _attempt_entered = attempt_span.enter();
        let attempt_started = std::time::Instant::now();
        let mut stage = "cancel_check";
        let mut byte_count = 0;
        tracing::info!(
            event = "release_metadata_attempt_started",
            attempt = attempt_number,
            "发布元数据查询尝试开始"
        );
        if direct {
            tracing::info!(
                event = "release_metadata_direct_started",
                "发布元数据开始直连回退"
            );
        }
        let attempt = (|| -> Result<serde_json::Value, (String, bool)> {
            ensure_not_cancelled(cancel).map_err(|error| (error, false))?;
            stage = "request";
            let response = download_request(
                url,
                if direct { None } else { endpoint.as_deref() },
                // 发布元数据上限 2MB，60 秒总预算与既有单次读超时口径相当。
                Duration::from_secs(60),
                progress,
                &request_id,
            )?;
            let status = response.status().as_u16();
            if (300..400).contains(&status) {
                tracing::warn!(
                    event = "release_metadata_redirect_not_followed",
                    stage = "response_status",
                    status,
                    policy = "no_redirects",
                    "发布元数据响应为重定向，按既有策略不跟随并继续解析响应"
                );
            }
            let mut reader = response.into_body().into_reader();
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 8192];
            stage = "body_read";
            tracing::info!(
                event = "release_metadata_body_started",
                read_timeout_ms = 60_000,
                "发布元数据读取开始"
            );
            loop {
                ensure_not_cancelled(cancel).map_err(|error| (error, false))?;
                let read = reader.read(&mut buffer).map_err(|error| {
                    tracing::warn!(event = "release_metadata_read_failed", stage, error_kind = ?error.kind(), error_code = error.raw_os_error(), timed_out = is_read_timeout(&error), elapsed_ms = jchtools_foundation::logging::elapsed_ms(attempt_started), "读取发布元数据失败");
                    (format!("读取发布元数据失败：{error}"), true)
                })?;
                if read == 0 {
                    break;
                }
                if bytes.len().saturating_add(read) > MAX_METADATA_BYTES {
                    stage = "size_limit";
                    return Err(("发布元数据超过大小上限".into(), false));
                }
                bytes.extend_from_slice(&buffer[..read]);
                byte_count = bytes.len();
            }
            ensure_not_cancelled(cancel).map_err(|error| (error, false))?;
            stage = "json_parse";
            tracing::info!(
                event = "release_metadata_body_completed",
                bytes = byte_count,
                elapsed_ms = jchtools_foundation::logging::elapsed_ms(attempt_started),
                "发布元数据读取完成"
            );
            serde_json::from_slice(&bytes).map_err(|error| {
                tracing::error!(event = "release_metadata_parse_failed", stage, error_type = "Json", error_category = ?error.classify(), line = error.line(), column = error.column(), "发布元数据 JSON 解析失败");
                (format!("发布元数据不是有效 JSON：{error}"), false)
            })
        })();
        match attempt {
            Ok(value) => {
                tracing::info!(
                    event = "release_metadata_completed",
                    result = "parsed",
                    bytes = byte_count,
                    direct_fallback = direct,
                    elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
                    "发布元数据查询完成"
                );
                return Ok(value);
            }
            Err((message, true)) if endpoint.is_some() && !direct => {
                tracing::warn!(
                    event = "release_metadata_direct_retrying",
                    stage,
                    next_attempt = attempt_number + 1,
                    elapsed_ms = jchtools_foundation::logging::elapsed_ms(attempt_started),
                    "经系统代理查询失败，直连重试一次"
                );
                progress(format!("经系统代理查询失败，直连重试一次：{message}"));
                direct = true;
            }
            Err((message, _)) => {
                if cancel.load(std::sync::atomic::Ordering::Acquire) {
                    tracing::info!(
                        event = "release_metadata_cancelled",
                        stage,
                        elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
                        "发布元数据查询已取消"
                    );
                } else {
                    tracing::error!(
                        event = "release_metadata_failed",
                        stage,
                        attempts = attempt_number,
                        direct_fallback = direct,
                        exhausted = direct,
                        elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
                        "发布元数据查询失败"
                    );
                }
                return Err(message);
            }
        }
    }
}
