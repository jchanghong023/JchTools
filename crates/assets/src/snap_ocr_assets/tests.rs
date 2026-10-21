    use super::tests_support::{
        capture_diagnostics, diagnostic_http_responses, diagnostic_http_server,
        DiagnosticWorkspace, sha256_bytes,
    };
    use std::fs;
    use std::io::{Read, Write};
    use std::path::Path;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn metadata_http_failure_logs_status_without_url_credentials_or_remote_text() {
        let response = b"HTTP/1.1 503 REMOTE_PRIVATE_DETAIL\r\nContent-Length: 19\r\nConnection: close\r\n\r\nREMOTE_PRIVATE_BODY".to_vec();
        let (address, server) = diagnostic_http_server(response);
        // ureq 3 的 http::Uri 不接受 fragment 组件（解析即失败、请求不会发出），
        // 敏感信息仅保留 userinfo 与 query 两种 URL 形态。
        let url = format!("http://DIAG_PRIVATE_USER:DIAG_PRIVATE_PASSWORD@{address}/release?token=DIAG_PRIVATE_TOKEN");
        let cancel = AtomicBool::new(false);
        let (result, logs) = capture_diagnostics(|| {
            super::fetch_release_json_with_proxy(
                &url,
                &cancel,
                &mut |_| {},
                &jchtools_foundation::system_proxy::SystemProxy::default(),
            )
        });
        server.join().expect("HTTP 状态失败供给线程结束");
        assert!(result
            .expect_err("HTTP 503 必须仍失败")
            .contains("下载请求失败"));
        assert!(logs.contains("asset_http_request_started"), "{logs}");
        assert!(logs.contains("asset_http_request_failed"), "{logs}");
        assert!(
            logs.contains("http_status") && logs.contains("503"),
            "{logs}"
        );
        assert!(logs.contains("release_metadata_failed"), "{logs}");
        assert!(
            logs.contains("operation_id")
                && logs.contains("request_id")
                && logs.contains("elapsed_ms")
                && logs.contains("connect_timeout_ms")
                && logs.contains("read_timeout_ms"),
            "{logs}"
        );
        for secret in [
            "DIAG_PRIVATE_USER",
            "DIAG_PRIVATE_PASSWORD",
            "DIAG_PRIVATE_TOKEN",
            "DIAG_PRIVATE_FRAGMENT",
            "REMOTE_PRIVATE_DETAIL",
            "REMOTE_PRIVATE_BODY",
        ] {
            assert!(
                !logs.contains(secret),
                "敏感数据不得出现在日志：{secret}\n{logs}"
            );
        }
    }

    #[test]
    fn metadata_json_failure_logs_parse_stage_without_response_body() {
        let body = r#"{"password":"JSON_PRIVATE_BODY","broken":}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes();
        let (address, server) = diagnostic_http_server(response);
        let cancel = AtomicBool::new(false);
        let (result, logs) = capture_diagnostics(|| {
            super::fetch_release_json_with_proxy(
                &format!("http://{address}/release"),
                &cancel,
                &mut |_| {},
                &jchtools_foundation::system_proxy::SystemProxy::default(),
            )
        });
        server.join().expect("JSON 失败供给线程结束");
        assert!(result
            .expect_err("非法 JSON 必须仍失败")
            .contains("不是有效 JSON"));
        assert!(logs.contains("release_metadata_body_completed"), "{logs}");
        assert!(
            logs.contains("release_metadata_parse_failed") && logs.contains("json_parse"),
            "{logs}"
        );
        assert!(
            logs.contains("release_metadata_failed") && logs.contains("error_category"),
            "{logs}"
        );
        assert!(!logs.contains("JSON_PRIVATE_BODY"), "{logs}");
    }

    #[test]
    fn metadata_proxy_body_failure_logs_one_direct_fallback_and_parsed_result() {
        // ureq 3 对 HTTP 代理不区分目标协议：明文 HTTP 目标同样先建 CONNECT
        // 隧道。代理供给改为先应答隧道建立，再在隧道内返回声明 1024 字节但
        // 实际截断的响应体，保持「代理正文截断仅回退直连一次」的注入语义。
        let proxy_listener =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("启动代理模拟监听器");
        let proxy_address = proxy_listener
            .local_addr()
            .expect("读取代理模拟监听地址");
        let proxy_server = std::thread::spawn(move || {
            let (mut stream, _) = proxy_listener.accept().expect("接受代理连接");
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .expect("设置代理连接读取超时");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 512];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).expect("消费 CONNECT 请求");
                assert!(read > 0, "CONNECT 请求不得在请求头完成前断开");
                request.extend_from_slice(&buffer[..read]);
            }
            assert!(
                request.starts_with(b"CONNECT "),
                "首跳应为 CONNECT 隧道：{}",
                String::from_utf8_lossy(&request)
            );
            stream
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .expect("应答代理隧道建立");
            request.clear();
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).expect("消费隧道内 GET 请求");
                assert!(read > 0, "隧道内 GET 不得在请求头完成前断开");
                request.extend_from_slice(&buffer[..read]);
            }
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 1024\r\nConnection: close\r\n\r\nPROXY_PRIVATE_TRUNCATED_BODY",
                )
                .expect("发送截断的代理响应");
        });
        let body = r#"{"private":"DIRECT_PRIVATE_RESPONSE"}"#;
        let (direct_address, direct_server) = diagnostic_http_server(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .into_bytes(),
        );
        let proxy = jchtools_foundation::system_proxy::SystemProxy::from_registry_values(
            1,
            Some(&proxy_address.to_string()),
            None,
        );
        let cancel = AtomicBool::new(false);
        let (result, logs) = capture_diagnostics(|| {
            super::fetch_release_json_with_proxy(
                &format!("http://{direct_address}/release"),
                &cancel,
                &mut |_| {},
                &proxy,
            )
        });
        proxy_server.join().expect("代理截断供给线程结束");
        direct_server.join().expect("直连供给线程结束");
        assert_eq!(
            result.expect("直连必须恢复成功")["private"],
            "DIRECT_PRIVATE_RESPONSE"
        );
        assert_eq!(
            logs.matches("asset_http_request_started").count(),
            2,
            "{logs}"
        );
        let failed = logs
            .find("release_metadata_read_failed")
            .expect("记录代理读取失败");
        let retrying = logs
            .find("release_metadata_direct_retrying")
            .expect("记录直连回退原因");
        let direct = logs
            .find("release_metadata_direct_started")
            .expect("记录直连开始");
        let completed = logs
            .find("release_metadata_completed")
            .expect("记录解析成功");
        assert!(
            failed < retrying && retrying < direct && direct < completed,
            "{logs}"
        );
        assert!(
            logs.contains("body_read") && logs.contains("attempt=2") && logs.contains("bytes="),
            "{logs}"
        );
        assert!(
            !logs.contains("PROXY_PRIVATE_TRUNCATED_BODY")
                && !logs.contains("DIRECT_PRIVATE_RESPONSE"),
            "{logs}"
        );
    }

    #[test]
    fn asset_http_failures_log_all_three_attempts_and_exhaustion_without_secrets() {
        let workspace = DiagnosticWorkspace::new();
        let response =
            b"HTTP/1.1 503 ASSET_PRIVATE_REASON\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec();
        let (address, server) =
            diagnostic_http_responses(vec![response.clone(), response.clone(), response]);
        // ureq 3 的 http::Uri 不接受 fragment 组件（解析即失败、请求不会发出），
        // 敏感信息仅保留 query 形态。
        let url = format!("http://{address}/asset?token=ASSET_PRIVATE_TOKEN");
        let destination = workspace.path().join("asset.bin");
        let partial = workspace.path().join("asset.part");
        let cancel = AtomicBool::new(false);
        let mut progress = |_message: String| {};
        let proxy = jchtools_foundation::system_proxy::SystemProxy::default();
        let (result, logs) = capture_diagnostics(|| {
            let span = jchtools_foundation::logging::operation_span("snap_ocr_assets", "download_asset");
            let _entered = span.enter();
            let mut fetch = |partial: &Path, progress: &mut dyn FnMut(String)| {
                super::download_attempt(&url, partial, 1, &cancel, progress, &proxy)
                    .map_err(|(message, _)| message)
            };
            super::download_asset_with(
                &destination,
                &partial,
                1,
                &"00".repeat(32),
                &cancel,
                &mut progress,
                &mut fetch,
            )
        });
        server.join().expect("三次真实 HTTP 失败供给线程结束");
        assert!(result
            .expect_err("三次失败必须耗尽")
            .contains("下载资产失败"));
        assert_eq!(
            logs.matches("asset_http_request_started").count(),
            3,
            "{logs}"
        );
        assert_eq!(
            logs.matches("asset_http_request_failed").count(),
            3,
            "{logs}"
        );
        assert_eq!(logs.matches("asset_attempt_retrying").count(), 2, "{logs}");
        assert_eq!(logs.matches("asset_attempt_exhausted").count(), 1, "{logs}");
        for attempt in ["attempt=1", "attempt=2", "attempt=3"] {
            assert!(logs.contains(attempt), "{logs}");
        }
        assert!(
            logs.contains("http_status")
                && logs.contains("503")
                && logs.contains("operation_id")
                && logs.contains("request_id")
                && logs.contains("elapsed_ms"),
            "{logs}"
        );
        for secret in [
            "ASSET_PRIVATE_REASON",
            "ASSET_PRIVATE_TOKEN",
            "ASSET_PRIVATE_FRAGMENT",
        ] {
            assert!(!logs.contains(secret), "{logs}");
        }
        assert!(
            !destination.exists() && !partial.exists(),
            "失败不落位且删除残留"
        );
    }

    #[test]
    fn checked_download_total_rejects_payload_over_manifest_size() {
        assert_eq!(
            super::checked_download_total(4, 6, 10).expect("边界大小应允许"),
            10
        );
        let error =
            super::checked_download_total(4, 7, 10).expect_err("超过清单大小必须在写盘前拒绝");
        assert!(error.contains("超过清单大小"));
    }

    // 覆盖 O-06/T-06/XB-10：目标自身以 .part 结尾时，取消也不得将其
    // 当成下载临时文件删除；修复前 with_extension("part") 与目标重合。
    #[test]
    fn cancelled_download_preserves_destination_with_part_extension() {
        let root = tempfile::tempdir().expect("创建测试目录");
        let destination = root.path().join("asset.part");
        fs::write(&destination, b"verified-existing").expect("预置已校验资产");

        let error = super::download_asset(
            "https://example.invalid/asset",
            &destination,
            b"verified-existing".len() as u64,
            &sha256_bytes(b"verified-existing"),
            &AtomicBool::new(true),
            &mut |_| {},
        )
        .expect_err("已取消的下载不得发起请求");

        assert!(error.contains("取消"), "{error}");
        assert_eq!(
            fs::read(&destination).expect("取消必须保留原有目标"),
            b"verified-existing"
        );
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    // 覆盖 C-3：拉取中断（网络错误）必须与校验失败一样进入最多 3 次重试，
    // 不得首次中断即整体失败（约 291MB 组件包弱网一次中断即作废、.part 报废）。
    #[test]
    fn download_asset_retries_after_stream_interruption() {
        let root = tempfile::tempdir().expect("创建测试目录");
        let destination = root.path().join("asset.bin");
        let body = b"download-body".to_vec();
        let cancel = AtomicBool::new(false);
        let mut messages = Vec::new();
        let mut progress = |message: String| messages.push(message);
        let mut calls = 0_usize;
        let mut fetch = |partial: &Path, _progress: &mut dyn FnMut(String)| -> Result<(), String> {
            calls += 1;
            if calls < 3 {
                return Err("模拟网络中断".to_string());
            }
            fs::write(partial, &body).map_err(|error| error.to_string())
        };

        super::download_asset_with(
            &destination,
            &destination.with_extension("part"),
            body.len() as u64,
            &sha256_bytes(&body),
            &cancel,
            &mut progress,
            &mut fetch,
        )
        .expect("两次中断后第三次拉取应成功落位");

        assert_eq!(calls, 3, "中断后必须重试");
        assert_eq!(fs::read(&destination).expect("成功后必须原子落位"), body);
        assert_eq!(
            messages
                .iter()
                .filter(|message| message.contains("下载中断，准备重试"))
                .count(),
            2,
            "应恰好发出两条中断重试提示：{messages:?}"
        );
    }

    // 覆盖 C-3：三次拉取全部中断时返回包含最后一次错误的失败，产物与 .part
    // 均不残留。
    #[test]
    fn download_asset_fails_after_three_interruptions() {
        let root = tempfile::tempdir().expect("创建测试目录");
        let destination = root.path().join("asset.bin");
        let cancel = AtomicBool::new(false);
        let mut progress = |_message: String| {};
        let mut calls = 0_usize;
        let mut fetch =
            |_partial: &Path, _progress: &mut dyn FnMut(String)| -> Result<(), String> {
                calls += 1;
                Err("模拟网络中断".to_string())
            };

        let error = super::download_asset_with(
            &destination,
            &destination.with_extension("part"),
            8,
            &"ab".repeat(32),
            &cancel,
            &mut progress,
            &mut fetch,
        )
        .expect_err("三次中断必须失败");

        assert_eq!(calls, 3, "重试次数上限为 3");
        assert!(
            error.contains("模拟网络中断"),
            "失败信息应包含最后一次错误：{error}"
        );
        assert!(!destination.exists(), "失败不得落位产物");
        assert!(
            !destination.with_extension("part").exists(),
            "失败后 .part 必须清理"
        );
    }

    // 覆盖 O-06/T-05：拉取已写入部分数据后同时遇到网络错误与用户取消，
    // 不重试、不覆盖既有资产，并在返回取消前移除 .part。
    #[test]
    fn download_asset_cancellation_during_fetch_error_cleans_partial() {
        let root = tempfile::tempdir().expect("创建测试目录");
        let destination = root.path().join("asset.bin");
        let partial = destination.with_extension("part");
        fs::write(&destination, b"existing").expect("预置既有资产");
        let cancel = AtomicBool::new(false);
        let mut calls = 0;
        let mut messages = Vec::new();
        let error = super::download_asset_with(
            &destination,
            &partial,
            8,
            &"ab".repeat(32),
            &cancel,
            &mut |message| messages.push(message),
            &mut |path, _| {
                calls += 1;
                fs::write(path, b"partial").expect("模拟部分下载");
                cancel.store(true, std::sync::atomic::Ordering::Release);
                Err("模拟网络中断".into())
            },
        )
        .expect_err("取消必须优先于网络重试");
        assert!(error.contains("取消"), "{error}");
        assert_eq!(calls, 1, "取消后不得重试");
        assert!(messages.is_empty(), "取消后不得发重试提示");
        assert!(!partial.exists(), "取消后不得残留部分下载");
        assert_eq!(fs::read(destination).unwrap(), b"existing");
    }

    // 覆盖 O-06/T-05：最终路径不能落位时保留占用项、清理已校验的 .part，
    // 不把写入失败误报为下载成功或继续重试网络。
    #[test]
    fn download_asset_install_failure_cleans_partial_and_preserves_destination() {
        let root = tempfile::tempdir().expect("创建测试目录");
        let destination = root.path().join("asset.bin");
        let partial = destination.with_extension("part");
        fs::create_dir(&destination).expect("用目录阻止文件落位");
        fs::write(destination.join("keep.txt"), b"keep").expect("预置占用项");
        let body = b"download";
        let mut calls = 0;
        let error = super::download_asset_with(
            &destination,
            &partial,
            body.len() as u64,
            &sha256_bytes(body),
            &AtomicBool::new(false),
            &mut |_| {},
            &mut |path, _| {
                calls += 1;
                fs::write(path, body).map_err(|error| error.to_string())
            },
        )
        .expect_err("目录占用必须阻止文件落位");
        assert!(error.contains("写入下载资产失败"), "{error}");
        assert_eq!(calls, 1, "落位失败不得重新下载");
        assert!(!partial.exists(), "落位失败必须清理 .part");
        assert_eq!(fs::read(destination.join("keep.txt")).unwrap(), b"keep");
    }
