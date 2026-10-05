#![allow(clippy::unwrap_used)]
//! S5-01 回归：引擎会话清场只结束本仓测试派生的 xberg.exe（可执行路径在
//! 仓库 `.tmp\` 或系统临时目录的 tempfile 目录内），不按映像名全杀——后者会
//! 误杀用户自行运行的 Xberg（与 XB-23「不操作用户自行启动的其他 Xberg」
//! 同口径；修复前 tests/common 的清场执行 `taskkill /IM xberg.exe /F`）。

// 本二进制只消费 session_lock / cleanup_stray_engines：共享支持模块的其余
// 工具在别的测试二进制中使用，此处允许整体引入而不逐项消警。
#[allow(dead_code)]
mod common;

/// 过滤判定单测：给定候选可执行路径，断言测试派生路径被选中、用户路径被排除
/// （清场脚本与该判定共用同一前缀集合，见 tests/common/mod.rs）。
#[test]
fn engine_cleanup_filter_selects_only_test_owned_paths() {
    let repo = env!("CARGO_MANIFEST_DIR");
    // 仓库 .tmp\ 下（大小写与正斜杠变体）命中。
    assert!(common::is_test_owned_engine_path(&format!(
        r"{repo}\.tmp\mock-engines\123-xberg.exe"
    )));
    let upper_repo = format!("{}\\.TMP\\workspace\\xberg.exe", repo.to_uppercase());
    assert!(common::is_test_owned_engine_path(&upper_repo));
    assert!(common::is_test_owned_engine_path(&format!(
        "{repo}/.tmp/gui-smoke/engine/xberg.exe"
    )));
    // %TEMP%\.tmp*（tempfile::tempdir 目录）命中。
    let tempfile_dir = std::env::temp_dir().join(".tmpAbCdEf").join("xberg.exe");
    assert!(common::is_test_owned_engine_path(
        &tempfile_dir.to_string_lossy()
    ));
    // 用户路径：真实引擎测试目录、任意安装目录、仓库内 .tmp 相邻目录
    // （无分隔符，不得误命中）、空串。
    assert!(!common::is_test_owned_engine_path(
        "C:\\Users\\jiang\\Documents\\xberg-test\\xberg-cli-x86_64-pc-windows-msvc\\xberg.exe"
    ));
    assert!(!common::is_test_owned_engine_path(
        "C:\\Program Files\\Xberg\\xberg.exe"
    ));
    assert!(!common::is_test_owned_engine_path(&format!(
        "{repo}\\.tmp-evil\\xberg.exe"
    )));
    assert!(!common::is_test_owned_engine_path(""));
}

/// 用户路径的 xberg.exe（临时目录中、但不在测试派生前缀内）必须在清场后存活。
#[test]
fn cleanup_keeps_user_owned_xberg_image_alive() {
    // 会话锁：与其他引擎测试二进制互斥，避免清场与真实引擎测试相互干扰。
    let _session = common::session_lock();
    // 诱饵目录：系统临时目录下、但不在 `.tmp*` 前缀内 → 按用户自有路径对待。
    let decoy_dir =
        std::env::temp_dir().join(format!("jchtools-s501-decoy-{}", std::process::id()));
    std::fs::create_dir_all(&decoy_dir).unwrap();
    let decoy_exe = decoy_dir.join("xberg.exe");
    std::fs::copy("C:\\Windows\\System32\\ping.exe", &decoy_exe).unwrap();
    let mut child = std::process::Command::new(&decoy_exe)
        .args(["-n", "60", "127.0.0.1"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // 清场（锁内调用，与各引擎测试同口径）。
    common::cleanup_stray_engines();
    let alive = child.try_wait().unwrap().is_none();
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&decoy_dir);
    assert!(alive, "清场不得按映像名误杀用户路径的 xberg.exe（S5-01）");
}

/// 反向守护：测试派生路径（`%TEMP%\.tmp*`，tempfile::tempdir 前缀）的 xberg.exe
/// 仍是清场目标——收窄清理范围不得让孤儿引擎漏杀（那是本清场存在的目的）。
#[test]
fn cleanup_still_kills_test_owned_xberg_image() {
    let _session = common::session_lock();
    let owned_dir = std::env::temp_dir().join(format!(".tmpjchs501-owned-{}", std::process::id()));
    std::fs::create_dir_all(&owned_dir).unwrap();
    let owned_exe = owned_dir.join("xberg.exe");
    std::fs::copy("C:\\Windows\\System32\\ping.exe", &owned_exe).unwrap();
    let mut child = std::process::Command::new(&owned_exe)
        .args(["-n", "60", "127.0.0.1"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    common::cleanup_stray_engines();
    // Stop-Process 发起后允许短暂的终止传播：有界等待进程退出。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let exited = loop {
        if child.try_wait().unwrap().is_some() {
            break true;
        }
        if std::time::Instant::now() > deadline {
            break false;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    if !exited {
        let _ = child.kill();
        let _ = child.wait();
    }
    let _ = std::fs::remove_dir_all(&owned_dir);
    assert!(
        exited,
        "测试派生路径的 xberg.exe 仍必须被清场结束（S5-01 反向）"
    );
}
