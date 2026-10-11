#![cfg_attr(windows, windows_subsystem = "windows")]

//! 截图 OCR 独立后台进程；主界面通过本地管道连接，不提供用户命令行入口。

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--capabilities") => {
            println!("{{\"shared_xberg_protocol\":2,\"background_service_protocol\":1}}");
        }
        Some("--xberg-broker") => {
            // P-10：代理进程落盘诊断日志（IPC 与引擎生命周期的关键证据）。
            if let Some(guard) = snap_ocr_worker::xberg_settings::state_dir()
                .ok()
                .as_deref()
                .and_then(snap_ocr_worker::logging::init)
            {
                snap_ocr_worker::logging::hold_for_process(guard);
            }
            let _flush_on_unwind = snap_ocr_worker::logging::FlushOnDrop;
            let served = snap_ocr_worker::xberg_runtime::serve();
            if served.is_err() {
                tracing::error!(
                    event = "application_failed",
                    stage = "serve",
                    error_type = "broker_failure",
                    exit_code = 1,
                    "截图共享代理异常退出"
                );
            }
            snap_ocr_worker::logging::flush_before_exit();
            if served.is_err() {
                std::process::exit(1);
            }
        }
        Some(flag @ ("--service" | "--service--autostart")) => {
            let autostart = flag == "--service--autostart";
            // P-10：初始化失败尝试用户级日志落点；均失败时 stderr 报告并继续服务。
            if let Some(guard) = snap_ocr_worker::xberg_settings::state_dir()
                .ok()
                .as_deref()
                .and_then(snap_ocr_worker::logging::init)
            {
                snap_ocr_worker::logging::hold_for_process(guard);
            }
            let _flush_on_unwind = snap_ocr_worker::logging::FlushOnDrop;
            let served = snap_ocr_worker::service::run_service(autostart);
            if let Err(error) = &served {
                tracing::error!(
                    event = "application_failed",
                    stage = "serve",
                    error_type = "service_failure",
                    exit_code = 1,
                    "截图服务异常退出"
                );
                eprintln!("截图服务启动失败：{error}");
            }
            snap_ocr_worker::logging::flush_before_exit();
            if served.is_err() {
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!("请从 JchTools 主界面启动截图服务");
            std::process::exit(2);
        }
    }
}
