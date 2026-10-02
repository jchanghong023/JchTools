#![cfg_attr(windows, windows_subsystem = "windows")]

//! 截图 OCR 独立后台进程；主界面通过本地管道连接，不提供用户命令行入口。

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--capabilities") => {
            println!("{{\"shared_xberg_protocol\":2,\"background_service_protocol\":1}}");
        }
        Some("--xberg-broker") => {
            // P-10：代理进程落盘诊断日志（IPC 与引擎生命周期的关键证据）。
            let _log_guard = snap_ocr_worker::xberg_settings::state_dir()
                .ok()
                .as_deref()
                .and_then(snap_ocr_worker::logging::init);
            if snap_ocr_worker::xberg_runtime::serve().is_err() {
                std::process::exit(1);
            }
        }
        Some(flag @ ("--service" | "--service--autostart")) => {
            let autostart = flag == "--service--autostart";
            if let Err(error) = snap_ocr_worker::service::run_service(autostart) {
                eprintln!("截图服务启动失败：{error}");
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!("请从 JchTools 主界面启动截图服务");
            std::process::exit(2);
        }
    }
}
