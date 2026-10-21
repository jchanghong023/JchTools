use super::{parse, Command, Input};
use std::ffi::OsString;

fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

#[test]
fn input_modes_are_mutually_exclusive() {
    for values in [
        vec!["--stdin", "--base64", "-"],
        vec!["--base64", "-", "--stdin"],
        vec!["--stdin", "--stdin"],
        vec!["https://example.com/a.png", "https://example.com/b.png"],
    ] {
        assert!(parse(&args(&values)).is_err());
    }
}

#[test]
fn base64_stdin_and_json_can_be_combined() {
    assert!(matches!(parse(&args(&["--json", "--base64", "-"])),
        Ok(Command::Recognize(Input::Base64(value))) if value == "-"));
}

#[test]
fn informational_commands_do_not_accept_image_inputs() {
    assert!(matches!(parse(&args(&["help"])), Ok(Command::Help)));
    assert!(matches!(parse(&args(&["--version"])), Ok(Command::Version)));
    assert!(parse(&args(&["--help", "--stdin"])).is_err());
    assert!(parse(&args(&["--base64"])).is_err());
    assert!(parse(&args(&["relative.png"])).is_err());
    assert!(parse(&args(&["ftp://example.com/a.png"])).is_err());
}

// P-09 / P-10 / O-34：只在独占测试子进程中覆盖 HKCU，既不读写用户代理配置，
// 也不改变并发测试的注册表视图。下载仍走真实 ureq 和回环 TCP，不替换请求函数。
#[rustfmt::skip]
#[cfg(all(windows, feature = "test-hooks"))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod review_network {
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        path::PathBuf,
        process::{Command, Stdio},
        time::{Duration, Instant},
    };

    const HKCU: isize = -2_147_483_647;
    const KEY_ALL_ACCESS: u32 = 0x000f_003f;

    #[link(name = "advapi32")]
    unsafe extern "system" {
        #[link_name = "RegOpenCurrentUser"]
        fn open_current_user(access: u32, key: *mut isize) -> i32;
        #[link_name = "RegCreateKeyExW"]
        fn create_key(
            parent: isize, name: *const u16, reserved: u32, class: *mut u16,
            options: u32, access: u32, security: *const core::ffi::c_void,
            key: *mut isize, disposition: *mut u32,
        ) -> i32;
        #[link_name = "RegSetValueExW"]
        fn set_value(
            key: isize, name: *const u16, reserved: u32, kind: u32,
            data: *const u8, size: u32,
        ) -> i32;
        #[link_name = "RegOverridePredefKey"]
        fn override_key(predefined: isize, replacement: isize) -> i32;
        #[link_name = "RegDeleteTreeW"]
        fn delete_tree(parent: isize, name: *const u16) -> i32;
        #[link_name = "RegCloseKey"]
        fn close_key(key: isize) -> i32;
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    struct PrivateRegistry {
        original: isize,
        root: isize,
        path: Vec<u16>,
    }

    impl PrivateRegistry {
        fn new() -> Self {
            let mut registry = Self {
                original: 0,
                root: 0,
                path: wide(&format!(
                    "Software\\JchToolsReview-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                        .unwrap().as_nanos(),
                )),
            };
            // SAFETY: original points to a live, correctly sized HKEY output.
            assert_eq!(unsafe { open_current_user(KEY_ALL_ACCESS, &raw mut registry.original) }, 0);
            // SAFETY: original is owned; path is NUL-terminated; optional pointers are null.
            assert_eq!(unsafe {
                create_key(
                    registry.original, registry.path.as_ptr(), 0, std::ptr::null_mut(),
                    1, KEY_ALL_ACCESS, std::ptr::null(), &raw mut registry.root,
                    std::ptr::null_mut(),
                )
            }, 0);
            // SAFETY: root remains open until this guard restores the child-only override.
            assert_eq!(unsafe { override_key(HKCU, registry.root) }, 0);
            registry
        }

        fn proxy(&self, endpoint: &str) {
            let mut key = 0;
            let path = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings");
            // SAFETY: root is owned, path is NUL-terminated and key is a live HKEY output.
            assert_eq!(unsafe {
                create_key(
                    self.root, path.as_ptr(), 0, std::ptr::null_mut(), 1,
                    KEY_ALL_ACCESS, std::ptr::null(), &raw mut key, std::ptr::null_mut(),
                )
            }, 0);
            let enabled = 1_u32;
            let name = wide("ProxyEnable");
            // SAFETY: key is open and DWORD data has the supplied four-byte size.
            assert_eq!(unsafe {
                set_value(key, name.as_ptr(), 0, 4, (&raw const enabled).cast(), 4)
            }, 0);
            let name = wide("ProxyServer");
            let value = wide(endpoint);
            // SAFETY: value is NUL-terminated UTF-16 and its byte size covers the terminator.
            assert_eq!(unsafe {
                set_value(
                    key, name.as_ptr(), 0, 1, value.as_ptr().cast(),
                    u32::try_from(value.len() * 2).unwrap(),
                )
            }, 0);
            // SAFETY: key is the owned handle created above and is no longer used.
            assert_eq!(unsafe { close_key(key) }, 0);
        }
    }

    impl Drop for PrivateRegistry {
        fn drop(&mut self) {
            // SAFETY: zero removes this process's predefined-key override.
            let _ = unsafe { override_key(HKCU, 0) };
            if self.root != 0 {
                // SAFETY: root is owned by this guard and no longer used.
                let _ = unsafe { close_key(self.root) };
            }
            if self.original != 0 {
                // SAFETY: original is open; path names only this child's private test key.
                let _ = unsafe { delete_tree(self.original, self.path.as_ptr()) };
                // SAFETY: original is owned by this guard and no longer used.
                let _ = unsafe { close_key(self.original) };
            }
        }
    }

    fn isolated(name: &str, body: impl FnOnce(&PrivateRegistry, PathBuf)) {
        if let Some(root) = std::env::var_os("JCHTOOLS_REVIEW_NETWORK_CHILD") {
            let registry = PrivateRegistry::new();
            body(&registry, PathBuf::from(root));
            return;
        }
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .find(|path| path.join("scripts/make_tmp.py").is_file())
            .unwrap()
            .to_path_buf();
        let workspace = repo.join(".tmp/review-repairs/JchocrRegress");
        assert!(Command::new("python")
            .arg(repo.join("scripts/make_tmp.py"))
            .args(["workspace", "--destination"])
            .arg(&workspace)
            .status().unwrap().success());
        let root = tempfile::tempdir_in(workspace).unwrap();
        let output_file = root.path().join("child.stdout");
        let error_file = root.path().join("child.stderr");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact", &format!("jchocr::tests::review_network::{name}"),
                "--nocapture", "--test-threads=1",
            ])
            .current_dir(root.path())
            .env("JCHTOOLS_REVIEW_NETWORK_CHILD", root.path())
            .env("JCHTOOLS_TEST_STATE_DIR", root.path().join("state"))
            .env("JCH_LOG_LEVEL", "INFO")
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(&output_file).unwrap())
            .stderr(std::fs::File::create(&error_file).unwrap())
            .spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(100);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("isolated network regression exceeded 100 seconds");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(
            status.success(),
            "isolated regression failed:\n{}\n{}",
            std::fs::read_to_string(output_file).unwrap(),
            std::fs::read_to_string(error_file).unwrap(),
        );
    }

    fn listener() -> TcpListener {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        listener
    }

    fn accept(listener: &TcpListener) -> Option<TcpStream> {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(40))).unwrap();
                stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
                Some(stream)
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => None,
            Err(error) => panic!("loopback accept failed: {error}"),
        }
    }

    fn headers(stream: &mut TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut byte = [0];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
            assert!(request.len() < 16 * 1024);
        }
        request
    }

    // P-09 / O-34：不回 CONNECT 响应触发真实 Timeout::Connect；本地直连仍可成功。
    #[test]
    fn review_f42_proxy_connect_timeout_retries_direct_once() {
        isolated("review_f42_proxy_connect_timeout_retries_direct_once", |registry, _| {
            for status in [200, 500] {
                let proxy = listener();
                let direct = listener();
                registry.proxy(&proxy.local_addr().unwrap().to_string());
                let url = format!("http://{}/image.png", direct.local_addr().unwrap());
                let request = std::thread::spawn(move || super::super::fetch_image(url));
                let mut proxy_stream = None;
                let mut proxy_attempts = 0;
                let mut direct_attempts = 0;
                while !request.is_finished() {
                    if let Some(mut stream) = accept(&proxy) {
                        assert!(headers(&mut stream).starts_with(b"CONNECT "));
                        proxy_attempts += 1;
                        // 保持连接且不响应：不是 refused/EOF/普通 Io 错误。
                        proxy_stream = Some(stream);
                    }
                    if let Some(mut stream) = accept(&direct) {
                        assert!(headers(&mut stream).starts_with(b"GET /image.png "));
                        direct_attempts += 1;
                        write!(stream,
                            "HTTP/1.1 {status} Test\r\nContent-Length: 5\r\nConnection: close\r\n\r\nimage"
                        ).unwrap();
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                let result = request.join().unwrap();
                drop(proxy_stream);
                assert_eq!(proxy_attempts, 1, "only the original proxy attempt is allowed");
                assert_eq!(direct_attempts, 1, "connect timeout must cause exactly one direct attempt");
                if status == 200 {
                    assert_eq!(
                        result.unwrap_or_else(|error| panic!("direct download failed: {}", error.message)),
                        b"image",
                    );
                } else {
                    let error = result.err().expect("direct HTTP failure must end the download");
                    assert_eq!((error.code, error.kind), (1, "network"));
                }
            }
        });
    }

    // P-09 / O-34：隧道建立后的响应/正文 timeout 及响应断流均不得切换直连。
    #[test]
    fn review_f42_response_and_body_timeouts_do_not_retry_direct() {
        isolated("review_f42_response_and_body_timeouts_do_not_retry_direct", |registry, _| {
            for stage in ["response_timeout", "body_timeout", "response_closed"] {
                let proxy = listener();
                let direct = listener();
                registry.proxy(&proxy.local_addr().unwrap().to_string());
                let url = format!("http://{}/image.png", direct.local_addr().unwrap());
                let request = std::thread::spawn(move || super::super::fetch_image(url));
                let mut tunnel = None;
                let mut proxy_attempts = 0;
                let mut direct_attempts = 0;
                while !request.is_finished() {
                    if let Some(mut stream) = accept(&proxy) {
                        assert!(headers(&mut stream).starts_with(b"CONNECT "));
                        proxy_attempts += 1;
                        stream.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").unwrap();
                        assert!(headers(&mut stream).starts_with(b"GET "));
                        if stage == "body_timeout" {
                            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n").unwrap();
                        }
                        if stage != "response_closed" {
                            tunnel = Some(stream);
                        }
                    }
                    if let Some(mut stream) = accept(&direct) {
                        direct_attempts += 1;
                        headers(&mut stream);
                        stream.write_all(b"HTTP/1.1 500 Test\r\nContent-Length: 0\r\n\r\n").unwrap();
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                let error = request.join().unwrap().err().expect("response/body failure must fail");
                drop(tunnel);
                assert_eq!((error.code, error.kind), (1, "network"));
                assert_eq!(proxy_attempts, 1);
                assert_eq!(direct_attempts, 0, "post-connect failure must not switch routes: {stage}");
            }
        });
    }

    // P-09 / O-34：TLS fatal alert / 握手超时均不属于可回退的代理连接阶段。
    #[test]
    fn review_f42_tls_failure_does_not_retry_direct() {
        isolated("review_f42_tls_failure_does_not_retry_direct", |registry, _| {
            for send_alert in [true, false] {
                let proxy = listener();
                let direct = listener();
                registry.proxy(&proxy.local_addr().unwrap().to_string());
                let url = format!("https://{}/image.png", direct.local_addr().unwrap());
                let request = std::thread::spawn(move || super::super::fetch_image(url));
                let mut tunnel = None;
                let mut proxy_attempts = 0;
                let mut direct_attempts = 0;
                while !request.is_finished() {
                    if let Some(mut stream) = accept(&proxy) {
                        assert!(headers(&mut stream).starts_with(b"CONNECT "));
                        proxy_attempts += 1;
                        stream.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").unwrap();
                        let mut hello = [0; 5];
                        stream.read_exact(&mut hello).unwrap();
                        assert_eq!(hello[0], 22, "the client must begin a TLS handshake");
                        let mut payload = vec![0; usize::from(u16::from_be_bytes([hello[3], hello[4]]))];
                        stream.read_exact(&mut payload).unwrap();
                        if send_alert {
                            stream.write_all(&[21, 3, 3, 0, 2, 2, 40]).unwrap();
                        } else {
                            // CONNECT 已成功，保持 TLS 无响应以覆盖同样标为 Connect 的握手超时。
                            tunnel = Some(stream);
                        }
                    }
                    if let Some(mut stream) = accept(&direct) {
                        direct_attempts += 1;
                        let _ = stream.write_all(&[21, 3, 3, 0, 2, 2, 40]);
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                let error = request.join().unwrap().err().expect("TLS failure must fail");
                drop(tunnel);
                assert_eq!((error.code, error.kind), (1, "network"));
                assert_eq!(proxy_attempts, 1);
                assert_eq!(direct_attempts, 0, "TLS failure must not trigger direct fallback");
            }
        });
    }

    // P-10 / O-34 / O-36：默认 INFO 日志必须留下实际 HTTP 失败状态、阶段和尝试，
    // URL 凭据、路径、query、响应正文和图像标记均不得入盘；不需要任何模型。
    #[test]
    fn review_f44_http_failures_log_safe_status_stage_and_attempt() {
        isolated("review_f44_http_failures_log_safe_status_stage_and_attempt", |registry, root| {
            let logging_guard = jchtools_foundation::logging::init(&root.join("state"))
                .expect("isolated diagnostics must initialize");
            let span = jchtools_foundation::logging::operation_span("jchocr", "ocr");
            let _entered = span.enter();
            let mut urls = Vec::new();
            for (status, retry_direct) in [(403, false), (404, false), (500, false), (500, true)] {
                let proxy_failure = retry_direct.then(|| {
                    let proxy = listener();
                    registry.proxy(&proxy.local_addr().unwrap().to_string());
                    std::thread::spawn(move || {
                        let deadline = Instant::now() + Duration::from_secs(10);
                        loop {
                            if let Some(mut stream) = accept(&proxy) {
                                assert!(headers(&mut stream).starts_with(b"CONNECT "));
                                stream.write_all(b"HTTP/1.1 502 Test\r\nContent-Length: 0\r\n\r\n").unwrap();
                                break;
                            }
                            assert!(Instant::now() < deadline, "download must try the proxy first");
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    })
                });
                let server = listener();
                let url = format!(
                    "http://review-user:review-password@{}/private-image.png?token=review-secret",
                    server.local_addr().unwrap(),
                );
                let serving = std::thread::spawn(move || {
                    let deadline = Instant::now() + Duration::from_secs(10);
                    loop {
                        if let Some(mut stream) = accept(&server) {
                            headers(&mut stream);
                            let body = b"private-response-body image-base64-sensitive";
                            write!(stream,
                                "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            ).unwrap();
                            let _ = stream.write_all(body);
                            break;
                        }
                        assert!(Instant::now() < deadline, "download must request the loopback endpoint");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                });
                let error = super::super::fetch_image(url.clone())
                    .err().expect("HTTP failure must end the download");
                assert_eq!((error.code, error.kind), (1, "network"));
                assert!(error.message.contains(&format!("HTTP {status}")));
                serving.join().unwrap();
                if let Some(proxy_failure) = proxy_failure {
                    proxy_failure.join().unwrap();
                }
                urls.push(url);
            }
            drop(logging_guard);
            let logs = std::fs::read_dir(root.join("state/logs")).unwrap()
                .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
                .collect::<Vec<_>>().join("\n");
            for secret in [
                "review-user", "review-password", "private-image.png", "review-secret",
                "private-response-body", "image-base64-sensitive",
            ] {
                assert!(!logs.contains(secret), "diagnostics leaked {secret}");
            }
            for url in urls {
                assert!(!logs.contains(&url));
            }
            for status in [403, 404, 500] {
                let line = logs.lines().find(|line| {
                    line.split_ascii_whitespace().any(|field| {
                        field == format!("status={status}") || field == format!("http_status={status}")
                    })
                }).unwrap_or_else(|| panic!("default logs lack HTTP {status} failure:\n{logs}"));
                assert!(line.contains("stage="), "HTTP failure must identify its stage: {line}");
                assert!(
                    line.split_ascii_whitespace().any(|field| field == "attempt=1"),
                    "HTTP failure must identify its attempt: {line}",
                );
                assert!(line.contains("operation_id="), "HTTP failure must retain correlation: {line}");
            }
            let retried = logs.lines().find(|line| {
                line.split_ascii_whitespace().any(|field| field == "http_status=500")
                    && line.split_ascii_whitespace().any(|field| field == "attempt=2")
            }).expect("direct retry HTTP failure must log the actual second attempt");
            assert!(retried.contains("stage="));
            assert!(retried.contains("route=direct") || retried.contains("route=\"direct\""));
            assert!(retried.contains("operation_id="));
        });
    }
}
