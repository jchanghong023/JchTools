#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
//! JchTools 图形界面入口：全部界面组装在 `jchtools::gui`（保持 bin 为薄壳，便于测试）。
fn main() {
    if std::env::args().nth(1).as_deref() == Some("--xberg-broker") {
        // P-10：代理进程同样落盘诊断日志（IPC 与引擎生命周期的关键证据）。
        // 句柄登记为进程全局：停止看门狗 exit(0) 前能显式刷盘，serve 返回路径
        // 也统一经 flush_before_exit 落盘（静态量不随进程退出自动析构）。
        if let Some(guard) = jchtools::xberg_settings::state_dir()
            .ok()
            .as_deref()
            .and_then(jchtools::logging::init)
        {
            jchtools::logging::hold_for_process(guard);
        }
        // P-10：guard 存于静态量后不随 panic 展开析构；用本地 RAII 兜住
        // serve() 的 unwind 路径（正常/错误退出路径由下方 flush_before_exit
        // 显式覆盖，两者经同一互斥量幂等）。
        let _flush_on_unwind = jchtools::logging::FlushOnDrop;
        let served = jchtools::xberg_runtime::serve();
        jchtools::logging::flush_before_exit();
        if served.is_err() {
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
