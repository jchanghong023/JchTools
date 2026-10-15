//! Windows 引擎测试跨进程会话锁；各 package 必须传入同一仓库根，不能使用 crate 的临时根。
//! 仅供测试通过 `mod` / `#[path]` 复用，不属于产品运行时。

use std::fs::{File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

/// 会话锁守卫：持有不共享且关闭即删除的文件句柄；异常进程退出也由系统释放。
pub(crate) struct SessionLock {
    _file: File,
}

/// 取仓库根 `.tmp/engine-session.lock`：最多等 180 秒，不按时龄抢占存活持有者。
///
/// 先取锁，再创建临时资产和捕获/修改进程环境；代理、环境恢复和临时目录
/// 必须先于本守卫析构。环境守卫可借用本守卫，约束恢复期间仍持有跨进程锁。
///
/// # Panics
/// 创建锁目录或打开锁文件失败，或等满 180 秒仍拿不到锁时 panic。
pub(crate) fn session_lock(repository_root: &Path) -> SessionLock {
    let path = repository_root.join(".tmp/engine-session.lock");
    std::fs::create_dir_all(path.parent().unwrap())
        .unwrap_or_else(|error| panic!("创建引擎会话锁目录失败：{error}"));
    let deadline = Instant::now() + Duration::from_secs(180);
    // Win32 FILE_FLAG_DELETE_ON_CLOSE：保留句柄期间拒绝共享打开/删除；最后一个
    // 句柄关闭时删除。create(true) 也可安全接管旧版遗留、已无人持有的文件，
    // 不需先检查时龄再删除（该检查会误抢耗时测试，旧守卫还会删掉新持有者的锁）。
    const DELETE_ON_CLOSE: u32 = 0x0400_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const DELETE_ACCESS: u32 = 0x0001_0000;
    loop {
        match OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .access_mode(GENERIC_WRITE | DELETE_ACCESS)
            .share_mode(0)
            .custom_flags(DELETE_ON_CLOSE)
            .open(&path)
        {
            Ok(file) => return SessionLock { _file: file },
            // 共享冲突表示已有持有者；关闭删除的短暂窗口可能返回访问被拒绝。
            Err(error) if matches!(error.raw_os_error(), Some(5 | 32 | 33)) => {}
            Err(error) => panic!("打开引擎会话锁失败（{}）：{error}", path.display()),
        }
        assert!(
            Instant::now() < deadline,
            "引擎会话锁等待超时：仍有测试持有锁或锁目录不可写；不得删除活跃锁"
        );
        std::thread::sleep(Duration::from_millis(300));
    }
}
