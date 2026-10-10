//! AH-16/AH-A15：真实后台及官方 SDK Agent 的子进程环境保护；不调用真实模型。
// 真实 ACP 后台只支持 Windows，内部主程序入口及隔离钩子分别需要 gui、test-hooks。
#![cfg(all(windows, feature = "gui", feature = "test-hooks"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "fixtures/acp_api/main.rs"]
mod fixture;

use fixture::{
    consumer::{audit, body, completion, config, unused_port, wait_event, Http},
    Plan,
};
use jchtools::acp_api::{runtime, settings, ServicePhase};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

const TEST_NAME: &str = "owned_agent_disables_discovery_without_mutating_parent_or_config";
const CHILD_ROOT: &str = "JCHTOOLS_ACP_GUARD_TEST_ROOT";
const CHILD_ACTION: &str = "JCHTOOLS_ACP_GUARD_TEST_ACTION";
const GUARD: &str = "OMP_JCHTOOLS_DISCOVERY";

struct Harness {
    temp: tempfile::TempDir,
    root: PathBuf,
    cleaned: bool,
}
impl Harness {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        std::fs::create_dir(&root).unwrap();
        Self {
            temp,
            root,
            cleaned: false,
        }
    }
    fn launch(&self, action: &str) -> Child {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(CHILD_ROOT, &self.root)
            .env(CHILD_ACTION, action)
            .env("JCHTOOLS_TEST_STATE_DIR", &self.root)
            .env(
                "JCHTOOLS_TEST_ACP_SERVICE_EXE",
                env!("CARGO_BIN_EXE_JchTools"),
            )
            .env(
                "JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT",
                self.temp.path().join("absent-snapshot-assets"),
            )
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        if action == "unset" || action == "cleanup" {
            command.env_remove(GUARD);
        } else {
            command.env(GUARD, action);
        }
        command.spawn().unwrap()
    }
    fn cleanup(&mut self) {
        let exit = finish_child(self.launch("cleanup")).unwrap();
        assert!(exit.success(), "隔离后台清理失败：{exit}");
        self.cleaned = true;
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        if !self.cleaned {
            // 断言失败时也仅请求本测试隔离实例安全 Stop；不杀 Agent、不触碰用户实例。
            let result = finish_child(self.launch("cleanup"));
            if !matches!(result, Ok(exit) if exit.success()) {
                eprintln!("ACP 环境测试隔离清理失败：{result:?}");
            }
        }
    }
}
fn finish_child(mut child: Child) -> std::io::Result<ExitStatus> {
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if let Some(exit) = child.try_wait()? {
            return Ok(exit);
        }
        if Instant::now() >= deadline {
            // 只结束本测试拥有的控制消费者，后台和 Agent 另经隔离 Stop 清理。
            child.kill()?;
            let _exit = child.wait()?;
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "ACP 环境测试消费者未及时收尾",
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}
fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_FAILED, WAIT_TIMEOUT},
        System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
    };
    // SAFETY: 只以同步权限查询本隔离测试记录的自有 PID，不修改或终止进程。
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return false;
    }
    // SAFETY: 有效进程句柄，零时等待只查询进程对象是否已完成退出。
    let waited = unsafe { WaitForSingleObject(handle, 0) };
    // SAFETY: 本次取得的唯一进程句柄在查询完成后关闭。
    unsafe { CloseHandle(handle) };
    assert_ne!(waited, WAIT_FAILED, "无法观察自有进程的实际退出");
    waited == WAIT_TIMEOUT
}
fn wait_gone(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while process_alive(pid) {
        assert!(Instant::now() < deadline, "自有进程 {pid} 未安全退出");
        thread::sleep(Duration::from_millis(10));
    }
}
fn stop_isolated_service() {
    let before = runtime::status().unwrap();
    runtime::stop().unwrap();
    for pid in [before.agent_pid, before.service_pid].into_iter().flatten() {
        wait_gone(pid);
    }
}
fn exercise_parent_environment(root: &Path, action: &str) {
    let inherited = std::env::var_os(GUARD);
    let expected = (action != "unset").then(|| std::ffi::OsString::from(action));
    assert_eq!(inherited, expected, "消费者必须实际继承指定父环境");
    assert!(settings::load_config().unwrap().is_none());

    let audit_root = root.parent().unwrap().join("Agent 参数 中文 空格");
    std::fs::create_dir(&audit_root).unwrap();
    let saved = config(
        env!("CARGO_BIN_EXE_jchtools-acp-fixture"),
        &audit_root,
        unused_port(),
        "normal",
    );
    runtime::save_config(&saved).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let ready = loop {
        let state = runtime::status().unwrap();
        if state.phase == ServicePhase::Ready {
            break state;
        }
        assert_ne!(state.phase, ServicePhase::Error, "{state:?}");
        assert!(Instant::now() < deadline, "后台未就绪：{state:?}");
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(ready.saved_config.as_ref(), Some(&saved));
    assert_eq!(ready.running_config.as_ref(), Some(&saved));
    assert_eq!(std::env::var_os(GUARD), inherited);
    let events = audit(&audit_root);
    let spawns = events
        .iter()
        .filter(|event| event["event"] == "spawn")
        .collect::<Vec<_>>();
    assert_eq!(spawns.len(), 1, "{events:?}");
    let spawn = spawns[0];
    assert_eq!(spawn["omp_jchtools_discovery"], "0");
    assert_eq!(spawn["pid"].as_u64(), ready.agent_pid.map(u64::from));
    let arguments = spawn["argv"]
        .as_array()
        .unwrap()
        .iter()
        .skip(1)
        .map(|value| value.as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(arguments, saved.arguments, "保护不得改写用户启动参数");
    assert_eq!(
        PathBuf::from(spawn["cwd"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        root.join("acp-workspace").canonicalize().unwrap(),
    );

    let plan = Plan::new("text");
    let response = Http::open(
        saved.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&plan, false)),
        None,
    );
    assert_eq!(response.status, 200);
    assert_eq!(completion(&response.json()), plan.text());
    assert_eq!(
        wait_event(&audit_root, "completed", &plan.tag)["pid"],
        spawn["pid"],
        "正常请求必须由已检查环境的唯一 Agent 完成",
    );
    assert_eq!(std::env::var_os(GUARD), inherited);
    assert_eq!(settings::load_config().unwrap(), Some(saved.clone()));

    let stopped = runtime::stop().unwrap();
    assert_eq!(stopped.phase, ServicePhase::Stopped);
    assert!(stopped.agent_pid.is_none());
    assert!(stopped.running_config.is_none());
    wait_gone(ready.agent_pid.unwrap());
    wait_gone(ready.service_pid.unwrap());
    assert!(std::net::TcpStream::connect(("127.0.0.1", saved.port)).is_err());
    assert_eq!(std::env::var_os(GUARD), inherited);
    assert_eq!(settings::load_config().unwrap(), Some(saved));
}

/// 覆盖 AH-16/AH-A15：unset / 1 / 0 均实际启动 Agent，父环境和持久程序/参数保持不变。
#[test]
fn owned_agent_disables_discovery_without_mutating_parent_or_config() {
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let action = std::env::var(CHILD_ACTION).unwrap();
        if action == "cleanup" {
            stop_isolated_service();
        } else {
            exercise_parent_environment(&PathBuf::from(root), &action);
        }
        return;
    }
    let parent_environment = std::env::var_os(GUARD);
    for action in ["unset", "1", "0"] {
        let mut harness = Harness::new();
        let exit = finish_child(harness.launch(action)).unwrap();
        if exit.success() {
            // 消费者成功返回前已证明 Stop、双 PID 退出及端口关闭；仅失败路径兜底清理。
            harness.cleaned = true;
        } else {
            harness.cleanup();
        }
        assert!(exit.success(), "父环境 {action} 的隔离消费者失败：{exit}");
        assert_eq!(std::env::var_os(GUARD), parent_environment);
    }
}
