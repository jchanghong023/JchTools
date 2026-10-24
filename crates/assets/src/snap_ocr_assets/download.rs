use crate::asset_util::{ensure_not_cancelled, verify_file_with_cancel, AssetDownloader};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// 下载接缝共用 [`crate::asset_util::AssetDownloader`]；本文件持有唯一的
/// HTTP 下载原语（P-03 联网边界测试按文件白名单执法：`ureq` 只允许出现在
/// `snap_ocr_assets.rs` 与 `markdown_assets.rs`），测试可注入本地供给验证
/// 「补缺下载」与「失败后重试不重下」。
///
/// 生产下载器：只访问清单固定地址（O-06/O-10：仅用户主动初始化联网）。
pub(super) struct NetworkDownloader;

impl AssetDownloader for NetworkDownloader {
    fn download(
        &mut self,
        url: &str,
        destination: &Path,
        expected_size: u64,
        expected_sha256: &str,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(String),
    ) -> Result<(), String> {
        let mut sink = |message: String| progress(message);
        download_asset(
            url,
            destination,
            expected_size,
            expected_sha256,
            cancel,
            &mut sink,
        )
    }
}

/// 流式下载到 `.part` 并按清单摘要校验；下载中断与校验失败均最多重试 3 次
///（C-3：弱网一次中断不作废整包），成功后原子改名落位。
/// markdown 侧的可选组件初始化复用同一原语（T-05/T-21 同口径）。
pub fn download_asset(
    url: &str,
    destination: &Path,
    expected_size: u64,
    expected_sha256: &str,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(String),
) -> Result<(), String> {
    let span = jchtools_foundation::logging::operation_span("snap_ocr_assets", "download_asset");
    let _entered = span.enter();
    let started = std::time::Instant::now();
    tracing::info!(event = "asset_download_started", url = %jchtools_foundation::logging::safe_url(url), size_bytes = expected_size, "资产下载开始");
    let mut stage = "prepare_directory";
    let result = (|| {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                tracing::error!(event = "asset_download_prepare_failed", stage = "prepare_directory", error_kind = ?error.kind(), error_code = error.raw_os_error(), "创建下载目录失败");
                format!("创建下载目录失败：{error}")
            })?;
        }
        // 追加后缀而非替换扩展名，避免目标自身为 .part 时清理误删已安装资产。
        let mut partial = destination.as_os_str().to_os_string();
        partial.push(".part");
        let partial = PathBuf::from(partial);
        cleanup_download_partial(&partial, "prepare_partial");
        stage = "download_verify_install";
        let mut fetch = |partial: &Path, progress: &mut dyn FnMut(String)| {
            download_stream(url, partial, expected_size, cancel, progress)
        };
        download_asset_with(
            destination,
            &partial,
            expected_size,
            expected_sha256,
            cancel,
            progress,
            &mut fetch,
        )
    })();
    let elapsed_ms = jchtools_foundation::logging::elapsed_ms(started);
    match &result {
        Ok(()) => tracing::info!(
            event = "asset_download_completed",
            elapsed_ms,
            bytes = expected_size,
            result = "verified_installed",
            "资产下载完成"
        ),
        Err(_) if cancel.load(std::sync::atomic::Ordering::Acquire) => tracing::info!(
            event = "asset_download_cancelled",
            stage,
            elapsed_ms,
            "资产下载已取消"
        ),
        Err(_) => tracing::error!(
            event = "asset_download_failed",
            stage,
            elapsed_ms,
            "资产下载失败"
        ),
    }
    result
}

fn cleanup_download_partial(partial: &Path, stage: &'static str) {
    if let Err(error) = fs::remove_file(partial) {
        if error.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(event = "asset_partial_cleanup_failed", stage, error_kind = ?error.kind(), error_code = error.raw_os_error(), error = %jchtools_foundation::logging::safe_error(&error.to_string()), "下载临时文件清理失败，保留既有错误处理策略");
        }
    }
}

/// 下载 + 校验重试核心：单次流式拉取经 `fetch` 注入（生产为 HTTP 流式下载
/// [`download_stream`]，测试注入中断脚本），`.part` 命名与目录准备由调用方
/// 承担。重试语义（C-3）：拉取中断与校验失败均最多重试 3 次；第 3 次仍失败
/// 时返回含最后一次错误信息的 Err；取消或最终落位失败也清理 `.part`，保留既有目标。
pub(super) fn download_asset_with<F>(
    destination: &Path,
    partial: &Path,
    expected_size: u64,
    expected_sha256: &str,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(String),
    fetch: &mut F,
) -> Result<(), String>
where
    F: FnMut(&Path, &mut dyn FnMut(String)) -> Result<(), String>,
{
    let started = std::time::Instant::now();
    for attempt in 1..=3 {
        let attempt_span = tracing::info_span!("asset_download_attempt", attempt, max_attempts = 3);
        let _attempt_entered = attempt_span.enter();
        let attempt_started = std::time::Instant::now();
        tracing::info!(event = "asset_attempt_started", attempt, "资产下载尝试开始");
        ensure_not_cancelled(cancel)?;
        if let Err(error) = fetch(partial, progress) {
            // 取消优先于重试语义：用户取消时不发重试提示，直接收场。
            if let Err(cancelled) = ensure_not_cancelled(cancel) {
                cleanup_download_partial(partial, "cancel_fetch_error");
                return Err(cancelled);
            }
            if attempt < 3 {
                tracing::warn!(
                    event = "asset_attempt_retrying",
                    attempt,
                    next_attempt = attempt + 1,
                    stage = "fetch",
                    elapsed_ms = jchtools_foundation::logging::elapsed_ms(attempt_started),
                    "资产下载中断，准备重试"
                );
                progress(format!("下载中断，准备重试（{attempt}/3）：{error}"));
                cleanup_download_partial(partial, "fetch_retry");
                continue;
            }
            tracing::error!(
                event = "asset_attempt_exhausted",
                attempt,
                stage = "fetch",
                elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
                "资产下载失败（重试耗尽）"
            );
            cleanup_download_partial(partial, "fetch_exhausted");
            return Err(format!("下载资产失败：{error}"));
        }
        match verify_file_with_cancel(partial, expected_size, expected_sha256, cancel) {
            Ok(()) => {
                if let Err(error) = ensure_not_cancelled(cancel) {
                    tracing::info!(
                        event = "asset_attempt_cancelled",
                        stage = "before_install",
                        elapsed_ms = jchtools_foundation::logging::elapsed_ms(attempt_started),
                        "资产下载尝试已取消"
                    );
                    cleanup_download_partial(partial, "cancel_before_install");
                    return Err(error);
                }
                if let Err(error) = fs::rename(partial, destination) {
                    tracing::error!(event = "asset_install_failed", stage = "rename", error_kind = ?error.kind(), error_code = error.raw_os_error(), elapsed_ms = jchtools_foundation::logging::elapsed_ms(attempt_started), "下载资产落位失败");
                    cleanup_download_partial(partial, "install_failed");
                    return Err(format!("写入下载资产失败：{error}"));
                }
                tracing::info!(
                    event = "asset_attempt_completed",
                    attempt,
                    elapsed_ms = jchtools_foundation::logging::elapsed_ms(attempt_started),
                    bytes = expected_size,
                    result = "verified_installed",
                    "资产下载校验落位完成"
                );
                return Ok(());
            }
            Err(_) if cancel.load(std::sync::atomic::Ordering::Acquire) => {
                tracing::info!(
                    event = "asset_attempt_cancelled",
                    stage = "verify",
                    elapsed_ms = jchtools_foundation::logging::elapsed_ms(attempt_started),
                    "资产校验尝试已取消"
                );
                cleanup_download_partial(partial, "cancel_verify");
                return Err("用户已取消初始化".to_string());
            }
            Err(error) if attempt < 3 => {
                tracing::warn!(
                    event = "asset_attempt_retrying",
                    attempt,
                    next_attempt = attempt + 1,
                    stage = "verify",
                    elapsed_ms = jchtools_foundation::logging::elapsed_ms(attempt_started),
                    "资产校验失败，准备重试"
                );
                progress(format!("资产校验失败，准备重试（{attempt}/3）：{error}"));
                cleanup_download_partial(partial, "verify_retry");
            }
            Err(error) => {
                tracing::error!(
                    event = "asset_attempt_exhausted",
                    attempt,
                    stage = "verify",
                    elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
                    "资产校验失败（重试耗尽）"
                );
                cleanup_download_partial(partial, "verify_exhausted");
                return Err(format!("下载资产校验失败：{error}"));
            }
        }
    }
    Err("下载资产失败".to_string())
}

fn download_stream(
    url: &str,
    partial: &Path,
    expected_size: u64,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(String),
) -> Result<(), String> {
    // P-09 的 ProxyOverride 按每个实际请求目标匹配，不能沿用初始 URL 的代理
    // 策略处理重定向；每跳失败时仅对该目标执行一次代理→直连回退。
    let proxy = jchtools_foundation::system_proxy::read();
    download_attempt(url, partial, expected_size, cancel, progress, &proxy)
        .map_err(|(message, _)| message)
}

/// P-09：组件下载首轮即跳过证书身份校验；在取消检查前给出明确提示。
pub(super) fn announce_certificate_bypass(progress: &mut dyn FnMut(String)) {
    tracing::warn!(
        event = "asset_tls_verification_bypassed",
        certificate_verification = false,
        "组件下载已跳过服务端证书校验，连接身份不再验证"
    );
    progress("已跳过 TLS 证书校验（连接身份不再验证）".into());
}

/// 响应体读取预算（P-09：下载超时口径）。
///
/// ureq 3 移除了 ureq 2 的单次读超时（`timeout_read`，60 秒无进展即报错），
/// 响应体只有 `timeout_recv_body` 总预算：自响应头收讫起算、不按次重置。
/// 为兼顾两个原有目标——「慢速但持续的大包（约 291MB 推理组件包）不被
/// 提前中断」与「停滞连接最终报错进入既有重试/直连回退」——按清单大小
/// 线性放大预算：60 秒起步、每 MiB 追加 2 秒（约 512 KiB/s 的地板平均
/// 速率）。代价是停滞连接的最长检出时间从 60 秒变为剩余预算；用户取消
/// 仍在每次成功读取之间生效（C-3 弱网重试与取消优先级不变）。
fn download_body_budget(expected_size: u64) -> Duration {
    const MEBIBYTE: u64 = 1024 * 1024;
    let mebibytes = expected_size / MEBIBYTE;
    Duration::from_secs(60_u64.saturating_add(mebibytes.saturating_mul(2)))
}

/// 判断读体错误是否为超时：ureq 3 的超时以 `io::Error(Other)` 包装
/// `ureq::Error::Timeout` 返回，不再体现为 `io::ErrorKind::TimedOut`，
/// 两种形态都视为超时（保持日志 `timed_out` 字段与直连回退判据的口径）。
pub(super) fn is_read_timeout(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ) || error.get_ref().is_some_and(|inner| {
        inner
            .downcast_ref::<ureq::Error>()
            .is_some_and(|error| matches!(error, ureq::Error::Timeout(_)))
    })
}

/// 单次下载尝试：逐跳重新选择系统代理；仅代理传输失败时对当前目标直连重试一次。
/// 重定向上限与既有 ureq 2.12.1 `.redirects(3)` 口径一致（最多跟随两跳）；
/// ureq 3 侧以 `max_redirects(0)` 关闭自动跟随，由本函数逐跳手工处理。
pub(super) fn download_attempt(
    url: &str,
    partial: &Path,
    expected_size: u64,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(String),
    proxy: &jchtools_foundation::system_proxy::SystemProxy,
) -> Result<(), (String, bool)> {
    let mut current_url = url.to_string();
    let mut redirects_followed = 0_u32;
    let mut retry_direct = false;
    let mut proxy_failure = None;
    // ureq 3 的响应体超时是总预算（见 download_body_budget），同一尝试的
    // 代理跳与直连回退跳共用同一预算。
    let recv_body = download_body_budget(expected_size);
    announce_certificate_bypass(progress);
    loop {
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            tracing::info!(
                event = "asset_attempt_cancelled",
                stage = "before_request",
                "资产请求尝试已取消"
            );
            cleanup_download_partial(partial, "cancel_before_request");
            return Err(("用户已取消初始化".to_string(), false));
        }
        let endpoint = if retry_direct {
            None
        } else {
            proxy.endpoint_for_url(&current_url)
        };
        let request_id = jchtools_foundation::logging::new_operation_id();
        let hop_span = tracing::info_span!("asset_http_hop", request_id = %request_id, redirect_hop = redirects_followed, route = if endpoint.is_some() { "proxy" } else if retry_direct { "direct_fallback" } else { "direct" });
        let _hop_entered = hop_span.enter();
        let hop_started = std::time::Instant::now();
        if retry_direct {
            tracing::info!(
                event = "asset_direct_fallback_started",
                stage = "request",
                "资产请求开始直连回退"
            );
        }
        let response = match download_request(
            &current_url,
            endpoint.as_deref(),
            recv_body,
            progress,
            &request_id,
        ) {
            Ok(response) => response,
            Err((proxy_message, true)) if endpoint.is_some() => {
                tracing::warn!(
                    event = "asset_direct_fallback_retrying",
                    stage = "request",
                    elapsed_ms = jchtools_foundation::logging::elapsed_ms(hop_started),
                    url = %jchtools_foundation::logging::safe_url(&current_url),
                    "系统代理连接失败，按 P-09 自动回退直连重试"
                );
                progress("系统代理连接失败，自动回退直连重试".to_string());
                proxy_failure = Some(proxy_message);
                retry_direct = true;
                continue;
            }
            Err((direct_message, _)) => {
                if retry_direct {
                    tracing::error!(
                        event = "asset_direct_fallback_failed",
                        stage = "request",
                        elapsed_ms = jchtools_foundation::logging::elapsed_ms(hop_started),
                        "资产直连回退请求失败"
                    );
                }
                return Err(download_attempt_error(
                    &current_url,
                    &mut proxy_failure,
                    direct_message,
                ));
            }
        };

        let (status, next_url) = resolve_redirect(
            &response,
            &current_url,
            redirects_followed,
            retry_direct,
            hop_started,
            &mut proxy_failure,
        )?;
        if let Some(next_url) = next_url {
            drop(response);
            current_url = next_url.to_string();
            redirects_followed += 1;
            retry_direct = false;
            proxy_failure = None;
            continue;
        }
        if !(200..300).contains(&status) {
            tracing::error!(
                event = "asset_http_status_failed",
                stage = "http_status",
                status,
                "资产请求返回非成功状态"
            );
            return Err(download_attempt_error(
                &current_url,
                &mut proxy_failure,
                format!("下载请求返回 HTTP {status}"),
            ));
        }

        match download_response_body(response, partial, expected_size, cancel, progress) {
            Ok(()) => {
                if retry_direct {
                    tracing::info!(
                        event = "asset_direct_fallback_completed",
                        elapsed_ms = jchtools_foundation::logging::elapsed_ms(hop_started),
                        result = "body_received",
                        "资产直连回退下载完成"
                    );
                }
                return Ok(());
            }
            Err((proxy_message, true)) if endpoint.is_some() => {
                tracing::warn!(
                    event = "asset_direct_fallback_retrying",
                    stage = "body_read",
                    elapsed_ms = jchtools_foundation::logging::elapsed_ms(hop_started),
                    url = %jchtools_foundation::logging::safe_url(&current_url),
                    "系统代理读取失败，按 P-09 自动回退直连重试"
                );
                progress("系统代理连接失败，自动回退直连重试".to_string());
                cleanup_download_partial(partial, "proxy_body_fallback");
                proxy_failure = Some(proxy_message);
                retry_direct = true;
            }
            Err((direct_message, _)) => {
                if retry_direct {
                    tracing::error!(
                        event = "asset_direct_fallback_failed",
                        stage = "body_read",
                        elapsed_ms = jchtools_foundation::logging::elapsed_ms(hop_started),
                        "资产直连回退数据读取失败"
                    );
                }
                return Err(download_attempt_error(
                    &current_url,
                    &mut proxy_failure,
                    direct_message,
                ));
            }
        }
    }
}

/// 处理单跳响应的重定向代数（上限检查、Location 提取、地址解析与跟随日志）。
/// 返回 `(状态码, Some(下一跳地址))`：为 `Some` 时跟随日志已记录，由调用点
/// 更新逐跳状态并 continue；为 `None` 时表示非重定向，调用点按普通响应以
/// 状态码继续分类。返回 `Err` 时失败日志已记录，由调用点直接传播。
fn resolve_redirect(
    response: &ureq::http::Response<ureq::Body>,
    current_url: &str,
    redirects_followed: u32,
    retry_direct: bool,
    hop_started: std::time::Instant,
    proxy_failure: &mut Option<String>,
) -> Result<(u16, Option<url::Url>), (String, bool)> {
    let status = response.status().as_u16();
    // ureq checks the redirect limit before looking up Location, including
    // a third 3xx response without a Location header.
    if (300..400).contains(&status) && redirects_followed + 1 >= 3 {
        tracing::error!(
            event = "asset_redirect_failed",
            stage = "redirect_limit",
            status,
            redirect_hop = redirects_followed,
            "资产请求达到重定向上限"
        );
        return Err(download_attempt_error(
            current_url,
            proxy_failure,
            format!("下载请求失败：达到重定向次数上限（3）：{current_url}"),
        ));
    }
    let location = if matches!(status, 301 | 302 | 303 | 307 | 308) {
        response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    } else {
        None
    };
    if let Some(location) = location {
        // ureq 3 移除了 request_url()（URL 校验/规范化），改用 url crate
        // 完成同等的源地址解析与相对 Location 拼接。
        let request_url = match url::Url::parse(current_url) {
            Ok(url) => url,
            Err(error) => {
                tracing::error!(
                    event = "asset_redirect_failed",
                    stage = "redirect_source_url",
                    "资产重定向源地址无效"
                );
                return Err(download_attempt_error(
                    current_url,
                    proxy_failure,
                    format!("下载请求失败：{error}"),
                ));
            }
        };
        let next_url = match request_url.join(&location) {
            Ok(url) => url,
            Err(error) => {
                tracing::error!(
                    event = "asset_redirect_failed",
                    stage = "redirect_target_url",
                    "资产重定向目标地址无效"
                );
                return Err(download_attempt_error(
                    current_url,
                    proxy_failure,
                    format!("下载重定向地址无效：{error}"),
                ));
            }
        };
        tracing::info!(event = "asset_redirect_followed", status, next_url = %jchtools_foundation::logging::safe_url(next_url.as_str()), next_hop = redirects_followed + 1, "资产请求跟随重定向");
        if retry_direct {
            tracing::info!(
                event = "asset_direct_fallback_completed",
                elapsed_ms = jchtools_foundation::logging::elapsed_ms(hop_started),
                result = "redirect",
                "资产直连回退收到重定向"
            );
        }
        Ok((status, Some(next_url)))
    } else {
        Ok((status, None))
    }
}

fn download_attempt_error(
    url: &str,
    proxy_failure: &mut Option<String>,
    direct_message: String,
) -> (String, bool) {
    if let Some(proxy_message) = proxy_failure.take() {
        proxy_direct_failure(url, &proxy_message, &direct_message)
    } else {
        (direct_message, false)
    }
}

fn proxy_direct_failure(url: &str, proxy_message: &str, direct_message: &str) -> (String, bool) {
    tracing::error!(
        event = "asset_proxy_and_direct_failed",
        url = %jchtools_foundation::logging::safe_url(url),
        result = "proxy_and_direct_failed",
        "系统代理与直连均失败"
    );
    (
        format!("系统代理与直连均失败——系统代理：{proxy_message}；直连：{direct_message}"),
        false,
    )
}

fn download_response_body(
    response: ureq::http::Response<ureq::Body>,
    partial: &Path,
    expected_size: u64,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(String),
) -> Result<(), (String, bool)> {
    let started = std::time::Instant::now();
    tracing::info!(
        event = "asset_body_started",
        expected_size,
        read_timeout_ms =
            u64::try_from(download_body_budget(expected_size).as_millis()).unwrap_or(u64::MAX),
        "资产响应数据读取开始"
    );
    let mut reader = response.into_body().into_reader();
    let mut output = File::create(partial).map_err(|error| {
        tracing::error!(event = "asset_body_failed", stage = "file_create", error_kind = ?error.kind(), error_code = error.raw_os_error(), elapsed_ms = jchtools_foundation::logging::elapsed_ms(started), "创建下载文件失败");
        (format!("创建下载文件失败：{error}"), false)
    })?;
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut current = 0_u64;
    loop {
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            tracing::info!(
                event = "asset_body_cancelled",
                stage = "body_read",
                bytes = current,
                elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
                "资产数据读取已取消"
            );
            drop(output);
            cleanup_download_partial(partial, "cancel_body_read");
            return Err(("用户已取消初始化".to_string(), false));
        }
        let read = reader
            .read(&mut buffer)
            .map_err(|error| {
                tracing::warn!(event = "asset_body_failed", stage = "body_read", error_kind = ?error.kind(), error_code = error.raw_os_error(), timed_out = is_read_timeout(&error), bytes = current, elapsed_ms = jchtools_foundation::logging::elapsed_ms(started), "读取下载数据失败");
                (format!("读取下载数据失败：{error}"), true)
            })?;
        if read == 0 {
            break;
        }
        let next = match checked_download_total(
            current,
            u64::try_from(read).unwrap_or(u64::MAX),
            expected_size,
        ) {
            Ok(next) => next,
            Err(error) => {
                tracing::error!(
                    event = "asset_body_failed",
                    stage = "size_limit",
                    expected_size,
                    bytes = current,
                    elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
                    "资产下载数据超过清单大小"
                );
                drop(output);
                cleanup_download_partial(partial, "body_size_limit");
                return Err((error, false));
            }
        };
        output
            .write_all(&buffer[..read])
            .map_err(|error| {
                tracing::error!(event = "asset_body_failed", stage = "file_write", error_kind = ?error.kind(), error_code = error.raw_os_error(), bytes = current, elapsed_ms = jchtools_foundation::logging::elapsed_ms(started), "写入下载数据失败");
                (format!("写入下载数据失败：{error}"), false)
            })?;
        current = next;
        if expected_size > 0 {
            let percent = current
                .saturating_mul(100)
                .checked_div(expected_size)
                .unwrap_or(100)
                .min(100);
            progress(format!("下载进度：{percent}%"));
        } else {
            progress(format!("下载进度：{current} 字节"));
        }
    }
    output
        .sync_all()
        .map_err(|error| {
            tracing::error!(event = "asset_body_failed", stage = "file_sync", error_kind = ?error.kind(), error_code = error.raw_os_error(), elapsed_ms = jchtools_foundation::logging::elapsed_ms(started), "同步下载文件失败");
            (format!("同步下载文件失败：{error}"), false)
        })?;
    tracing::info!(
        event = "asset_body_completed",
        bytes = current,
        elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
        result = "synced",
        "资产响应数据读取完成"
    );
    Ok(())
}

/// ureq 传输错误的单次分类结果：`stage`/`variant` 供日志聚合，`retryable`
/// 是 P-09 直连回退判据，`timed_out` 对应日志字段，`io_error` 携带底层 IO
/// 错误码来源。合并原先并行的分类表，避免新增错误变体时各表静默漂移。
struct TransportErrorClass<'a> {
    stage: &'static str,
    variant: &'static str,
    retryable: bool,
    timed_out: bool,
    io_error: Option<&'a std::io::Error>,
}

/// 发起一个不自动跟随重定向的 GET；调用方按新 URL 重新应用系统代理策略。
/// `recv_body` 为响应体读取总预算（下载与元数据两路口径不同，见
/// [`download_body_budget`]）。
pub(super) fn download_request(
    url: &str,
    proxy: Option<&str>,
    recv_body: Duration,
    progress: &mut dyn FnMut(String),
    request_id: &str,
) -> Result<ureq::http::Response<ureq::Body>, (String, bool)> {
    let span = tracing::info_span!("asset_http_request", request_id);
    let _entered = span.enter();
    let started = std::time::Instant::now();
    let mut route = if proxy.is_some() { "proxy" } else { "direct" };
    // ureq 3 移植口径：
    // - `redirects(0)` → `max_redirects(0)` + `max_redirects_will_error(false)`，
    //   3xx 响应原样返回，由调用方逐跳处理；
    // - `http_status_as_error(false)`：错误枚举不再携带响应体，4xx/5xx 以
    //   Ok(Response) 返回，状态分类统一在本函数收口；
    // - `timeout_connect`/`timeout_read` → `timeout_resolve`/`timeout_connect`/
    //   `timeout_recv_response`（各 30/30/60 秒，与原连接+单次读超时一致），
    //   读体改为 `timeout_recv_body` 总预算（见 download_body_budget 的差异说明）。
    let config = ureq::Agent::config_builder()
        .max_redirects(0)
        .max_redirects_will_error(false)
        .http_status_as_error(false)
        .timeout_resolve(Some(Duration::from_secs(30)))
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .timeout_recv_body(Some(recv_body))
        .user_agent("JchTools-snap-ocr-assets/1");
    let mut endpoint_proxy = None;
    if let Some(proxy_url) = proxy {
        match ureq::Proxy::new(proxy_url) {
            Ok(parsed) => endpoint_proxy = Some(parsed),
            // 端点字符串由本仓库解析生成，正常不可能非法；异常时按直连
            // 继续，不让代理问题阻塞下载（P-09 可用性优先）。
            Err(error) => {
                tracing::warn!(
                    event = "asset_proxy_invalid",
                    stage = "proxy_parse",
                    fallback = "direct",
                    "系统代理地址无效，本次直连"
                );
                route = "direct_invalid_proxy";
                progress(format!("系统代理地址无法解析（{error}），本次直连"));
            }
        }
    }
    // P-09：两种分支都必须显式写 .proxy(...)。ureq 3 的 Config 默认继承
    // Proxy::try_from_env() 读到的环境代理，逐跳路由只能由本仓库的系统代理解析决定。
    let config = config.proxy(endpoint_proxy);
    // P-09：下载独立 TLS connector 只跳身份检查，TLS 1.2/1.3 签名仍由 rustls 验证。
    // 构造配置失败与握手失败共用下方 TLS 错误分类，不触发代理直连回退。
    let agent = crate::download_tls::agent(config.build());
    tracing::info!(event = "asset_http_request_started", method = "GET", url = %jchtools_foundation::logging::safe_url(url), route, certificate_verification = false, connect_timeout_ms = 30_000, read_timeout_ms = u64::try_from(recv_body.as_millis()).unwrap_or(u64::MAX), automatic_redirects = 0, "资产 GET 请求开始");
    let result = agent.and_then(|agent| agent.get(url).call());
    let elapsed_ms = jchtools_foundation::logging::elapsed_ms(started);
    let response = match result {
        Ok(response) => response,
        Err(error) => {
            // ureq 2 → 3 的传输错误分类等价映射（stage 用于日志聚合）：
            // Dns → HostNotFound；ConnectionFailed 不变；ProxyConnect 与
            // ProxyUnauthorized（CONNECT 建立失败/CONNECT 407）→ ConnectProxyFailed；
            // InvalidUrl/UnknownScheme → Http/BadUri；BadStatus/BadHeader → Protocol。
            // 证书身份失败（旧 tls_certificate 阶段）在跳过证书校验后不会再出现，
            // 其余 rustls 错误按 tls 归类。错误变体名入日志，不记录错误全文
            // （P-10：ConnectProxyFailed 等文本可能携带服务端内容）。
            // P-09 直连回退判据：ureq 2 的 Dns/ConnectionFailed/ProxyConnect/
            // ProxyUnauthorized/Io 在 ureq 3 的等价种类；Timeout 是旧版以 Io
            // 携带的连接/读超时在 ureq 3 拆分出的独立种类，保持可回退。
            let class = match &error {
                ureq::Error::Timeout(_) => TransportErrorClass {
                    stage: "timeout",
                    variant: "Timeout",
                    retryable: true,
                    timed_out: true,
                    io_error: None,
                },
                ureq::Error::HostNotFound => TransportErrorClass {
                    stage: "dns",
                    variant: "HostNotFound",
                    retryable: true,
                    timed_out: false,
                    io_error: None,
                },
                ureq::Error::ConnectionFailed => TransportErrorClass {
                    stage: "connect",
                    variant: "ConnectionFailed",
                    retryable: true,
                    timed_out: false,
                    io_error: None,
                },
                ureq::Error::ConnectProxyFailed(_) => TransportErrorClass {
                    stage: "proxy_connect",
                    variant: "ConnectProxyFailed",
                    retryable: true,
                    timed_out: false,
                    io_error: None,
                },
                ureq::Error::Io(error) => TransportErrorClass {
                    // Io 未入原 stage/variant 表，按通配兜底口径归 transport/Other。
                    stage: "transport",
                    variant: "Other",
                    retryable: true,
                    timed_out: false,
                    io_error: Some(error),
                },
                ureq::Error::InvalidProxyUrl => TransportErrorClass {
                    stage: "proxy_connect",
                    variant: "InvalidProxyUrl",
                    retryable: false,
                    timed_out: false,
                    io_error: None,
                },
                ureq::Error::BadUri(_) => TransportErrorClass {
                    stage: "request_url",
                    variant: "BadUri",
                    retryable: false,
                    timed_out: false,
                    io_error: None,
                },
                ureq::Error::Http(_) => TransportErrorClass {
                    stage: "request_url",
                    variant: "Http",
                    retryable: false,
                    timed_out: false,
                    io_error: None,
                },
                ureq::Error::Rustls(_) => TransportErrorClass {
                    stage: "tls",
                    variant: "Rustls",
                    retryable: false,
                    timed_out: false,
                    io_error: None,
                },
                ureq::Error::Tls(_) => TransportErrorClass {
                    stage: "tls",
                    variant: "Tls",
                    retryable: false,
                    timed_out: false,
                    io_error: None,
                },
                ureq::Error::Protocol(_) => TransportErrorClass {
                    stage: "response_headers",
                    variant: "Protocol",
                    retryable: false,
                    timed_out: false,
                    io_error: None,
                },
                // 通配兜底显式给 retryable=false：新增错误变体不再依赖并行
                // 表的一致性，统一按不可回退的 transport/Other 处理。
                _ => TransportErrorClass {
                    stage: "transport",
                    variant: "Other",
                    retryable: false,
                    timed_out: false,
                    io_error: None,
                },
            };
            tracing::error!(
                event = "asset_http_request_failed",
                stage = class.stage,
                error_type = "Transport",
                error_variant = class.variant,
                error_code = class.io_error.and_then(std::io::Error::raw_os_error),
                elapsed_ms,
                timed_out = class.timed_out,
                retryable = class.retryable,
                "资产 GET 请求传输失败"
            );
            return Err((format!("下载请求失败：{error}"), class.retryable));
        }
    };
    let status = response.status().as_u16();
    if (200..300).contains(&status) || (300..400).contains(&status) {
        tracing::info!(
            event = "asset_http_request_completed",
            status,
            elapsed_ms,
            result = "headers_received",
            "资产 GET 请求收到响应"
        );
        return Ok(response);
    }
    // 其余状态（4xx/5xx）对齐 ureq 2 的 Error::Status 行为：记录失败并转为错误，
    // 调用方无需重复处理。P-09：HTTP 代理认证失败以状态码而非 CONNECT 传输错误
    // 返回；只让实际经代理的 407 进入已有直连回退，普通直连的 407 与其他
    // 4xx/5xx 一样不可重试。
    let retryable = proxy.is_some() && status == 407;
    tracing::error!(
        event = "asset_http_request_failed",
        stage = "http_status",
        error_type = "HttpStatus",
        status,
        elapsed_ms,
        timed_out = false,
        "资产 GET 请求返回错误状态"
    );
    Err((format!("下载请求失败：http status: {status}"), retryable))
}

pub(super) fn checked_download_total(
    current: u64,
    chunk: u64,
    expected_size: u64,
) -> Result<u64, String> {
    let next = current.saturating_add(chunk);
    if expected_size > 0 && next > expected_size {
        Err(format!("下载数据超过清单大小（预期 {expected_size} 字节）"))
    } else {
        Ok(next)
    }
}
