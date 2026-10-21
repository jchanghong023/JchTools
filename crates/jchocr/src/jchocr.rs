//! O-33～O-37：通过应用共享截图运行时识别单张图片的控制台客户端。
mod cli;
mod download;
mod xberg;

#[cfg(test)]
mod tests;

use cli::{parse, Command, Failure};
use serde_json::json;
use std::ffi::OsString;
use std::io::Write;
use xberg::recognize;

#[cfg(test)]
use cli::Input;
#[cfg(test)]
use download::fetch_image;

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
