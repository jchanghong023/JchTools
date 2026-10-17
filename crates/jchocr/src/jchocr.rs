//! O-33～O-37：通过应用共享截图运行时识别单张图片的控制台客户端。
use base64::Engine as _;
use jchtools_xberg::xberg_runtime::MAX_MESSAGE_BYTES;
use serde_json::json;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Duration;
use ureq::unversioned::{
    resolver::DefaultResolver,
    transport::{
        ConnectProxyConnector, ConnectionDetails, Connector, RustlsConnector, SocksConnector,
        TcpConnector,
    },
};

// 为共享协议 envelope 的有界元数据预留 256 KiB，其余预算覆盖 Base64 的 4:3 扩张。
const MAX_DOWNLOADED_IMAGE_BYTES: u64 = ((MAX_MESSAGE_BYTES - 256 * 1024) / 4) * 3;

const HELP: &str = r#"jchocr - 图片 OCR

用法：
  jchocr <图片绝对路径或HTTP(S) URL> [--json]
  jchocr --stdin [--json]
  jchocr --base64 <Base64值或-> [--json]
  jchocr --help | help
  jchocr --version

每次只选一种输入；不接受相对路径。
--stdin 读取原始图片字节；--base64 - 从 stdin 读取 Base64，忽略空白。
URL 仅限 HTTP(S)；Base64 不接受 data: URL。
常见格式：PNG、JPEG、BMP、GIF、TIFF、WebP，实际支持由引擎决定。

输出：
  默认 stdout：UTF-8 OCR 原文，保留空格和换行。
  --json stdout：{"ok":true,"text":"..."}
  无文字：成功，text 为空。
  失败只写 stderr；--json：{"ok":false,"error_kind":"...","error":"..."}
  error_kind：usage、input、xberg、network、inference、runtime。

退出码：0 成功；2 参数或输入错误；3 Xberg 或所需资产不可用；
        1 下载、推理或运行错误。
"#;
#[derive(Debug)]
enum Input {
    File(PathBuf),
    Url(String),
    Stdin,
    Base64(OsString),
}

#[derive(Debug)]
enum Command {
    Help,
    Version,
    Recognize(Input),
}

struct Failure {
    code: i32,
    kind: &'static str,
    message: String,
}

impl Failure {
    fn new(code: i32, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            kind,
            message: message.into(),
        }
    }
    fn usage(message: &str) -> Self {
        Self::new(2, "usage", format!("{message}；用法请见 jchocr --help"))
    }
    fn input(message: &str) -> Self {
        Self::new(2, "input", message)
    }
}

fn parse(args: &[OsString]) -> Result<Command, Failure> {
    let mut input = None;
    let mut information = None;
    let mut json_seen = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let next_input = match arg.to_str() {
            Some("--json") if !json_seen => {
                json_seen = true;
                continue;
            }
            Some("--help" | "help" | "--version") => {
                if information.is_some() {
                    return Err(Failure::usage("帮助和版本命令只能选择一个"));
                }
                information = Some(if arg == "--version" {
                    Command::Version
                } else {
                    Command::Help
                });
                continue;
            }
            Some("--stdin") => Input::Stdin,
            Some("--base64") => Input::Base64(
                args.next()
                    .cloned()
                    .ok_or_else(|| Failure::usage("--base64 需要 Base64 值或 '-'"))?,
            ),
            Some(value) if value.starts_with('-') => {
                return Err(Failure::usage("未知或重复的选项"))
            }
            Some(value) if value.contains("://") => {
                let url =
                    url::Url::parse(value).map_err(|_| Failure::input("HTTP(S) 图片 URL 无效"))?;
                if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                    return Err(Failure::input("图片 URL 仅接受 HTTP 或 HTTPS"));
                }
                Input::Url(url.to_string())
            }
            _ => {
                let path = PathBuf::from(arg);
                if !path.is_absolute() {
                    return Err(Failure::input("本地图片路径必须为绝对路径"));
                }
                Input::File(path)
            }
        };
        if input.replace(next_input).is_some() {
            return Err(Failure::usage("必须且只能选择一种图片输入"));
        }
    }
    if let Some(command) = information {
        if input.is_some() {
            return Err(Failure::usage("帮助或版本命令不能与图片输入组合"));
        }
        return Ok(command);
    }
    input
        .map(Command::Recognize)
        .ok_or_else(|| Failure::usage("必须提供图片输入"))
}

fn read_stdin() -> Result<Vec<u8>, Failure> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .lock()
        .read_to_end(&mut bytes)
        .map_err(|_| Failure::input("无法从标准输入读取图片"))?;
    Ok(bytes)
}

fn local_bytes(input: Input) -> Result<Vec<u8>, Failure> {
    match input {
        Input::File(path) => {
            std::fs::read(path).map_err(|_| Failure::input("无法读取本地图片文件"))
        }
        Input::Stdin => read_stdin(),
        Input::Base64(value) => {
            let mut encoded = if value == "-" {
                read_stdin()?
            } else {
                value
                    .into_string()
                    .map_err(|_| Failure::input("Base64 输入必须使用 ASCII 字符"))?
                    .into_bytes()
            };
            encoded.retain(|byte| !byte.is_ascii_whitespace());
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| Failure::input("标准 Base64 图片输入无效"))
        }
        Input::Url(_) => unreachable!("URL 输入在 Xberg 预检后下载"),
    }
}

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

// 仅在 TCP / SOCKS / CONNECT 阶段保留可回退错误；TLS 握手也可能返回 Io
// 或 Timeout::Connect，不能从最终错误枚举或错误文案反推它属于代理连接阶段。
#[derive(Debug)]
struct ConnectionStage<C>(C);

#[derive(Debug)]
struct ProxyConnectionFailure(ureq::Error);

impl std::fmt::Display for ProxyConnectionFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("代理连接阶段失败")
    }
}

impl std::error::Error for ProxyConnectionFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

impl<C: Connector> Connector for ConnectionStage<C> {
    type Out = C::Out;

    fn connect(
        &self,
        details: &ConnectionDetails<'_>,
        chained: Option<()>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        self.0.connect(details, chained).map_err(|error| {
            if matches!(
                error,
                ureq::Error::HostNotFound
                    | ureq::Error::ConnectionFailed
                    | ureq::Error::ConnectProxyFailed(_)
                    | ureq::Error::Io(_)
                    | ureq::Error::Timeout(ureq::Timeout::Connect)
            ) {
                ureq::Error::Other(Box::new(ProxyConnectionFailure(error)))
            } else {
                error
            }
        })
    }
}

fn proxy_connection_failed(error: &ureq::Error) -> bool {
    match error {
        // 解析可能在 connector 之前失败；这两个变体不表示 TLS / 响应阶段错误。
        ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => true,
        ureq::Error::Other(error) => error.is::<ProxyConnectionFailure>(),
        _ => false,
    }
}

fn http_request(
    url: &str,
    proxy: Option<&str>,
) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
    // HTTP 错误状态仍是有效响应：统一交给下载层判断状态码，避免错误枚举携带响应体。
    let mut config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_recv_body(Some(Duration::from_secs(30)));
    // ureq 3 的 config_builder 默认继承环境代理（Proxy::try_from_env）；
    // P-09 要求逐跳显式路由，无代理时必须显式清空，避免环境变量劫持"直连"。
    let proxy = proxy.and_then(|value| ureq::Proxy::new(value).ok());
    config = config.proxy(proxy);
    // 与锁定 ureq 的默认 SOCKS → CONNECT → TCP → rustls 链一致，仅在 TLS
    // 之前标记连接错误。证书校验及握手签名校验保持默认开启。
    let connection =
        ().chain(SocksConnector::default())
            .chain(ConnectProxyConnector::default())
            .chain(TcpConnector::default());
    let connector = ConnectionStage(connection).chain(RustlsConnector::default());
    ureq::Agent::with_parts(config.build(), connector, DefaultResolver::default())
        .get(url)
        .call()
}

fn image_download_over_budget(stage: &'static str) -> Failure {
    tracing::warn!(
        component = "jchocr",
        event = "image_download_rejected",
        stage,
        reason = "message_budget_exceeded",
        "图片下载超过共享协议容量预算"
    );
    Failure::new(1, "network", "图片下载超过共享协议容量预算")
}

fn fetch_image(mut url: String) -> Result<Vec<u8>, Failure> {
    let proxy = jchtools_foundation::system_proxy::read();
    for hop in 0..3 {
        let endpoint = proxy.endpoint_for_url(&url);
        let mut attempt = 1_u8;
        let mut route = if endpoint.is_some() {
            "proxy"
        } else {
            "direct"
        };
        tracing::info!(
            component = "jchocr",
            event = "image_download_started",
            redirect_hop = hop,
            target = "image_download",
            attempt,
            route,
            "图片下载开始"
        );
        let response = match http_request(&url, endpoint.as_deref()) {
            Err(error) if endpoint.is_some() && proxy_connection_failed(&error) => {
                tracing::warn!(
                    component = "jchocr",
                    event = "image_download_direct_retry",
                    target = "image_download",
                    stage = "connect",
                    redirect_hop = hop,
                    route,
                    attempt,
                    next_attempt = 2,
                    "系统代理连接失败，直连重试一次"
                );
                attempt = 2;
                route = "direct";
                http_request(&url, None)
            }
            result => result,
        }
        .map_err(|_| Failure::new(1, "network", "图片下载失败：网络连接错误"))?;
        if matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
            if hop == 2 {
                return Err(Failure::new(
                    1,
                    "network",
                    "图片下载达到 HTTP 重定向次数上限",
                ));
            }
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| Failure::new(1, "network", "图片重定向缺少 Location 响应头"))?;
            let source = url::Url::parse(&url)
                .map_err(|_| Failure::new(1, "network", "图片重定向源地址无效"))?;
            let target = source
                .join(location)
                .map_err(|_| Failure::new(1, "network", "图片重定向目标地址无效"))?;
            if !matches!(target.scheme(), "http" | "https") || target.host_str().is_none() {
                return Err(Failure::new(1, "network", "图片重定向仅接受 HTTP 或 HTTPS"));
            }
            url = target.to_string();
            continue;
        }
        if !(200..300).contains(&response.status().as_u16()) {
            tracing::error!(
                component = "jchocr",
                event = "image_download_failed",
                target = "image_download",
                stage = "response_status",
                http_status = response.status().as_u16(),
                redirect_hop = hop,
                route,
                attempt,
                "图片下载返回失败状态"
            );
            return Err(Failure::new(
                1,
                "network",
                format!("图片下载失败：HTTP {}", response.status().as_u16()),
            ));
        }
        if response
            .headers()
            .get("content-length")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .is_some_and(|length| length > MAX_DOWNLOADED_IMAGE_BYTES)
        {
            return Err(image_download_over_budget("content_length"));
        }
        let mut bytes = Vec::new();
        response
            .into_body()
            .into_reader()
            .take(MAX_DOWNLOADED_IMAGE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Failure::new(1, "network", "无法读取下载的图片"))?;
        if bytes.len() as u64 > MAX_DOWNLOADED_IMAGE_BYTES {
            return Err(image_download_over_budget("body"));
        }
        tracing::info!(
            component = "jchocr",
            event = "image_download_completed",
            "图片下载完成"
        );
        return Ok(bytes);
    }
    unreachable!("最后一跳必然结束重定向循环")
}

fn recognize(input: Input) -> Result<String, Failure> {
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

fn report(error: &Failure, json_output: bool) -> i32 {
    let mut stderr = std::io::stderr().lock();
    if json_output {
        let _ = writeln!(
            stderr,
            "{}",
            json!({"ok":false,"error_kind":error.kind,"error":error.message})
        );
    } else {
        let _ = writeln!(stderr, "jchocr: {}", error.message);
    }
    error.code
}

/// 执行公开控制台合同；参数不包含程序名。
pub fn run(args: &[OsString]) -> i32 {
    // 即使其他参数无效，也按 --json 格式输出错误。
    let json_output = args.iter().any(|arg| arg == "--json");
    let command = match parse(args) {
        Ok(command) => command,
        Err(error) => return report(&error, json_output),
    };
    let input = match command {
        Command::Help => return print_stdout(HELP.as_bytes(), json_output),
        Command::Version => {
            return print_stdout(
                concat!("jchocr ", env!("CARGO_PKG_VERSION"), "\n").as_bytes(),
                json_output,
            )
        }
        Command::Recognize(input) => input,
    };
    if let Some(guard) = jchtools_foundation::xberg_settings::state_dir()
        .ok()
        .as_deref()
        .and_then(jchtools_foundation::logging::init)
    {
        jchtools_foundation::logging::hold_for_process(guard);
    }
    let _flush_on_unwind = jchtools_foundation::logging::FlushOnDrop;
    let span = jchtools_foundation::logging::operation_span("jchocr", "ocr");
    let _entered = span.enter();
    let started = std::time::Instant::now();
    tracing::info!(
        component = "jchocr",
        event = "ocr_cli_started",
        "图片 OCR 命令开始"
    );
    let result = recognize(input);
    let code = match result {
        Ok(text) if json_output => print_stdout(
            format!("{}\n", json!({"ok":true,"text":text})).as_bytes(),
            true,
        ),
        Ok(text) => print_stdout(text.as_bytes(), false),
        Err(error) => {
            tracing::error!(
                component = "jchocr",
                event = "ocr_cli_failed",
                error_type = error.kind,
                exit_code = error.code,
                "图片 OCR 命令失败"
            );
            report(&error, json_output)
        }
    };
    tracing::info!(
        component = "jchocr",
        event = "ocr_cli_completed",
        exit_code = code,
        elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
        "图片 OCR 命令结束"
    );
    jchtools_foundation::logging::flush_before_exit();
    code
}

fn print_stdout(bytes: &[u8], json_output: bool) -> i32 {
    let mut stdout = std::io::stdout().lock();
    match stdout.write_all(bytes).and_then(|()| stdout.flush()) {
        Ok(()) => 0,
        Err(_) => report(
            &Failure::new(1, "runtime", "无法向标准输出写入 UTF-8 正文"),
            json_output,
        ),
    }
}

#[cfg(test)]
mod tests {
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
}
