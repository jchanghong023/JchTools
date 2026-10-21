    use super::tests_support::{
        capture_diagnostics, diagnostic_http_server, diagnostic_tls_server,
        DiagnosticWorkspace, review_f29_tls_server,
    };
    use std::fs;
    use std::sync::atomic::AtomicBool;

    // P-09：身份校验跳过不是签名校验跳过。真实 TLS 1.2/1.3 握手分别覆盖
    // 元数据和文件下载；同一自签证书的正常签名必须成功，错误 transcript 必须失败。
    #[test]
    fn review_f29_certificate_bypass_rejects_invalid_handshake_signatures() {
        const BODY: &str = r#"{"tag_name":"review-f29"}"#;
        let workspace = tempfile::tempdir().unwrap();
        let proxy = jchtools_foundation::system_proxy::SystemProxy::from_registry_values(
            0, None, None,
        );
        let mut accepted_invalid = Vec::new();
        for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
            for metadata in [true, false] {
                for wrong_signature in [false, true] {
                    let (address, server) = review_f29_tls_server(
                        version,
                        wrong_signature,
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{BODY}",
                            BODY.len(),
                        ),
                    );
                    let partial = workspace.path().join("download.part");
                    let url = format!("https://{address}/release");
                    let result = if metadata {
                        super::fetch_release_json_with_proxy(
                            &url, &AtomicBool::new(false), &mut |_| {}, &proxy,
                        )
                        .map(|value| assert_eq!(value["tag_name"], "review-f29"))
                    } else {
                        super::download_attempt(
                            &url, &partial, BODY.len() as u64, &AtomicBool::new(false),
                            &mut |_| {}, &proxy,
                        )
                        .map_err(|(message, _)| message)
                        .map(|()| assert_eq!(fs::read(&partial).unwrap(), BODY.as_bytes()))
                    };
                    let received_get = server.join().expect("本地 TLS 服务端退出");
                    if wrong_signature {
                        if result.is_ok() || received_get || partial.exists() {
                            accepted_invalid.push(format!("{version:?}, metadata={metadata}"));
                        }
                    } else {
                        result.expect("跳过证书身份检查后，有效握手签名必须成功");
                        assert!(received_get, "有效握手必须进入 HTTP 下载阶段");
                    }
                    if partial.exists() {
                        fs::remove_file(&partial).unwrap();
                    }
                }
            }
        }
        assert!(
            accepted_invalid.is_empty(),
            "P-09：不得接受无效握手签名或读取其 HTTP 正文：{accepted_invalid:?}"
        );
    }

    // 覆盖 P-09/XB-10：未知颁发者的 HTTPS 代理须首轮成功，不先等待不可用的直连。
    // 代理正文截断仍只回退直连一次；观察实际 TLS 握手、JSON 及下载字节。
    #[test]
    fn certificate_bypass_uses_first_proxy_request_and_preserves_transport_fallback() {
        const BODY: &str = r#"{"tag_name":"isolated-tls-release"}"#;
        for (metadata, truncated_proxy_body) in [(true, false), (false, false), (true, true), (false, true)] {
            let complete = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{BODY}", BODY.len());
            let proxy_response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{BODY}", BODY.len() + usize::from(truncated_proxy_body));
            let (proxy_address, proxy_server) = diagnostic_tls_server(true, 1, proxy_response);
            let (direct_address, direct_server) = diagnostic_tls_server(false, usize::from(truncated_proxy_body), complete);
            let proxy = jchtools_foundation::system_proxy::SystemProxy::from_registry_values(
                1, Some(&proxy_address.to_string()), None,
            );
            let url = format!("https://{direct_address}/release");
            let workspace = DiagnosticWorkspace::new();
            let partial = workspace.path().join("asset.part");
            if metadata {
                let value = super::fetch_release_json_with_proxy(
                    &url, &AtomicBool::new(false), &mut |_| {}, &proxy,
                ).expect("代理 HTTPS 必须首轮跳过证书校验并查询成功");
                assert_eq!(value["tag_name"], "isolated-tls-release");
            } else {
                super::download_attempt(
                    &url, &partial, BODY.len() as u64, &AtomicBool::new(false),
                    &mut |_| {}, &proxy,
                ).expect("代理 HTTPS 必须首轮跳过证书校验并下载成功");
                assert_eq!(fs::read(&partial).unwrap(), BODY.as_bytes());
            }
            assert_eq!(proxy_server.join().unwrap(), [true], "代理只应执行一次成功的 TLS 握手");
            let expected_direct = if truncated_proxy_body { vec![true] } else { vec![] };
            assert_eq!(direct_server.join().unwrap(), expected_direct, "仅真实传输失败才回退直连");
        }
    }

    // 覆盖 P-09：无代理时未知颁发者也只请求一次；初始进度回调取消不得发请求或写产物。
    #[test]
    fn certificate_bypass_direct_only_respects_cancellation() {
        const BODY: &str = r#"{"tag_name":"direct-tls-release"}"#;
        for (metadata, cancelled) in [(true, false), (false, false), (true, true), (false, true)] {
            let response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{BODY}", BODY.len());
            let (address, server) = diagnostic_tls_server(false, usize::from(!cancelled), response);
            let cancel = AtomicBool::new(false);
            let mut progress = |_: String| {
                if cancelled {
                    cancel.store(true, std::sync::atomic::Ordering::Release);
                }
            };
            let workspace = DiagnosticWorkspace::new();
            let partial = workspace.path().join("asset.part");
            let url = format!("https://{address}/release");
            let proxy = jchtools_foundation::system_proxy::SystemProxy::default();
            let result = if metadata {
                super::fetch_release_json_with_proxy(&url, &cancel, &mut progress, &proxy)
                    .map(|value| assert_eq!(value["tag_name"], "direct-tls-release"))
            } else {
                super::download_attempt(&url, &partial, BODY.len() as u64, &cancel, &mut progress, &proxy)
                    .map_err(|(message, _)| message)
                    .map(|()| assert_eq!(fs::read(&partial).unwrap(), BODY.as_bytes()))
            };
            if cancelled {
                assert!(result.unwrap_err().contains("取消"));
                assert!(!partial.exists(), "取消后不得产生下载文件");
                assert_eq!(server.join().unwrap(), Vec::<bool>::new());
            } else {
                result.expect("无代理时必须首轮跳过证书校验并成功");
                assert_eq!(server.join().unwrap(), [true]);
            }
        }
    }

    // 覆盖 P-09：每个 HTTPS 重定向目标均首轮跳过证书校验，不产生额外证书重试。
    #[test]
    fn certificate_bypass_applies_to_each_redirect_target() {
        const BODY: &str = "redirected asset";
        let (target, target_server) = diagnostic_tls_server(false, 1, format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{BODY}", BODY.len(),
        ));
        let (source, source_server) = diagnostic_tls_server(false, 1, format!(
            "HTTP/1.1 302 Found\r\nLocation: https://{target}/release\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        ));
        let workspace = DiagnosticWorkspace::new();
        let partial = workspace.path().join("redirected.part");
        super::download_attempt(
            &format!("https://{source}/release"), &partial, BODY.len() as u64,
            &AtomicBool::new(false), &mut |_| {},
            &jchtools_foundation::system_proxy::SystemProxy::default(),
        ).expect("两跳 HTTPS 均须首轮跳过证书校验并成功");
        assert_eq!(fs::read(&partial).unwrap(), BODY.as_bytes());
        assert_eq!(source_server.join().unwrap(), [true]);
        assert_eq!(target_server.join().unwrap(), [true]);
    }

    // 覆盖 P-09/O-10：HTTPS 代理拒绝 CONNECT 认证仍是代理连接失败，
    // 必须允许调用方直连回退；修复前 ProxyUnauthorized 被误分为不可回退。
    #[test]
    fn proxy_authentication_failure_allows_direct_fallback() {
        let (proxy_address, proxy_server) = diagnostic_http_server(
            b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        );
        let (result, logs) = capture_diagnostics(|| {
            super::download_request(
                "https://example.invalid/asset",
                Some(&format!("http://{proxy_address}")),
                std::time::Duration::from_secs(60),
                &mut |_| {},
                "proxy-auth-regression",
            )
        });
        proxy_server.join().expect("代理认证拒绝供给线程结束");
        let (_, retryable) = result.expect_err("代理 CONNECT 认证拒绝必须报错");
        assert!(retryable, "代理认证失败必须允许一次直连回退");
        assert!(logs.contains("proxy_connect") && logs.contains("retryable=true"), "{logs}");
    }

    // 覆盖 P-09：普通 HTTP 代理的 407 同样是代理认证失败，须仅直连回退一次。
    // HTTPS CONNECT 的同类错误由上一个用例覆盖；非代理的 HTTP 错误不得回退。
    #[test]
    fn http_proxy_authentication_failure_falls_back_to_direct_metadata() {
        let (proxy_address, proxy_server) = diagnostic_http_server(
            b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        );
        let body = r#"{"tag_name":"direct-release"}"#;
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
        let (result, logs) = capture_diagnostics(|| {
            super::fetch_release_json_with_proxy(
                &format!("http://{direct_address}/release"),
                &AtomicBool::new(false),
                &mut |_| {},
                &proxy,
            )
        });
        proxy_server.join().expect("HTTP 代理认证拒绝供给线程结束");
        assert_eq!(result.expect("HTTP 代理 407 后必须直连成功")["tag_name"], "direct-release");
        direct_server.join().expect("直连元数据供给线程结束");
        assert_eq!(logs.matches("asset_http_request_started").count(), 2, "{logs}");
        assert_eq!(logs.matches("release_metadata_direct_retrying").count(), 1, "{logs}");

        let (address, server) = diagnostic_http_server(
            b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        );
        let (_, retryable) = super::download_request(
            &format!("http://{address}/release"),
            None,
            std::time::Duration::from_secs(60),
            &mut |_| {},
            "direct-http-status-regression",
        )
        .expect_err("非代理 407 必须仍报 HTTP 错误");
        server.join().expect("非代理 HTTP 错误供给线程结束");
        assert!(!retryable, "非代理 HTTP 状态不得触发直连回退");
    }

    // P-09: 重定向到 ProxyOverride 命中目标时必须直连，不能继承首跳代理。
    #[test]
    fn redirect_rechecks_proxy_override_for_each_target() {
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};
        use std::time::Instant;

        fn accept_with_timeout(listener: &TcpListener) -> TcpStream {
            listener
                .set_nonblocking(true)
                .expect("设置本地测试监听器非阻塞");
            let deadline = Instant::now() + std::time::Duration::from_secs(3);
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        // Windows 接收的连接可能继承监听器非阻塞模式；HTTP 读取
                        // 使用下方 read_timeout，不以请求恰好已到达来掩盖调度竞态。
                        stream.set_nonblocking(false).expect("设置测试连接阻塞读取");
                        return stream;
                    }
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(error) => panic!("等待本地 HTTP 请求失败：{error}"),
                }
            }
        }

        fn read_request(stream: &mut TcpStream) -> String {
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                .expect("设置本地 HTTP 读取超时");
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 512];
            loop {
                let count = stream.read(&mut buffer).expect("读取本地 HTTP 请求");
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..count]);
                if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            String::from_utf8_lossy(&bytes).into_owned()
        }

        let target_listener =
            TcpListener::bind(("127.0.0.1", 0)).expect("启动直连重定向目标监听器");
        let target_addr = target_listener.local_addr().expect("读取目标监听地址");
        let body = b"redirected-asset";
        let target = std::thread::spawn(move || {
            let mut stream = accept_with_timeout(&target_listener);
            let request = read_request(&mut stream);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("发送直连响应头");
            stream.write_all(body).expect("发送直连响应内容");
            request
        });

        let proxy_listener = TcpListener::bind(("127.0.0.1", 0)).expect("启动系统代理模拟监听器");
        let proxy_addr = proxy_listener.local_addr().expect("读取代理监听地址");
        let redirect_url = format!("http://127.0.0.1:{}/asset", target_addr.port());
        let proxy = std::thread::spawn(move || {
            let mut stream = accept_with_timeout(&proxy_listener);
            let first_request = read_request(&mut stream);
            // ureq 3 对 HTTP CONNECT 代理不再区分目标协议：明文 HTTP 目标同样
            // 先建 CONNECT 隧道，再在隧道内发送源站 GET；旧版 ureq 2 直接发
            // 绝对地址 GET。两种线上形态都证明首跳经系统代理，此处都接受。
            let mut requests = first_request.clone();
            if first_request.starts_with("CONNECT ") {
                stream
                    .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                    .expect("应答代理隧道建立");
                requests.push('\n');
                requests.push_str(&read_request(&mut stream));
            }
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: {redirect_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream
                .write_all(response.as_bytes())
                .expect("发送代理重定向响应");
            requests
        });

        let proxy_setting = format!("http=127.0.0.1:{}", proxy_addr.port());
        let system_proxy = jchtools_foundation::system_proxy::SystemProxy::from_registry_values(
            1,
            Some(&proxy_setting),
            Some("127.0.0.1"),
        );
        let source_url = "http://origin.example/asset";
        let target_url = format!("http://127.0.0.1:{}/asset", target_addr.port());
        assert!(system_proxy.endpoint_for_url(source_url).is_some());
        assert_eq!(system_proxy.endpoint_for_url(&target_url), None);

        let temp = DiagnosticWorkspace::new();
        let partial = temp.path().join("redirect.part");
        let cancel = AtomicBool::new(false);
        let mut progress = |_message: String| {};
        let (result, logs) = capture_diagnostics(|| {
            let span = jchtools_foundation::logging::operation_span("snap_ocr_assets", "download_asset");
            let _entered = span.enter();
            super::download_attempt(
                source_url,
                &partial,
                u64::try_from(body.len()).expect("测试资产大小可转换"),
                &cancel,
                &mut progress,
                &system_proxy,
            )
        });

        let proxy_request = proxy.join().expect("代理线程完成");
        let target_request = target.join().expect("目标线程完成");
        assert!(
            proxy_request.starts_with("CONNECT origin.example:80 HTTP/1.1")
                || proxy_request.starts_with("GET http://origin.example/asset HTTP/1.1"),
            "首跳应经系统代理（CONNECT 隧道或绝对地址 GET）：{proxy_request}"
        );
        assert!(
            target_request.starts_with("GET /asset HTTP/1.1"),
            "重定向目标应直接请求：{target_request}"
        );
        result.expect("逐跳按 ProxyOverride 路由后下载成功");
        assert_eq!(fs::read(partial).expect("读取下载结果"), body);
        assert_eq!(
            logs.matches("asset_http_request_started").count(),
            2,
            "{logs}"
        );
        assert!(
            logs.contains("asset_redirect_followed")
                && logs.contains("redirect_hop=0")
                && logs.contains("redirect_hop=1")
                && logs.contains("asset_body_completed")
                && logs.contains("operation_id")
                && logs.contains("request_id"),
            "{logs}"
        );
        assert!(
            !logs.contains("redirected-asset"),
            "资产正文不得进入日志：{logs}"
        );
    }
