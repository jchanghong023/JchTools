use base64::Engine as _;
use std::ffi::OsString;
use std::io::Read;
use std::path::PathBuf;

#[derive(Debug)]
pub(super) enum Input {
    File(PathBuf),
    Url(String),
    Stdin,
    Base64(OsString),
}

#[derive(Debug)]
pub(super) enum Command {
    Help,
    Version,
    Recognize(Input),
}

pub(super) struct Failure {
    pub(super) code: i32,
    pub(super) kind: &'static str,
    pub(super) message: String,
}

impl Failure {
    pub(super) fn new(code: i32, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            kind,
            message: message.into(),
        }
    }
    fn usage(message: &str) -> Self {
        Self::new(2, "usage", format!("{message}；用法请见 jchocr --help"))
    }
    pub(super) fn input(message: &str) -> Self {
        Self::new(2, "input", message)
    }
}

pub(super) fn parse(args: &[OsString]) -> Result<Command, Failure> {
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

pub(super) fn local_bytes(input: Input) -> Result<Vec<u8>, Failure> {
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
