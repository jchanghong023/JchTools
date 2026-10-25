// 测试骨架保持控制台子系统（not(test)）：GUI 子系统的骨架被 PowerShell `&`
// 直调时不等待、无输出、无退出码，验收阶段会丢日志（回归测试见文件尾）。
#![cfg_attr(all(target_os = "windows", not(test)), windows_subsystem = "windows")]
//! JchTools 图形界面入口：全部界面组装在 `jchtools_gui::gui`（保持 bin 为薄壳，便于测试）。
/// 三个内部后台服务分支共用的骨架：初始化诊断日志并登记进程句柄、建立 FlushOnDrop
/// 兜底后调用服务；失败时先按调用方字段上报 `application_failed`，再统一刷盘，最后以
/// 退出码 1 退出（Err 路径先上报后刷盘，Ok 路径仅刷盘）。
fn run_background<E>(service: impl FnOnce() -> Result<(), E>, report_failure: impl FnOnce(&E)) {
    // P-10：代理进程同样落盘诊断日志（IPC 与引擎生命周期的关键证据）。
    // 句柄登记为进程全局：停止看门狗 exit(0) 前能显式刷盘，serve 返回路径
    // 也统一经 flush_before_exit 落盘（静态量不随进程退出自动析构）。
    if let Some(guard) = jchtools_foundation::xberg_settings::state_dir()
        .ok()
        .as_deref()
        .and_then(jchtools_foundation::logging::init)
    {
        jchtools_foundation::logging::hold_for_process(guard);
    }
    // P-10：guard 存于静态量后不随 panic 展开析构；用本地 RAII 兜住
    // serve() 的 unwind 路径（正常/错误退出路径由下方 flush_before_exit
    // 显式覆盖，两者经同一互斥量幂等）。
    let _flush_on_unwind = jchtools_foundation::logging::FlushOnDrop;
    let served = service();
    if let Err(error) = &served {
        report_failure(error);
    }
    jchtools_foundation::logging::flush_before_exit();
    if served.is_err() {
        std::process::exit(1);
    }
}
fn main() {
    let mode = std::env::args_os().nth(1);
    if mode.as_deref() == Some(std::ffi::OsStr::new("--codeagent-service")) {
        let stopping =
            std::env::args_os().nth(2).as_deref() == Some(std::ffi::OsStr::new("--stop"));
        run_background(
            || {
                if stopping {
                    jchtools_codeagent::codeagent::stop()
                } else {
                    jchtools_codeagent::codeagent::serve()
                }
            },
            |_| {
                tracing::error!(
                    event = "application_failed",
                    component = "codeagent",
                    stage = "serve",
                    error_type = "service_failure",
                    exit_code = 1,
                    "CodeAgent 后台异常退出"
                );
            },
        );
        return;
    }
    if mode.as_deref() == Some(std::ffi::OsStr::new("--acp-http-service")) {
        let stopping =
            std::env::args_os().nth(2).as_deref() == Some(std::ffi::OsStr::new("--stop"));
        let stage = if stopping {
            "maintenance_stop"
        } else {
            "serve"
        };
        // serve 已排空自有资源，但原认证控制入口和单实例所有权一直保留到 OS 退出；
        // 维护命令在下面的日志刷盘尾段仍能绑定原进程对象等待，不把 serve 返回当退出。
        run_background(
            || {
                if stopping {
                    jchtools_acp_api::acp_api::runtime::stop_for_maintenance()
                } else {
                    jchtools_acp_api::acp_api::runtime::serve()
                }
            },
            |error| {
                tracing::error!(
                    event = "application_failed",
                    component = "acp_http_service",
                    stage = stage,
                    error_type = ?error.kind,
                    exit_code = 1,
                    "模型服务后台操作失败"
                );
            },
        );
        return;
    }
    if mode.as_deref() == Some(std::ffi::OsStr::new("--xberg-broker")) {
        run_background(jchtools_xberg::xberg_runtime::serve, |_| {
            tracing::error!(
                event = "application_failed",
                component = "xberg_broker",
                stage = "serve",
                error_type = "broker_failure",
                exit_code = 1,
                "共享代理异常退出"
            );
        });
        return;
    }
    if let Err(error) = jchtools_gui::gui::run() {
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

#[cfg(test)]
mod tests {
    use std::fs;

    /// 回归（2026-10-24）：bin 的测试骨架曾是 GUI 子系统（windows_subsystem 为
    /// crate 级属性），验收脚本用 PowerShell `&` 直调骨架时不等待、不接输出、
    /// 不设退出码，阶段日志缺失并中断验收。生产构建仍须是 GUI 子系统，测试
    /// 构建必须是控制台子系统，此处自检当前骨架自身的 PE Subsystem 字段。
    #[test]
    fn bin_test_harness_uses_console_subsystem() {
        let exe = std::env::current_exe().expect("取得当前测试骨架路径");
        let image = fs::read(&exe).expect("读取测试骨架 PE 镜像");
        let pe_offset = u32::from_le_bytes(
            image[0x3C..0x40].try_into().expect("DOS 头 e_lfanew 完整"),
        ) as usize;
        assert_eq!(&image[pe_offset..pe_offset + 4], b"PE\0\0", "PE 签名");
        let optional_header = pe_offset + 24;
        let magic = u16::from_le_bytes(
            image[optional_header..optional_header + 2]
                .try_into()
                .expect("可选头 magic 完整"),
        );
        assert!(
            magic == 0x10B || magic == 0x20B,
            "未知可选头格式：0x{magic:X}"
        );
        const SUBSYSTEM_CONSOLE: u16 = 3;
        let subsystem = u16::from_le_bytes(
            image[optional_header + 68..optional_header + 70]
                .try_into()
                .expect("Subsystem 字段完整"),
        );
        assert_eq!(
            subsystem, SUBSYSTEM_CONSOLE,
            "测试骨架必须是控制台子系统，否则验收驱动无法采集结果"
        );
    }
}

