//! 控制台入口；共享运行时以内部代理模式重新启动本程序。
fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let code = if args.first().is_some_and(|arg| arg == "--xberg-broker") {
        if let Some(guard) = jchtools_foundation::xberg_settings::state_dir()
            .ok()
            .as_deref()
            .and_then(jchtools_foundation::logging::init)
        {
            jchtools_foundation::logging::hold_for_process(guard);
        }
        let _flush_on_unwind = jchtools_foundation::logging::FlushOnDrop;
        let result = jchtools_xberg::xberg_runtime::serve();
        if result.is_err() {
            tracing::error!(
                event = "application_failed",
                component = "xberg_broker",
                stage = "serve",
                error_type = "broker_failure",
                exit_code = 1,
                "共享代理异常退出"
            );
        }
        jchtools_foundation::logging::flush_before_exit();
        i32::from(result.is_err())
    } else {
        jchocr::jchocr::run(&args)
    };
    if code != 0 {
        std::process::exit(code);
    }
}
