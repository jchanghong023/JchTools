//! 无业务依赖的共享基础设施：诊断日志、性能打点、应用配置与系统代理。
pub mod logging;
pub mod perf;
pub mod system_proxy;
pub mod xberg_settings;

/// 隔离测试支持；环境覆盖只在测试或显式 `test-hooks` 特性下可用。
#[cfg(any(test, feature = "test-hooks"))]
pub mod test_support;
