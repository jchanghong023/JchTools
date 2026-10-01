//! 引擎集成测试共用工具：跨进程会话锁。
//!
//! 生产语义是每登录会话至多一个 Xberg（xberg_runtime_windows 的进程扫描执法）。
//! cargo test 会并行运行多个测试二进制，各自派生真实代理与引擎时天然互斥——
//! 用 `.tmp/` 下的锁文件把「需要会话引擎」的测试串行化，互斥由文件系统保证，
//! 不依赖时序运气。锁文件陈旧（超过 3 分钟无人续期）时视为持有者已死，直接抢占。

use std::fs::OpenOptions;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// 会话锁守卫：Drop 时删除锁文件。
pub(super) struct SessionLock {
    path: PathBuf,
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// 取会话锁：最多等 180 秒；锁文件存在且超过 3 分钟视为陈旧，直接覆盖抢占。
///
/// # Panics
/// 等满 180 秒仍拿不到锁（前一个持有测试可能卡死）， panic 并提示清理
/// `.tmp/engine-session.lock`。
pub(super) fn session_lock() -> SessionLock {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".tmp/engine-session.lock");
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => return SessionLock { path },
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = std::fs::metadata(&path).is_ok_and(|metadata| {
                    metadata.modified().is_ok_and(|modified| {
                        modified
                            .elapsed()
                            .is_ok_and(|age| age > Duration::from_secs(180))
                    })
                });
                if stale {
                    // 陈旧锁：持有者已死（崩溃/被杀），抢占。
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
            }
            Err(_) => {}
        }
        assert!(
            Instant::now() < deadline,
            "引擎会话锁等待超时：请人工清理 .tmp/engine-session.lock 后重跑"
        );
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// 清理本会话残留的引擎与代理（锁内调用）：上一轮失败的孤儿会占住
/// 会话单引擎执法，让下一轮从头就「已有 Xberg」。只按命令行特征匹配
/// 本项目派生的进程（`--xberg-broker`）与引擎映像名，不触碰其他进程。
pub(super) fn cleanup_stray_engines() {
    // 0) 先停截图服务本体：XB-22 常驻看护会把被杀的代理与引擎按自己的
    //    周期重新拉起，只杀代理/引擎永远赢不了（实测重跑 60s 忙碌超时）；
    //    与下方同口径，仅结束本项目派生的服务进程。GUI 打开时会按 XB-22
    //    重新拉起服务，dev 机测试窗口期内不保留。
    let stop_service = "Get-CimInstance Win32_Process | Where-Object { $_.Name -eq 'snap-ocr-worker.exe' -and $_.CommandLine -match '--service' } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }";
    let _ = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            stop_service,
        ])
        .output();
    std::thread::sleep(Duration::from_millis(300));
    // 1) 再杀「托管代理」：主程序代理与截图服务代理（后者按 XB-22 常驻保活，
    //    会把被杀的引擎立刻重新拉起，必须先于引擎处理）。
    let script = "Get-CimInstance Win32_Process | Where-Object { ($_.Name -eq 'JchTools.exe' -or $_.Name -eq 'snap-ocr-worker.exe') -and $_.CommandLine -match '--xberg-broker' } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }";
    let _ = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .output();
    std::thread::sleep(Duration::from_millis(400));
    // 2) 清引擎，两遍（防监督者在窗口期重启）。
    for _ in 0..2 {
        let _ = std::process::Command::new("taskkill")
            .args(["/IM", "xberg.exe", "/F"])
            .output();
        std::thread::sleep(Duration::from_millis(300));
    }
}
