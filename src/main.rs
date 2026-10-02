#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
//! JchTools 图形界面入口：全部界面组装在 `jchtools::gui`（保持 bin 为薄壳，便于测试）。
fn main() {
    if std::env::args().nth(1).as_deref() == Some("--xberg-broker") {
        // P-10：代理进程同样落盘诊断日志（IPC 与引擎生命周期的关键证据）。
        let _log_guard = jchtools::xberg_settings::state_dir()
            .ok()
            .as_deref()
            .and_then(jchtools::logging::init);
        if jchtools::xberg_runtime::serve().is_err() {
            std::process::exit(1);
        }
        return;
    }
    if let Err(error) = jchtools::gui::run() {
        let _ = rfd::MessageDialog::new()
            .set_title("JchTools 启动失败")
            .set_description(format!(
                "{error:#}

可尝试设置 SLINT_BACKEND=winit-software 后启动。"
            ))
            .set_level(rfd::MessageLevel::Error)
            .show();
        std::process::exit(1);
    }
}
