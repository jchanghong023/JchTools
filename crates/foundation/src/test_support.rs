//! 测试支持：跨 crate 共享的进程环境变量锁。
//!
//! 资产、运行时和设置测试会改写隔离覆盖变量；并行线程共享进程环境，
//! 必须经同一把锁串行，避免相互覆盖。
use std::sync::{LazyLock, Mutex};

/// 所有隔离覆盖使用同一进程级锁，不能由消费 crate 各自定义。
pub fn env_lock() -> &'static Mutex<()> {
    static LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
    &LOCK
}
