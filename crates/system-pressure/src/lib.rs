//! 手动启停的独立系统压力记录器；生产输出位置、周期与保留时长固定。

#[cfg(windows)]
mod collector;
#[cfg(windows)]
mod service;
#[cfg(any(windows, test))]
mod window;

/// D盘存在时固定输出到其根目录，否则使用系统临时目录；写入失败不切换目录。
pub fn output_path() -> std::path::PathBuf {
    #[cfg(feature = "test-hooks")]
    if let Some(path) = std::env::var_os("JCHTOOLS_TEST_SYSTEM_PRESSURE_OUTPUT") {
        return std::path::PathBuf::from(path);
    }
    let root = std::path::Path::new(r"D:\");
    if root.is_dir() {
        root.join("JchTools-performance.csv")
    } else {
        std::env::temp_dir().join("JchTools-performance.csv")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub running: bool,
    pub error: Option<String>,
}

/// 查询真实后台状态；不会启动后台或创建输出文件。
pub fn status() -> Result<Snapshot, String> {
    #[cfg(windows)]
    {
        service::status()
    }
    #[cfg(not(windows))]
    {
        Err("系统压力记录仅支持 Windows".into())
    }
}

/// 手动启动独立后台，首次 CSV 写入成功后才返回正在运行。
pub fn start() -> Result<Snapshot, String> {
    #[cfg(windows)]
    {
        service::start(
            std::env::current_exe()
                .map_err(|error| format!("无法取得系统压力记录程序路径：{error}"))?,
        )
    }
    #[cfg(not(windows))]
    {
        Err("系统压力记录仅支持 Windows".into())
    }
}

/// 等待后台停止后返回；保留已输出的 CSV。
pub fn stop() -> Result<Snapshot, String> {
    #[cfg(windows)]
    {
        service::stop()
    }
    #[cfg(not(windows))]
    {
        Err("系统压力记录仅支持 Windows".into())
    }
}

/// 仅供主程序内部后台模式调用，不依赖 GUI 生命周期。
pub fn serve() -> Result<(), String> {
    #[cfg(windows)]
    {
        service::serve()
    }
    #[cfg(not(windows))]
    {
        Err("系统压力记录仅支持 Windows".into())
    }
}

/// 产品测试使用真实主程序入口，生产启动始终使用当前程序。
#[cfg(all(windows, feature = "test-hooks"))]
pub fn start_for_test(executable: std::path::PathBuf) -> Result<Snapshot, String> {
    service::start(executable)
}
