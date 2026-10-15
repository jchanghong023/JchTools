//! O-33～O-37：通过应用共享截图运行时识别单张图片的控制台客户端。
use base64::Engine as _;
use serde_json::json;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

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
                let url = ureq::get(value)
                    .request_url()
                    .map_err(|_| Failure::input("HTTP(S) 图片 URL 无效"))?;
                if !matches!(url.as_url().scheme(), "http" | "https")
                    || url.as_url().host_str().is_none()
                {
                    return Err(Failure::input("图片 URL 仅接受 HTTP 或 HTTPS"));
                }
                Input::Url(url.as_url().to_string())
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
    let settings = jchtools_foundation::xberg_settings::settings().map_err(|error| {
        Failure::new(
            1,
            "runtime",
            format!("读取 JchTools Xberg 设置失败：{error}"),
        )
    })?;
    let root = match settings.source {
        jchtools_foundation::xberg_settings::Source::Custom => settings.custom,
        jchtools_foundation::xberg_settings::Source::Downloaded => settings.downloaded,
    }
    .ok_or_else(|| {
        Failure::new(
            3,
            "xberg",
            "尚未配置 Xberg。请在 JchTools 设置页下载或配置 Xberg。",
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
            format!("{error}。请在 JchTools 设置页下载或配置完整的 Xberg 截图资产。"),
        )
    })?;
    Ok(root)
}

fn http_request(url: &str, proxy: Option<&str>) -> Result<ureq::Response, ureq::ErrorKind> {
    let mut builder = ureq::builder()
        .redirects(0)
        .timeout_connect(Duration::from_secs(30))
        .timeout_read(Duration::from_secs(30));
    if let Some(proxy) = proxy.and_then(|value| ureq::Proxy::new(value).ok()) {
        builder = builder.proxy(proxy);
    }
    // HTTP 错误状态仍是有效响应；统一交给下载层判断，避免携带大尺寸 Response 的错误枚举。
    match builder.build().get(url).call() {
        Ok(response) | Err(ureq::Error::Status(_, response)) => Ok(response),
        Err(ureq::Error::Transport(error)) => Err(error.kind()),
    }
}

fn fetch_image(mut url: String) -> Result<Vec<u8>, Failure> {
    let proxy = jchtools_foundation::system_proxy::read();
    for hop in 0..3 {
        let endpoint = proxy.endpoint_for_url(&url);
        tracing::info!(
            component = "jchocr",
            event = "image_download_started",
            redirect_hop = hop,
            route = if endpoint.is_some() {
                "proxy"
            } else {
                "direct"
            },
            "图片下载开始"
        );
        let response = match http_request(&url, endpoint.as_deref()) {
            Err(error)
                if endpoint.is_some()
                    && matches!(
                        error,
                        ureq::ErrorKind::Dns
                            | ureq::ErrorKind::ConnectionFailed
                            | ureq::ErrorKind::ProxyConnect
                            | ureq::ErrorKind::ProxyUnauthorized
                            | ureq::ErrorKind::Io
                    ) =>
            {
                tracing::warn!(
                    component = "jchocr",
                    event = "image_download_direct_retry",
                    "系统代理连接失败，直连重试一次"
                );
                http_request(&url, None)
            }
            result => result,
        }
        .map_err(|_| Failure::new(1, "network", "图片下载失败：网络连接错误"))?;
        if matches!(response.status(), 301 | 302 | 303 | 307 | 308) {
            if hop == 2 {
                return Err(Failure::new(
                    1,
                    "network",
                    "图片下载达到 HTTP 重定向次数上限",
                ));
            }
            let location = response
                .header("location")
                .ok_or_else(|| Failure::new(1, "network", "图片重定向缺少 Location 响应头"))?;
            let source = ureq::get(&url)
                .request_url()
                .map_err(|_| Failure::new(1, "network", "图片重定向源地址无效"))?;
            let target = source
                .as_url()
                .join(location)
                .map_err(|_| Failure::new(1, "network", "图片重定向目标地址无效"))?;
            if !matches!(target.scheme(), "http" | "https") || target.host_str().is_none() {
                return Err(Failure::new(1, "network", "图片重定向仅接受 HTTP 或 HTTPS"));
            }
            url = target.to_string();
            continue;
        }
        if !(200..300).contains(&response.status()) {
            return Err(Failure::new(
                1,
                "network",
                format!("图片下载失败：HTTP {}", response.status()),
            ));
        }
        let mut bytes = Vec::new();
        response
            .into_reader()
            .read_to_end(&mut bytes)
            .map_err(|_| Failure::new(1, "network", "无法读取下载的图片"))?;
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
}
