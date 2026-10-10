//! AH-06～AH-10/AH-13：真实JchTools后台进程、SQLite、单实例及SDK Agent生命周期。
//! 此处合成 Agent 不是实际模型；真实基准为 OpenCode v2.0.26 / OpenCode Zen MiMo-V2.6-Flash Free。
#![allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "fixtures/acp_api/main.rs"]
mod fixture;
use fixture::{
    consumer::{audit, body, completion, config, release, unused_port, wait_event, Http},
    Plan,
};
use jchtools::acp_api::{runtime, settings, ServiceConfig, ServiceStatus};
use serde_json::{json, Value};
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const CHILD_ROOT: &str = "JCHTOOLS_ACP_PROCESS_TEST_ROOT";
fn child_operation() -> bool {
    let Some(root) = std::env::var_os(CHILD_ROOT) else {
        return false;
    };
    let root = PathBuf::from(root);
    let action = std::env::var("JCHTOOLS_ACP_PROCESS_ACTION").unwrap();
    let result = match action.as_str() {
        "save" => runtime::save_config(
            &serde_json::from_str(&std::env::var("JCHTOOLS_ACP_PROCESS_CONFIG").unwrap()).unwrap(),
        ),
        "ensure" => runtime::ensure_started(),
        "status" => runtime::status(),
        "apply" => runtime::apply_and_restart(),
        "queued-apply" => {
            std::fs::write(
                root.join("queued-apply.json"),
                json!({"queued":true}).to_string(),
            )
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(90);
            // 只等待父进程确认旧 daemon 已退役的显式放行，不用延时制造竞态。
            while !root.join("release-queued-apply").exists() {
                assert!(Instant::now() < deadline, "晚到Apply没有收到放行");
                thread::sleep(Duration::from_millis(10));
            }
            runtime::apply_and_restart()
        }
        "discover" => {
            let result = match runtime::discover() {
                Ok(discovery) => json!({"ok":discovery}),
                Err(error) => json!({"error":error}),
            };
            std::fs::write(
                std::env::var_os("JCHTOOLS_ACP_PROCESS_RESULT").unwrap(),
                result.to_string(),
            )
            .unwrap();
            return true;
        }
        "stop" => runtime::stop(),
        "load" => {
            let saved = settings::load_config().unwrap();
            std::fs::write(
                std::env::var_os("JCHTOOLS_ACP_PROCESS_RESULT").unwrap(),
                json!({"config":saved}).to_string(),
            )
            .unwrap();
            return true;
        }
        _ => panic!("未知child操作 {action} 于 {}", root.display()),
    };
    let value = match result {
        Ok(status) => json!({"ok":status}),
        Err(error) => json!({"error":error}),
    };
    std::fs::write(
        std::env::var_os("JCHTOOLS_ACP_PROCESS_RESULT").unwrap(),
        value.to_string(),
    )
    .unwrap();
    true
}
struct Harness {
    temp: tempfile::TempDir,
    root: PathBuf,
    audit: PathBuf,
    executable: PathBuf,
    port: u16,
    release_markers: RefCell<Vec<PathBuf>>,
}
impl Harness {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        std::fs::create_dir(&root).unwrap();
        let audit = temp.path().join("参数 中文 空格");
        std::fs::create_dir(&audit).unwrap();
        let directory = temp.path().join("程序 中文 空格");
        std::fs::create_dir(&directory).unwrap();
        let executable = directory.join("SDK Agent.exe");
        std::fs::copy(env!("CARGO_BIN_EXE_jchtools-acp-fixture"), &executable).unwrap();
        Self {
            temp,
            root,
            audit,
            executable,
            port: unused_port(),
            release_markers: RefCell::new(Vec::new()),
        }
    }
    fn config(&self, mode: &str) -> ServiceConfig {
        config(
            self.executable.to_str().unwrap(),
            &self.audit,
            self.port,
            mode,
        )
    }
    fn register_hold(&self, plan: &Plan) {
        let mut markers = self.release_markers.borrow_mut();
        if plan.barrier {
            markers.push(self.audit.join(format!("release-{}", plan.tag)));
        }
        if plan.action == "terminal-held" {
            markers.push(
                self.root
                    .join("acp-workspace")
                    .join(format!("terminal-release-{}", plan.tag)),
            );
        }
    }
    fn launch(&self, test: &str, action: &str, config: Option<&ServiceConfig>) -> (Child, PathBuf) {
        let result = self
            .temp
            .path()
            .join(format!("result-{}.json", uuid::Uuid::new_v4()));
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", test, "--nocapture"])
            .env(CHILD_ROOT, &self.root)
            .env("JCHTOOLS_ACP_PROCESS_ACTION", action)
            .env("JCHTOOLS_ACP_PROCESS_RESULT", &result)
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
        if let Some(config) = config {
            command.env(
                "JCHTOOLS_ACP_PROCESS_CONFIG",
                serde_json::to_string(config).unwrap(),
            );
        }
        (command.spawn().unwrap(), result)
    }
    fn call(&self, test: &str, action: &str, config: Option<&ServiceConfig>) -> Value {
        let (child, result) = self.launch(test, action, config);
        finish_child(child, &result)
    }
    fn ready(&self, test: &str) -> ServiceStatus {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let result = self.call(test, "status", None);
            if result["ok"]["phase"] == "ready" {
                return serde_json::from_value(result["ok"].clone()).unwrap();
            }
            assert!(Instant::now() < deadline, "后台未就绪: {result}");
            thread::sleep(Duration::from_millis(20));
        }
    }
    fn phase(&self, test: &str, phase: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let result = self.call(test, "status", None);
            if result["ok"]["phase"] == phase {
                return result["ok"].clone();
            }
            assert!(Instant::now() < deadline, "没有进入{phase}: {result}");
            thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        // AH-10/AH-13：先放行自有探针，再请求产品安全Stop；绝不强杀后台/Agent/终端。
        for marker in self.release_markers.get_mut().iter() {
            let _released = std::fs::write(marker, b"cleanup");
        }
        let _released = std::fs::write(self.audit.join("release-exit"), b"cleanup");
        let result = self.temp.path().join("cleanup-result.json");
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "cleanup_isolated_service", "--nocapture"])
            .env(CHILD_ROOT, &self.root)
            .env("JCHTOOLS_ACP_PROCESS_ACTION", "stop")
            .env("JCHTOOLS_ACP_PROCESS_RESULT", result)
            .env("JCHTOOLS_TEST_STATE_DIR", &self.root)
            .env(
                "JCHTOOLS_TEST_ACP_SERVICE_EXE",
                env!("CARGO_BIN_EXE_JchTools"),
            )
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = child {
            if wait_consumer(&mut child, Duration::from_secs(30)).is_none() {
                // 仅终止本测试spawn的cleanup消费者；不终止它控制的产品进程。
                let _killed = child.kill();
                let _exit = wait_consumer(&mut child, Duration::from_secs(5));
            }
        }
    }
}
fn wait_consumer(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(_) => return None,
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
}
fn finish_child(mut child: Child, path: &Path) -> Value {
    let status = wait_consumer(&mut child, Duration::from_secs(30));
    if let Some(status) = status {
        assert!(status.success(), "child {status}");
        return serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    }
    let pid = child.id();
    // 只回收直接spawn的控制消费者，产品的安全Stop仍由Harness Drop请求。
    let _killed = child.kill();
    let _exit = wait_consumer(&mut child, Duration::from_secs(5));
    panic!("控制消费者未安全收尾，pid {pid}");
}

// 必须先放行并回收晚到的控制消费者，再由 Harness 的有界 Stop 清理自有服务。
struct ReleaseMarker {
    path: PathBuf,
    consumer: Option<Child>,
}
impl ReleaseMarker {
    fn finish(mut self, result: &Path) -> Value {
        std::fs::write(&self.path, b"release").unwrap();
        finish_child(self.consumer.take().unwrap(), result)
    }
}
impl Drop for ReleaseMarker {
    fn drop(&mut self) {
        if let Some(mut child) = self.consumer.take() {
            let _released = std::fs::write(&self.path, b"cleanup");
            if wait_consumer(&mut child, Duration::from_secs(30)).is_none() {
                // 仅回收本测试直接 spawn 的消费者，不强杀后台或 Agent。
                let _killed = child.kill();
                let _exit = wait_consumer(&mut child, Duration::from_secs(5));
            }
        }
    }
}
struct ObservedProcess {
    pid: u32,
    handle: std::os::windows::io::OwnedHandle,
}
impl ObservedProcess {
    fn try_open(pid: u32) -> Option<Self> {
        use std::os::windows::io::{FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        };
        // SAFETY: 只取得本测试自有进程的查询/同步权限，不授予终止权限。
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
        if handle.is_null() {
            None
        } else {
            Some(Self {
                pid,
                // SAFETY: 有效句柄只移交一次，OwnedHandle 在所有路径关闭它。
                handle: unsafe { OwnedHandle::from_raw_handle(handle) },
            })
        }
    }
    fn open(pid: u32) -> Self {
        Self::try_open(pid).unwrap_or_else(|| {
            panic!(
                "无法观察自有进程 {pid}：{}",
                std::io::Error::last_os_error()
            )
        })
    }
    fn is_alive(&self) -> bool {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::{
            Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
            System::Threading::WaitForSingleObject,
        };
        // SAFETY: 持有有效同步句柄，只读取 OS 状态，不修改/终止进程。
        let state = unsafe { WaitForSingleObject(self.handle.as_raw_handle(), 0) };
        assert!(
            matches!(state, WAIT_OBJECT_0 | WAIT_TIMEOUT),
            "进程 {} 退出观察失败：{}",
            self.pid,
            std::io::Error::last_os_error()
        );
        state == WAIT_TIMEOUT
    }
    fn wait_gone(&self) {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::{
            Foundation::WAIT_OBJECT_0, System::Threading::WaitForSingleObject,
        };
        // GetExitCodeProcess 可先于 process object signaled 发布退出码；
        // 与产品 try_wait 同样等 OS 确认退出，且持有句柄避免等待期间 PID 复用。
        // SAFETY: 有效句柄保持到 Drop，等待限于原有 15 秒，不修改/终止进程。
        let exited = unsafe { WaitForSingleObject(self.handle.as_raw_handle(), 15_000) };
        assert_eq!(exited, WAIT_OBJECT_0, "进程 {} 没有释放", self.pid);
    }
}
fn process_alive(pid: u32) -> bool {
    ObservedProcess::try_open(pid).is_some_and(|process| process.is_alive())
}
fn wait_gone(pid: u32) {
    if let Some(process) = ObservedProcess::try_open(pid) {
        process.wait_gone();
    }
}

/// 异常路径清理也通过真实产品stop；无配置时验证返回可分类结果，不启动服务。
#[test]
fn cleanup_isolated_service() {
    if child_operation() {
        return;
    }
    let harness = Harness::new();
    let result = harness.call("cleanup_isolated_service", "status", None);
    assert_eq!(result["ok"]["phase"], "unconfigured");
    assert!(result["ok"]["service_pid"].is_null());
    assert!(result["ok"]["agent_pid"].is_null());
    assert!(audit(&harness.audit).is_empty(), "无有效配置不得启动Agent");
}

/// 覆盖 AH-06/AH-07/AH-08/AH-09：程序/参数空格中文实际启动；client退出后旧新HTTP请求继续；新client连同一PID。
#[test]
fn independent_service_survives_facade_clients_and_reuses_one_agent() {
    if child_operation() {
        return;
    }
    let harness = Harness::new();
    let config = harness.config("normal");
    let saved = harness.call(
        "independent_service_survives_facade_clients_and_reuses_one_agent",
        "save",
        Some(&config),
    );
    assert!(saved.get("error").is_none(), "{saved}");
    let before = harness.ready("independent_service_survives_facade_clients_and_reuses_one_agent");
    assert_ne!(before.service_pid, Some(std::process::id()));
    let service_pid = before.service_pid.unwrap();
    let agent_pid = before.agent_pid.unwrap();
    assert_ne!(service_pid, agent_pid);
    assert!(process_alive(service_pid));
    assert!(process_alive(agent_pid));
    // Windows系统网络表的实际绑定，不用HTTP可达性推断没有局域网监听。
    let network = Command::new("netstat.exe")
        .args(["-ano", "-p", "TCP"])
        .output()
        .unwrap();
    assert!(network.status.success());
    let text = String::from_utf8_lossy(&network.stdout);
    let suffix = format!(":{}", harness.port);
    let pid_text = service_pid.to_string();
    let bindings = text
        .lines()
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            (fields.len() >= 5
                && fields[0] == "TCP"
                && fields[1].ends_with(&suffix)
                && fields[fields.len() - 1] == pid_text)
                .then(|| fields[1].to_owned())
        })
        .collect::<Vec<_>>();
    assert_eq!(bindings, vec![format!("127.0.0.1:{}", harness.port)]);
    let mut plan = Plan::new("text");
    plan.barrier = true;
    harness.register_hold(&plan);
    let mut response = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&plan, true)),
        None,
    );
    assert_eq!(response.delta(), plan.chunks[0]);
    // 两个启动/连接调用来自不同进程；退出这些消费者不得关掉HTTP或Agent。
    for _ in 0..2 {
        let connected = harness.call(
            "independent_service_survives_facade_clients_and_reuses_one_agent",
            "ensure",
            None,
        );
        assert_eq!(connected["ok"]["service_pid"], service_pid);
        assert_eq!(connected["ok"]["agent_pid"], agent_pid);
    }
    release(&harness.audit, &plan);
    assert_eq!(response.finish_stream(), plan.chunks[1]);
    let next = Plan::new("text");
    let response = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&next, false)),
        None,
    );
    assert_eq!(response.status, 200);
    assert_eq!(completion(&response.json()), next.text());
    assert_eq!(
        wait_event(&harness.audit, "started", &next.tag)["pid"],
        agent_pid
    );
    let events = audit(&harness.audit);
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        1
    );
    let spawn = events
        .iter()
        .find(|event| event["event"] == "spawn")
        .unwrap();
    let workspace = harness.root.join("acp-workspace").canonicalize().unwrap();
    assert_eq!(
        PathBuf::from(spawn["cwd"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        workspace
    );
    for session in events.iter().filter(|event| event["event"] == "session") {
        let cwd = PathBuf::from(session["cwd"].as_str().unwrap());
        assert!(cwd.is_absolute());
        assert_eq!(cwd.canonicalize().unwrap(), workspace);
    }
    assert_eq!(spawn["argv"][1], "--agent");
    assert_eq!(
        PathBuf::from(spawn["argv"][2].as_str().unwrap()),
        harness.audit
    );
    let persisted = harness.call(
        "independent_service_survives_facade_clients_and_reuses_one_agent",
        "load",
        None,
    );
    assert_eq!(persisted["config"], serde_json::to_value(&config).unwrap());
    let stopped = harness.call(
        "independent_service_survives_facade_clients_and_reuses_one_agent",
        "stop",
        None,
    );
    assert_eq!(stopped["ok"]["phase"], "stopped");
    wait_gone(service_pid);
    wait_gone(agent_pid);
    assert!(std::net::TcpStream::connect(("127.0.0.1", harness.port)).is_err());
}

/// 覆盖 AH-09：save立即持久不打断请求；apply停止接新、等待已入队请求结束才切换端口/argv。
#[test]
fn saving_is_non_disruptive_and_apply_drains_before_switching() {
    if child_operation() {
        return;
    }
    let test = "saving_is_non_disruptive_and_apply_drains_before_switching";
    let harness = Harness::new();
    let original = harness.config("normal");
    assert!(harness
        .call(test, "save", Some(&original))
        .get("error")
        .is_none());
    let running = harness.ready(test);
    let mut plan = Plan::new("text");
    plan.barrier = true;
    harness.register_hold(&plan);
    let mut stream = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&plan, true)),
        None,
    );
    assert_eq!(stream.delta(), plan.chunks[0]);
    let queued = Plan::new("text");
    let session = stream.headers["x-jchtools-session-id"].clone();
    let port = harness.port;
    let queued_body = json!({"model":fixture::MODEL_A,"messages":[{"role":"user","content":plan.prompt()},{"role":"assistant","content":plan.text()},{"role":"user","content":queued.prompt()}]});
    let queued_job = thread::spawn(move || {
        let response = Http::open(
            port,
            "POST",
            "/v1/chat/completions",
            Some(&queued_body),
            Some(&session),
        );
        assert_eq!(response.status, 200);
        response.json()
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let status = harness.call(test, "status", None);
        if status["ok"]["waiting"] == 1 {
            break;
        }
        assert!(Instant::now() < deadline, "续会话等待未入队: {status}");
        thread::sleep(Duration::from_millis(10));
    }
    let mut changed = original.clone();
    changed.port = unused_port();
    let new_audit = harness.temp.path().join("应用后 参数");
    std::fs::create_dir(&new_audit).unwrap();
    changed.arguments[1] = new_audit.to_string_lossy().into_owned();
    let saved = harness.call(test, "save", Some(&changed));
    assert_eq!(
        saved["ok"]["saved_config"],
        serde_json::to_value(&changed).unwrap()
    );
    assert_eq!(
        saved["ok"]["running_config"],
        serde_json::to_value(&original).unwrap()
    );
    assert_eq!(saved["ok"]["agent_pid"], running.agent_pid.unwrap());
    assert_eq!(
        harness.call(test, "load", None)["config"],
        serde_json::to_value(&changed).unwrap()
    );
    let (apply, result) = harness.launch(test, "apply", None);
    harness.phase(test, "draining");
    assert!(process_alive(running.agent_pid.unwrap()));
    assert_admission_closed(harness.port);
    assert!(!result.exists(), "已有请求结束前不得应用完成");
    assert!(audit(&new_audit).is_empty(), "drain前不得增开Agent");
    release(&harness.audit, &plan);
    assert_eq!(stream.finish_stream(), plan.chunks[1]);
    assert_eq!(completion(&queued_job.join().unwrap()), queued.text());
    assert_eq!(
        wait_event(&harness.audit, "started", &queued.tag)["pid"],
        running.agent_pid.unwrap()
    );
    let applied = finish_child(apply, &result);
    assert_eq!(
        applied["ok"]["running_config"],
        serde_json::to_value(&changed).unwrap()
    );
    let next = Plan::new("text");
    let response = Http::open(
        changed.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&next, false)),
        None,
    );
    assert_eq!(response.status, 200);
    assert_eq!(completion(&response.json()), next.text());
    wait_gone(running.agent_pid.unwrap());
    assert_ne!(
        wait_event(&new_audit, "started", &next.tag)["pid"],
        running.agent_pid.unwrap()
    );
    assert!(std::net::TcpStream::connect(("127.0.0.1", harness.port)).is_err());
    let stopped = harness.call(test, "stop", None);
    assert_eq!(stopped["ok"]["phase"], "stopped");
}

/// 覆盖 AH-09/AH-10/AH-13：另一消费者的Stop必须打断Apply的drain等待，而不是排在其后。
#[test]
fn stop_during_apply_cancels_barrier_without_starting_replacement_agent() {
    if child_operation() {
        return;
    }
    let test = "stop_during_apply_cancels_barrier_without_starting_replacement_agent";
    let harness = Harness::new();
    let original = harness.config("barrier-held");
    let saved = harness.call(test, "save", Some(&original));
    assert!(saved.get("error").is_none(), "{saved}");
    let running = harness.ready(test);
    let mut plan = Plan::new("text");
    plan.barrier = true;
    harness.register_hold(&plan);
    let mut stream = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&plan, true)),
        None,
    );
    assert_eq!(stream.delta(), plan.chunks[0]);
    let replacement_audit = harness.temp.path().join("不得启动的替换Agent");
    std::fs::create_dir(&replacement_audit).unwrap();
    let mut changed = original;
    changed.port = unused_port();
    changed.arguments[1] = replacement_audit.to_string_lossy().into_owned();
    let saved = harness.call(test, "save", Some(&changed));
    assert!(saved.get("error").is_none(), "{saved}");
    let (apply, apply_result) = harness.launch(test, "apply", None);
    let draining = harness.phase(test, "draining");
    assert_eq!(draining["agent_pid"], running.agent_pid.unwrap());
    assert!(!apply_result.exists(), "barrier仍持有时Apply必须等待");
    assert!(audit(&replacement_audit).is_empty());
    // 不释放barrier：只能由第二个真实控制消费者的Stop取消本轮次。
    let (stop, stop_result) = harness.launch(test, "stop", None);
    wait_event(&harness.audit, "cancelled", &plan.tag);
    assert!(!harness.audit.join(format!("release-{}", plan.tag)).exists());
    let cancelled = stream.sse();
    assert!(cancelled.contains("error"), "{cancelled}");
    assert!(
        !cancelled.contains("\"finish_reason\":\"stop\""),
        "{cancelled}"
    );
    let stopped = finish_child(stop, &stop_result);
    assert_eq!(stopped["ok"]["phase"], "stopped");
    assert!(stopped["ok"]["agent_pid"].is_null());
    let applied = finish_child(apply, &apply_result);
    assert!(
        applied.get("error").is_some() || applied["ok"]["phase"] == "stopped",
        "被Stop抢占的Apply不得重新报告Ready: {applied}"
    );
    wait_gone(running.agent_pid.unwrap());
    wait_gone(running.service_pid.unwrap());
    assert!(
        audit(&replacement_audit).is_empty(),
        "Stop不得触发替换Agent"
    );
    assert_eq!(
        audit(&harness.audit)
            .iter()
            .filter(|entry| entry["event"] == "spawn")
            .count(),
        1
    );
    let final_status = harness.call(test, "status", None);
    assert_eq!(final_status["ok"]["phase"], "stopped");
    assert!(final_status["ok"]["agent_pid"].is_null());
    assert!(std::net::TcpStream::connect(("127.0.0.1", changed.port)).is_err());
}

/// 覆盖 AH-06/AH-08/AH-09：首次端口冲突保留可查询后台，保存后允许GUI应用并直接Apply恢复同一后台。
#[test]
fn occupied_initial_port_preserves_service_and_apply_recovers_without_stop() {
    if child_operation() {
        return;
    }
    let test = "occupied_initial_port_preserves_service_and_apply_recovers_without_stop";
    let harness = Harness::new();
    let occupied = std::net::TcpListener::bind(("127.0.0.1", harness.port)).unwrap();
    let saved = harness.call(test, "save", Some(&harness.config("normal")));
    assert!(saved.get("error").is_none(), "{saved}");
    let failed = harness.phase(test, "error");
    let service_pid = u32::try_from(failed["service_pid"].as_u64().unwrap()).unwrap();
    assert!(process_alive(service_pid), "端口冲突不得退出控制后台");
    assert!(failed["agent_pid"].is_null());
    assert!(failed["error"]
        .as_str()
        .unwrap()
        .contains(&harness.port.to_string()));
    for _ in 0..3 {
        let queried = harness.call(test, "status", None);
        assert_eq!(queried["ok"]["phase"], "error");
        assert_eq!(queried["ok"]["service_pid"], service_pid);
        assert!(process_alive(service_pid));
    }
    assert!(audit(&harness.audit).is_empty(), "端口占用不得启动Agent");
    let mut changed = harness.config("normal");
    changed.port = unused_port();
    let saved = harness.call(test, "save", Some(&changed));
    assert_eq!(saved["ok"]["service_pid"], service_pid);
    assert_eq!(saved["ok"]["phase"], "error", "save不得暗中重启");
    let saved_status: ServiceStatus = serde_json::from_value(saved["ok"].clone()).unwrap();
    assert!(
        saved_status.pending_apply(),
        "首次启动失败后保存可用端口必须允许GUI应用，即使尚无运行实例: {saved}"
    );
    assert_eq!(
        harness.call(test, "load", None)["config"],
        serde_json::to_value(&changed).unwrap()
    );
    // 原端口继续由本测试占用，且不先Stop；Apply必须使用刚保存的可用端口。
    let applied = harness.call(test, "apply", None);
    assert_eq!(applied["ok"]["phase"], "ready", "{applied}");
    assert_eq!(applied["ok"]["service_pid"], service_pid);
    assert_eq!(
        applied["ok"]["running_config"],
        serde_json::to_value(&changed).unwrap()
    );
    let next = Plan::new("text");
    let response = Http::open(
        changed.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&next, false)),
        None,
    );
    assert_eq!(response.status, 200);
    assert_eq!(completion(&response.json()), next.text());
    assert_eq!(
        audit(&harness.audit)
            .iter()
            .filter(|entry| entry["event"] == "spawn")
            .count(),
        1
    );
    drop(occupied);
    assert_eq!(harness.call(test, "stop", None)["ok"]["phase"], "stopped");
    wait_gone(service_pid);
}

/// 覆盖 AH-08/AH-13：不提供ACP且立即自然退出的Agent，其旧PID不得被status重新公布。
#[test]
fn immediately_exited_agent_pid_stays_absent_across_status_queries() {
    if child_operation() {
        return;
    }
    let test = "immediately_exited_agent_pid_stays_absent_across_status_queries";
    let harness = Harness::new();
    let tag = uuid::Uuid::new_v4().to_string();
    let mut short_lived = harness.config("normal");
    short_lived.arguments = vec![
        "--terminal".into(),
        harness.audit.to_string_lossy().into_owned(),
        tag.clone(),
        "terminal".into(),
    ];
    // 既有fixture的terminal模式记录自身PID，输出非ACP字节后立即以7退出。
    let saved = harness.call(test, "save", Some(&short_lived));
    assert!(saved.get("error").is_none(), "{saved}");
    let pid_file = harness.audit.join(format!("terminal-{tag}.pid"));
    let deadline = Instant::now() + Duration::from_secs(15);
    let pid = loop {
        if let Ok(text) = std::fs::read_to_string(&pid_file) {
            if let Ok(pid) = text.parse::<u32>() {
                break pid;
            }
        }
        assert!(Instant::now() < deadline, "短寿命Agent没有记录真实PID");
        thread::sleep(Duration::from_millis(10));
    };
    wait_gone(pid);
    let deadline = Instant::now() + Duration::from_secs(10);
    let settled = loop {
        let queried = harness.call(test, "status", None);
        assert!(queried.get("error").is_none(), "{queried}");
        assert_ne!(queried["ok"]["phase"], "ready", "无ACP程序不得就绪");
        if queried["ok"]["phase"] == "error" && queried["ok"]["agent_pid"].is_null() {
            break queried;
        }
        assert!(
            Instant::now() < deadline,
            "已退出Agent的PID未清除: {queried}"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert!(settled["ok"]["error"].is_string());
    for _ in 0..5 {
        let queried = harness.call(test, "status", None);
        assert_eq!(queried["ok"]["phase"], "error");
        assert!(
            queried["ok"]["agent_pid"].is_null(),
            "查询复活了旧PID {pid}: {queried}"
        );
        assert_eq!(queried["ok"]["service_pid"], settled["ok"]["service_pid"]);
        assert!(!process_alive(pid));
    }
    assert_eq!(harness.call(test, "stop", None)["ok"]["phase"], "stopped");
}

/// 覆盖 AH-08/AH-A07：占用端口与握手失败明确错误、不换端口、不报告Ready。
#[test]
fn occupied_port_and_initialize_failure_are_specific_and_never_ready() {
    if child_operation() {
        return;
    }
    let test = "occupied_port_and_initialize_failure_are_specific_and_never_ready";
    let harness = Harness::new();
    let occupied = std::net::TcpListener::bind(("127.0.0.1", harness.port)).unwrap();
    let result = harness.call(test, "save", Some(&harness.config("normal")));
    assert_ne!(result["ok"]["phase"], "ready");
    let failed = harness.phase(test, "error");
    assert!(failed["error"]
        .as_str()
        .unwrap()
        .contains(&harness.port.to_string()));
    assert!(
        audit(&harness.audit).is_empty(),
        "端口占用不得改端口或启动Agent"
    );
    drop(occupied);
    let _stop = harness.call(test, "stop", None);
    let failed = harness.call(test, "save", Some(&harness.config("handshake-error")));
    assert_ne!(failed["ok"]["phase"], "ready");
    let error = harness.phase(test, "error");
    let diagnostic = error["error"].as_str().unwrap();
    assert!(
        diagnostic.contains("initialize") || diagnostic.contains("初始化"),
        "{error}"
    );
}

/// 覆盖 AH-10/AH-13/AH-A09：主动stop取消在途轮次，不伪成功，最终释放服务和自有Agent。
#[test]
fn explicit_stop_cancels_inflight_requests_then_releases_owned_processes() {
    if child_operation() {
        return;
    }
    let test = "explicit_stop_cancels_inflight_requests_then_releases_owned_processes";
    let harness = Harness::new();
    assert!(harness
        .call(test, "save", Some(&harness.config("normal")))
        .get("error")
        .is_none());
    let status = harness.ready(test);
    let mut plan = Plan::new("text");
    plan.barrier = true;
    harness.register_hold(&plan);
    let mut stream = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&plan, true)),
        None,
    );
    assert_eq!(stream.delta(), plan.chunks[0]);
    let (stop, result) = harness.launch(test, "stop", None);
    wait_event(&harness.audit, "cancelled", &plan.tag);
    let terminal = stream.sse();
    assert!(terminal.contains("error"), "{terminal}");
    assert!(!terminal.contains("\"finish_reason\":\"stop\""));
    let stopped = finish_child(stop, &result);
    assert_eq!(stopped["ok"]["phase"], "stopped");
    wait_gone(status.agent_pid.unwrap());
    wait_gone(status.service_pid.unwrap());
    assert!(std::net::TcpStream::connect(("127.0.0.1", harness.port)).is_err());
}

/// AH-09/AH-10：已排队但晚于显式 Stop 的 Apply 只能作用于已有 daemon，不能充当 Ensure。
#[test]
fn queued_apply_after_explicit_stop_does_not_restart_absent_service() {
    if child_operation() {
        return;
    }
    let test = "queued_apply_after_explicit_stop_does_not_restart_absent_service";
    let harness = Harness::new();
    let config = harness.config("normal");
    let saved_config = serde_json::to_value(&config).unwrap();
    // AH-09 的首次保存自动启动保持不变；不是把所有自动启动一并禁止。
    let saved = harness.call(test, "save", Some(&config));
    assert!(saved.get("error").is_none(), "{saved}");
    let running = harness.ready(test);
    let service_pid = running.service_pid.unwrap();
    let agent_pid = running.agent_pid.unwrap();
    assert!(process_alive(service_pid));
    assert!(process_alive(agent_pid));
    let initial_audit = audit(&harness.audit);
    assert_eq!(
        initial_audit
            .iter()
            .filter(|event| event["event"] == "spawn" && event["pid"] == agent_pid)
            .count(),
        1,
        "必须先观察真实 Agent 启动，而不是只相信 Ready 快照"
    );
    let discovery = harness.call(test, "discover", None);
    assert_eq!(discovery["ok"]["phase"], "ready", "{discovery}");

    // 模拟 GUI observer 已排队、但还没有发 IPC 的 Apply；不占用硬件鼠标。
    let (apply, apply_result) = harness.launch(test, "queued-apply", None);
    let queued = ReleaseMarker {
        path: harness.root.join("release-queued-apply"),
        consumer: Some(apply),
    };
    assert_eq!(
        wait_file(&harness.root.join("queued-apply.json"))["queued"],
        true
    );
    assert!(!apply_result.exists(), "放行前晚到Apply不得发出IPC");
    assert_eq!(harness.call(test, "stop", None)["ok"]["phase"], "stopped");
    wait_gone(agent_pid);
    wait_gone(service_pid);
    assert!(std::net::TcpStream::connect(("127.0.0.1", config.port)).is_err());
    let retired_audit = audit(&harness.audit);
    assert_eq!(retired_audit, initial_audit, "空闲 Stop 不得另开 Agent");
    let absent = harness.call(test, "discover", None);
    assert_eq!(absent, json!({"ok":null}), "只读 Discover 必须确认管道缺席");
    let stopped = harness.call(test, "status", None);
    assert_eq!(stopped["ok"]["phase"], "stopped", "{stopped}");
    assert!(stopped["ok"]["service_pid"].is_null(), "{stopped}");
    assert!(stopped["ok"]["agent_pid"].is_null(), "{stopped}");
    assert_eq!(stopped["ok"]["saved_config"], saved_config);
    assert_eq!(harness.call(test, "load", None)["config"], saved_config);

    // 两个旧 PID 已真实退出、Discover 已确认管道缺席后才执行公开 Apply API。
    let applied = queued.finish(&apply_result);
    let after_discovery = harness.call(test, "discover", None);
    let after_status = harness.call(test, "status", None);
    let after_audit = audit(&harness.audit);
    let still_saved = harness.call(test, "load", None);
    let listening = std::net::TcpStream::connect(("127.0.0.1", config.port)).is_ok();
    assert!(
        matches!(
            applied["error"]["kind"].as_str(),
            Some("not_ready" | "stopping")
        ),
        "缺席后台的晚到Apply必须明确拒绝，不得通过ensure_started回退重新启动: \
         Apply={applied}, Discover={after_discovery}, Status={after_status}, audit={after_audit:?}"
    );
    assert!(
        applied["error"]["message"]
            .as_str()
            .is_some_and(|message| !message.trim().is_empty()),
        "{applied}"
    );
    assert_eq!(after_discovery, json!({"ok":null}), "Apply 不得创建后台");
    assert_eq!(after_status["ok"]["phase"], "stopped", "{after_status}");
    assert!(
        after_status["ok"]["service_pid"].is_null(),
        "{after_status}"
    );
    assert!(after_status["ok"]["agent_pid"].is_null(), "{after_status}");
    assert_eq!(after_audit, retired_audit, "Apply 不得另开 Agent");
    assert!(!listening, "Apply 不得重新建立 HTTP 监听");
    assert_eq!(after_status["ok"]["saved_config"], saved_config);
    assert_eq!(still_saved["config"], saved_config);
    assert!(!process_alive(service_pid));
    assert!(!process_alive(agent_pid));

    // 用户新打开 GUI 的显式 Ensure 仍可从原样保存的配置启动。
    let ensured = harness.call(test, "ensure", None);
    assert!(ensured.get("error").is_none(), "{ensured}");
    let reopened = harness.ready(test);
    assert!(process_alive(reopened.service_pid.unwrap()));
    assert!(process_alive(reopened.agent_pid.unwrap()));
    assert_eq!(reopened.saved_config, Some(config.clone()));
    assert_eq!(reopened.running_config, Some(config));
    assert_eq!(harness.call(test, "discover", None)["ok"]["phase"], "ready");
    assert_eq!(
        audit(&harness.audit)
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        2,
        "只有首次保存和新的显式 Ensure 可以各启动一次 Agent"
    );
    assert_eq!(harness.call(test, "stop", None)["ok"]["phase"], "stopped");
    wait_gone(reopened.agent_pid.unwrap());
    wait_gone(reopened.service_pid.unwrap());
    assert_eq!(harness.call(test, "discover", None), json!({"ok":null}));
    assert!(std::net::TcpStream::connect(("127.0.0.1", harness.port)).is_err());
    assert_eq!(harness.call(test, "load", None)["config"], saved_config);
}

fn wait_process_audit(root: &Path, event: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(found) = audit(root)
            .into_iter()
            .find(|entry| entry["event"] == event)
        {
            return found;
        }
        assert!(
            Instant::now() < deadline,
            "真实后台Agent未发布{event}: {:?}",
            audit(root)
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_service_phase(harness: &Harness, test: &str, phase: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = harness.call(test, "status", None);
        if status["ok"]["phase"] == phase {
            return status["ok"].clone();
        }
        assert!(Instant::now() < deadline, "后台未进入{phase}: {status}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn discovery_without_side_effects(
    harness: &Harness,
    test: &str,
    phase: &str,
    base_url: Option<&str>,
    instance: Option<&Value>,
) -> Value {
    let before = harness.call(test, "status", None)["ok"].clone();
    assert_eq!(before["phase"], phase);
    let saved = harness.call(test, "load", None);
    let before_audit = audit(&harness.audit);
    let first = harness.call(test, "discover", None);
    let descriptor = first["ok"].clone();
    let identity = descriptor["instance_id"].clone();
    assert!(
        uuid::Uuid::parse_str(identity.as_str().unwrap()).is_ok_and(|id| !id.is_nil()),
        "发现必须提供真实后台实例标识: {first}"
    );
    if let Some(expected) = instance {
        assert_eq!(&identity, expected, "同一后台进程不能改变instance_id");
    }
    // 精确比较字段集合和能力：不能泄漏PID、启动命令、配置、凭据或模型快照。
    assert_eq!(
        descriptor,
        json!({
            "protocol_version":1,
            "service_id":"jchtools-acp-http",
            "instance_id":identity,
            "phase":phase,
            "base_url":base_url,
            "execution_mode":"server_agent",
            "capabilities":{
                "text":true,
                "streaming":true,
                "client_tools":false,
                "server_tools":true
            }
        }),
        "Discover只能返回最小版本化描述: {first}"
    );
    for _ in 0..2 {
        assert_eq!(harness.call(test, "discover", None), first);
    }
    assert_eq!(
        harness.call(test, "status", None)["ok"],
        before,
        "Discover不能改变实际阶段、PID、配置或在途请求"
    );
    assert_eq!(harness.call(test, "load", None), saved);
    assert_eq!(
        audit(&harness.audit),
        before_audit,
        "Discover不能启动Agent、触发模型轮次或取消既有请求"
    );
    identity
}

/// AH-06/AH-12/AH-13：真实独立后台的三种HTTP断连只取消对应轮次，保留另一会话和唯一Agent。
#[test]
fn real_service_http_disconnect_cancels_only_corresponding_round() {
    if child_operation() {
        return;
    }
    use std::{
        io::Write,
        net::{Shutdown, TcpStream},
    };
    let test = "real_service_http_disconnect_cancels_only_corresponding_round";
    let harness = Harness::new();
    let saved = harness.call(test, "save", Some(&harness.config("normal")));
    assert!(saved.get("error").is_none(), "{saved}");
    let ready = wait_service_phase(&harness, test, "ready");
    let service_pid = u32::try_from(ready["service_pid"].as_u64().unwrap()).unwrap();
    let agent_pid = u32::try_from(ready["agent_pid"].as_u64().unwrap()).unwrap();
    assert_ne!(service_pid, std::process::id());
    assert_ne!(agent_pid, service_pid);
    let service = ObservedProcess::open(service_pid);
    let agent = ObservedProcess::open(agent_pid);
    // 在同一个独立后台里分别覆盖首个SSE前、JSON完成前、已收到增量SSE。
    for (stream, incremental) in [(true, false), (false, false), (true, true)] {
        let mut victim = Plan::new(if incremental { "text" } else { "silent" });
        victim.barrier = true;
        let mut survivor = Plan::new("text");
        survivor.barrier = true;
        survivor.chunks = vec!["另一会话的首段".into(), "另一会话的完整尾段".into()];
        harness.register_hold(&victim);
        harness.register_hold(&survivor);
        let (socket, response) = if incremental {
            let mut response = Http::open(
                harness.port,
                "POST",
                "/v1/chat/completions",
                Some(&body(&victim, stream)),
                None,
            );
            assert_eq!(response.status, 200);
            assert_eq!(response.delta(), victim.chunks[0]);
            (None, Some(response))
        } else {
            // 直接发送完整HTTP请求；不等待响应头或首事件，也不退出消费者进程。
            let mut socket = TcpStream::connect(("127.0.0.1", harness.port)).unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let request = body(&victim, stream).to_string();
            write!(
                socket,
                "POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{request}",
                request.len()
            )
            .unwrap();
            socket.flush().unwrap();
            (Some(socket), None)
        };
        let started = wait_event(&harness.audit, "started", &victim.tag);
        assert_eq!(started["pid"], agent_pid);
        let mut other = Http::open(
            harness.port,
            "POST",
            "/v1/chat/completions",
            Some(&body(&survivor, true)),
            None,
        );
        assert_eq!(other.status, 200);
        let prefix = other.delta();
        assert_eq!(prefix, survivor.chunks[0]);
        let other_started = wait_event(&harness.audit, "started", &survivor.tag);
        assert_ne!(started["session"], other_started["session"]);
        assert_eq!(other_started["pid"], agent_pid);
        assert!(!audit(&harness.audit)
            .iter()
            .any(|event| { event["event"] == "completed" && event["tag"] == victim.tag }));

        // shutdown(Both) / response owning socket的析构是真实断连；当前测试进程继续存活。
        if let Some(socket) = socket {
            socket.shutdown(Shutdown::Both).unwrap();
            drop(socket);
        }
        drop(response);
        let deadline = Instant::now() + Duration::from_secs(10);
        let cancel_events = loop {
            let events = audit(&harness.audit);
            let received = events.iter().any(|event| {
                event["event"] == "cancel-received"
                    && event["session"] == started["session"]
                    && event["pid"] == agent_pid
            });
            let consumed = events.iter().any(|event| {
                event["event"] == "cancelled"
                    && event["tag"] == victim.tag
                    && event["pid"] == agent_pid
            });
            if received && consumed {
                break events;
            }
            assert!(
                Instant::now() < deadline,
                "HTTP断连未取消对应真实Agent轮次 stream={stream} incremental={incremental}: {events:?}"
            );
            thread::sleep(Duration::from_millis(10));
        };
        // 所有取消证据在任何release/Stop之前取得，不能把清理取消当作HTTP断连。
        for plan in [&victim, &survivor] {
            assert!(!harness.audit.join(format!("release-{}", plan.tag)).exists());
        }
        assert!(
            !cancel_events.iter().any(|event| {
                (event["event"] == "cancel-received"
                    && event["session"] == other_started["session"])
                    || (event["event"] == "cancelled" && event["tag"] == survivor.tag)
                    || (event["event"] == "completed" && event["tag"] == survivor.tag)
            }),
            "另一会话必须仍持有barrier且未被取消: {cancel_events:?}"
        );
        assert!(service.is_alive());
        assert!(agent.is_alive());
        release(&harness.audit, &survivor);
        assert_eq!(prefix + &other.finish_stream(), survivor.text());
        wait_event(&harness.audit, "completed", &survivor.tag);
        drop(other);
        let next = Plan::new("text");
        let response = Http::open(
            harness.port,
            "POST",
            "/v1/chat/completions",
            Some(&body(&next, false)),
            None,
        );
        assert_eq!(response.status, 200);
        assert_eq!(completion(&response.json()), next.text());
        assert_eq!(
            wait_event(&harness.audit, "started", &next.tag)["pid"],
            agent_pid
        );
        let unchanged = harness.call(test, "status", None);
        assert_eq!(unchanged["ok"]["phase"], "ready");
        assert_eq!(unchanged["ok"]["service_pid"], service_pid);
        assert_eq!(unchanged["ok"]["agent_pid"], agent_pid);
        assert_eq!(
            audit(&harness.audit)
                .iter()
                .filter(|event| event["event"] == "spawn")
                .count(),
            1,
            "断连后第三个请求仍须复用原Agent"
        );
    }
    assert_eq!(harness.call(test, "stop", None)["ok"]["phase"], "stopped");
    agent.wait_gone();
    service.wait_gone();
}

/// AH-15/AH-A14：真实初始化hold窗口内Discover只报告Starting，不公布可用地址或推进初始化。
#[test]
fn real_service_discover_during_starting_is_minimal_stable_and_read_only() {
    if child_operation() {
        return;
    }
    let test = "real_service_discover_during_starting_is_minimal_stable_and_read_only";
    let harness = Harness::new();
    let marker = harness.audit.join("release-initialize");
    harness.release_markers.borrow_mut().push(marker.clone());
    let config = harness.config("initialize-held");
    let saved = harness.call(test, "save", Some(&config));
    assert!(saved.get("error").is_none(), "{saved}");
    let held = wait_process_audit(&harness.audit, "initialize-held");
    let starting = wait_service_phase(&harness, test, "starting");
    assert_eq!(held["pid"], starting["agent_pid"]);
    let service_pid = u32::try_from(starting["service_pid"].as_u64().unwrap()).unwrap();
    let agent_pid = u32::try_from(starting["agent_pid"].as_u64().unwrap()).unwrap();
    assert_ne!(service_pid, std::process::id());
    assert!(process_alive(service_pid));
    assert!(process_alive(agent_pid));
    let instance = discovery_without_side_effects(&harness, test, "starting", None, None);
    assert!(!marker.exists(), "Discover不能释放初始化hold");
    assert_eq!(
        audit(&harness.audit)
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        1
    );
    std::fs::write(&marker, b"initialize").unwrap();
    let ready = wait_service_phase(&harness, test, "ready");
    assert_eq!(ready["service_pid"], service_pid);
    assert_eq!(ready["agent_pid"], agent_pid);
    let address = format!("http://127.0.0.1:{}", harness.port);
    discovery_without_side_effects(&harness, test, "ready", Some(&address), Some(&instance));
    assert_eq!(harness.call(test, "stop", None)["ok"]["phase"], "stopped");
    wait_gone(agent_pid);
    wait_gone(service_pid);
    assert_eq!(harness.call(test, "discover", None), json!({"ok":null}));
}

/// AH-09/AH-15/AH-A14：真实Apply drain持有轮次时Discover不公布地址、不取消轮次、不应用保存配置。
#[test]
fn real_service_discover_during_draining_is_minimal_stable_and_read_only() {
    if child_operation() {
        return;
    }
    let test = "real_service_discover_during_draining_is_minimal_stable_and_read_only";
    let harness = Harness::new();
    let original = harness.config("barrier-held");
    let saved = harness.call(test, "save", Some(&original));
    assert!(saved.get("error").is_none(), "{saved}");
    let ready = wait_service_phase(&harness, test, "ready");
    let address = format!("http://127.0.0.1:{}", harness.port);
    let instance = discovery_without_side_effects(&harness, test, "ready", Some(&address), None);
    let mut plan = Plan::new("text");
    plan.barrier = true;
    harness.register_hold(&plan);
    let mut stream = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&plan, true)),
        None,
    );
    let prefix = stream.delta();
    assert_eq!(prefix, plan.chunks[0]);
    wait_event(&harness.audit, "started", &plan.tag);
    let mut changed = original.clone();
    changed.port = unused_port();
    assert!(harness
        .call(test, "save", Some(&changed))
        .get("error")
        .is_none());
    let (apply, result) = harness.launch(test, "apply", None);
    let draining = wait_service_phase(&harness, test, "draining");
    assert_eq!(draining["service_pid"], ready["service_pid"]);
    assert_eq!(draining["agent_pid"], ready["agent_pid"]);
    assert_eq!(
        draining["saved_config"],
        serde_json::to_value(&changed).unwrap()
    );
    assert_eq!(
        draining["running_config"],
        serde_json::to_value(&original).unwrap()
    );
    discovery_without_side_effects(&harness, test, "draining", None, Some(&instance));
    assert!(!result.exists(), "Discover不能提前完成Apply");
    assert!(!harness.audit.join(format!("release-{}", plan.tag)).exists());
    assert_eq!(
        audit(&harness.audit)
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        1
    );
    assert_admission_closed(harness.port);
    release(&harness.audit, &plan);
    assert_eq!(prefix + &stream.finish_stream(), plan.text());
    let applied = finish_child(apply, &result);
    assert_eq!(applied["ok"]["phase"], "ready", "{applied}");
    assert_eq!(applied["ok"]["service_pid"], ready["service_pid"]);
    assert_eq!(
        applied["ok"]["running_config"],
        serde_json::to_value(&changed).unwrap()
    );
    let address = format!("http://127.0.0.1:{}", changed.port);
    discovery_without_side_effects(&harness, test, "ready", Some(&address), Some(&instance));
    let next = Plan::new("text");
    let response = Http::open(
        changed.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&next, false)),
        None,
    );
    assert_eq!(response.status, 200);
    assert_eq!(completion(&response.json()), next.text());
    assert_eq!(harness.call(test, "stop", None)["ok"]["phase"], "stopped");
    wait_gone(u32::try_from(applied["ok"]["agent_pid"].as_u64().unwrap()).unwrap());
    wait_gone(u32::try_from(ready["service_pid"].as_u64().unwrap()).unwrap());
    assert_eq!(harness.call(test, "discover", None), json!({"ok":null}));
}

/// AH-10/AH-15/AH-A14：真实Stop等待存活自有terminal时Discover只报告Stopping，不改变安全等待。
#[test]
fn real_service_discover_during_stopping_is_minimal_stable_and_read_only() {
    if child_operation() {
        return;
    }
    let test = "real_service_discover_during_stopping_is_minimal_stable_and_read_only";
    let harness = Harness::new();
    let saved = harness.call(test, "save", Some(&harness.config("normal")));
    assert!(saved.get("error").is_none(), "{saved}");
    let ready = wait_service_phase(&harness, test, "ready");
    let address = format!("http://127.0.0.1:{}", harness.port);
    let instance = discovery_without_side_effects(&harness, test, "ready", Some(&address), None);
    let plan = Plan::new("terminal-held");
    harness.register_hold(&plan);
    let response = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&plan, false)),
        None,
    );
    assert_eq!(response.status, 200);
    let value = response.json();
    assert!(serde_json::from_str::<Value>(completion(&value))
        .unwrap()
        .is_string());
    wait_event(&harness.audit, "completed", &plan.tag);
    let workspace = harness.root.join("acp-workspace");
    let pid_file = workspace.join(format!("terminal-{}.pid", plan.tag));
    let deadline = Instant::now() + Duration::from_secs(10);
    let terminal_pid = loop {
        if let Ok(text) = std::fs::read_to_string(&pid_file) {
            if let Ok(pid) = text.parse::<u32>() {
                break pid;
            }
        }
        assert!(Instant::now() < deadline, "自有terminal未发布真实PID");
        thread::sleep(Duration::from_millis(10));
    };
    let terminal = ObservedProcess::open(terminal_pid);
    assert!(terminal.is_alive());
    let (stop, result) = harness.launch(test, "stop", None);
    let stopping = wait_service_phase(&harness, test, "stopping");
    assert_eq!(stopping["service_pid"], ready["service_pid"]);
    assert_eq!(stopping["agent_pid"], ready["agent_pid"]);
    discovery_without_side_effects(&harness, test, "stopping", None, Some(&instance));
    let marker = workspace.join(format!("terminal-release-{}", plan.tag));
    assert!(!marker.exists(), "Discover不能放行terminal");
    assert!(!result.exists(), "存活terminal未自然结束前Stop必须等待");
    assert!(terminal.is_alive());
    assert!(process_alive(
        u32::try_from(ready["service_pid"].as_u64().unwrap()).unwrap()
    ));
    assert!(process_alive(
        u32::try_from(ready["agent_pid"].as_u64().unwrap()).unwrap()
    ));
    std::fs::write(marker, b"terminal-completed").unwrap();
    let stopped = finish_child(stop, &result);
    assert_eq!(stopped["ok"]["phase"], "stopped", "{stopped}");
    assert!(stopped["ok"]["service_pid"].is_null());
    assert!(stopped["ok"]["agent_pid"].is_null());
    terminal.wait_gone();
    wait_gone(u32::try_from(ready["agent_pid"].as_u64().unwrap()).unwrap());
    wait_gone(u32::try_from(ready["service_pid"].as_u64().unwrap()).unwrap());
    assert_eq!(harness.call(test, "discover", None), json!({"ok":null}));
    let absent = harness.call(test, "status", None);
    assert_eq!(absent["ok"]["phase"], "stopped", "{absent}");
    assert!(absent["ok"]["service_pid"].is_null());
    assert!(absent["ok"]["agent_pid"].is_null());
}

/// AH-09/AH-10/AH-13：Stop抢占Apply时Agent取消后异常退出；收尾错误不得永久占住后台和单实例锁。
#[test]
fn stop_cancellation_agent_exit_releases_daemon_and_allows_restart() {
    if child_operation() {
        return;
    }
    let test = "stop_cancellation_agent_exit_releases_daemon_and_allows_restart";
    let harness = Harness::new();
    let config = harness.config("cancel-exit");
    let saved = harness.call(test, "save", Some(&config));
    assert!(saved.get("error").is_none(), "{saved}");
    let running = harness.ready(test);
    let service_pid = running.service_pid.unwrap();
    let agent_pid = running.agent_pid.unwrap();
    let mut plan = Plan::new("text");
    plan.barrier = true;
    harness.register_hold(&plan);
    let mut stream = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&plan, true)),
        None,
    );
    assert_eq!(stream.delta(), plan.chunks[0]);
    let started = wait_event(&harness.audit, "started", &plan.tag);
    assert_eq!(started["pid"], agent_pid);
    let (apply, apply_result) = harness.launch(test, "apply", None);
    let draining = harness.phase(test, "draining");
    assert_eq!(draining["service_pid"], service_pid);
    assert_eq!(draining["agent_pid"], agent_pid);
    assert!(!apply_result.exists(), "自然Apply必须等待未释放的真实轮次");
    let (stop, stop_result) = harness.launch(test, "stop", None);
    let terminal = stream.sse();
    let stopped = finish_child(stop, &stop_result);
    let applied = finish_child(apply, &apply_result);
    assert!(terminal.contains("error"), "{terminal}");
    assert!(
        !terminal.contains("\"finish_reason\":\"stop\""),
        "{terminal}"
    );
    assert!(!harness.audit.join(format!("release-{}", plan.tag)).exists());
    assert!(
        audit(&harness.audit).iter().any(|event| {
            event["event"] == "cancel-exit"
                && event["session"] == started["session"]
                && event["pid"] == agent_pid
                && event["code"] == 75
        }),
        "必须由真实Stop的session/cancel触发自有Agent异常退出"
    );
    assert!(
        applied.get("error").is_some() || applied["ok"]["phase"] == "stopped",
        "被Stop抢占的Apply不得创建替换Agent: {applied}"
    );
    if let Some(error) = stopped.get("error") {
        assert!(error["kind"].is_string(), "{stopped}");
        assert!(
            error["message"]
                .as_str()
                .is_some_and(|message| !message.trim().is_empty()),
            "Stop收尾错误必须明确报告: {stopped}"
        );
    } else {
        assert_eq!(stopped["ok"]["phase"], "stopped", "{stopped}");
    }
    // 错误本身不是验收结果：旧Agent/HTTP实际收尾后，旧daemon必须自然退出。
    wait_gone(agent_pid);
    assert!(std::net::TcpStream::connect(("127.0.0.1", harness.port)).is_err());
    wait_gone(service_pid);
    let inactive = harness.call(test, "status", None);
    assert_eq!(inactive["ok"]["phase"], "stopped", "{inactive}");
    assert!(inactive["ok"]["service_pid"].is_null(), "{inactive}");
    assert!(inactive["ok"]["agent_pid"].is_null(), "{inactive}");
    assert_eq!(
        inactive["ok"]["saved_config"],
        serde_json::to_value(&config).unwrap()
    );
    let ensured = harness.call(test, "ensure", None);
    assert!(ensured.get("error").is_none(), "{ensured}");
    let reopened = harness.ready(test);
    assert_ne!(reopened.service_pid, Some(service_pid));
    assert_ne!(reopened.agent_pid, Some(agent_pid));
    assert!(process_alive(reopened.service_pid.unwrap()));
    assert!(process_alive(reopened.agent_pid.unwrap()));
    let models = Http::open(harness.port, "GET", "/v1/models", None, None);
    assert_eq!(models.status, 200);
    let models = models.json();
    assert_eq!(models["object"], "list");
    assert_eq!(
        models["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![fixture::MODEL_A, fixture::MODEL_B]
    );
    let events = audit(&harness.audit);
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event"] == "started" && event["tag"] == plan.tag)
            .count(),
        1,
        "取消失败轮次不得在重开后台后重放"
    );
    assert_eq!(harness.call(test, "stop", None)["ok"]["phase"], "stopped");
    wait_gone(reopened.agent_pid.unwrap());
    wait_gone(reopened.service_pid.unwrap());
    assert!(std::net::TcpStream::connect(("127.0.0.1", harness.port)).is_err());
}

thread_local! {
    // 和既有gui_flow一致：驱动定时器必须活过pre-loop hook，绝不占硬件鼠标。
    static GUI_DRIVER: std::cell::RefCell<Option<slint::Timer>> = const {std::cell::RefCell::new(None)};
}
fn gui_child_operation() -> bool {
    if std::env::var("JCHTOOLS_ACP_PROCESS_ACTION").as_deref() != Ok("gui") {
        return false;
    }
    use jchtools::gui::{self, EngineTestOverrides};
    use slint::ComponentHandle;
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };
    let root = PathBuf::from(std::env::var_os(CHILD_ROOT).unwrap());
    let config: ServiceConfig =
        serde_json::from_str(&std::env::var("JCHTOOLS_ACP_PROCESS_CONFIG").unwrap()).unwrap();
    let mode = std::env::var("JCHTOOLS_ACP_GUI_MODE").unwrap_or_else(|_| "reopen".into());
    std::env::set_var("SLINT_BACKEND", "winit-software");
    let snapshot_assets = root.join("synthetic-snapshot-assets");
    std::fs::create_dir_all(&snapshot_assets).unwrap();
    std::env::set_var("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT", snapshot_assets);
    let failure = Rc::new(RefCell::new(None::<String>));
    let observed = failure.clone();
    let state_dir = root.clone();
    let result_path = std::env::var_os("JCHTOOLS_ACP_PROCESS_RESULT").unwrap();
    gui::run_with_engine_overrides(move |ui| {
        ui.invoke_select_tool("acp-http".into());
        assert_eq!(ui.get_screen(),8);
        ui.window().set_size(slint::LogicalSize::new(1120.0,720.0));
        if mode=="save" {
            ui.set_acp_executable(config.executable.clone().into());
            ui.set_acp_arguments(settings::format_arguments(&config.arguments).into());
            ui.set_acp_port(config.port.to_string().into());
            ui.invoke_acp_config_edited();ui.invoke_acp_save_config();
        }
        let ui=ui.as_weak();let stage=Rc::new(Cell::new(0_u8));let stopped_at=Rc::new(Cell::new(None::<Instant>));
        let started=Instant::now();let timer=slint::Timer::default();
        timer.start(slint::TimerMode::Repeated,Duration::from_millis(50),move || {
            let Some(ui)=ui.upgrade() else {return;};
            let fail=|message:String| {*failure.borrow_mut()=Some(message);let _quit=slint::quit_event_loop();};
            if started.elapsed()>Duration::from_secs(45) {fail(format!("GUI流程超时 phase={} action={} error={}",ui.get_acp_phase(),ui.get_acp_operation(),ui.get_acp_action_error()));return;}
            if stage.get()==0 && ui.get_acp_ready() && !ui.get_acp_request_pending() {
                if ui.get_acp_executable().as_str()!=config.executable || settings::parse_arguments(ui.get_acp_arguments().as_str()).unwrap()!=config.arguments || ui.get_acp_port().as_str()!=config.port.to_string() {
                    fail("GUI保存/恢复的程序、argv或port不一致".into());return;
                }
                let snapshot=json!({"service_pid":ui.get_acp_service_pid().to_string(),"agent_pid":ui.get_acp_agent_pid().to_string(),"phase":ui.get_acp_phase().to_string(),
                    "saved":ui.get_acp_saved_config().to_string(),"running":ui.get_acp_running_config().to_string(),"ready":ui.get_acp_ready()});
                std::fs::write(root.join("gui-ready.json"),snapshot.to_string()).unwrap();stage.set(1);
            }
            if stage.get()==1 && root.join("gui-stop").exists() {
                ui.invoke_acp_request_stop();
                if ui.get_confirm_kind()!=5 {fail("独立模型服务退出未显示确认层5".into());return;}
                std::fs::write(root.join("gui-confirmed.txt"),ui.get_confirm_text().as_str()).unwrap();
                ui.invoke_confirmed(5);ui.set_confirm_kind(0);stage.set(2);
            }
            if stage.get()==2 && ui.get_acp_phase().as_str()=="stopped" && !ui.get_acp_request_pending() {
                if !ui.get_acp_explicit_stopped() {fail("当前GUI未记住显式退出".into());return;}
                stopped_at.set(Some(Instant::now()));stage.set(3);
            }
            if stage.get()==3 && stopped_at.get().is_some_and(|at|at.elapsed()>Duration::from_secs(1)) {
                if ui.get_acp_phase().as_str()!="stopped" || ui.get_acp_ready() || !ui.get_acp_service_pid().is_empty() || !ui.get_acp_agent_pid().is_empty() {
                    fail("显式退出后当前GUI自动拉起服务或残留PID".into());return;
                }
                std::fs::write(root.join("gui-stopped.json"),json!({"phase":ui.get_acp_phase().to_string(),"explicit":ui.get_acp_explicit_stopped()}).to_string()).unwrap();stage.set(4);
            }
            if stage.get()==1 && root.join("gui-save-next.json").exists() {
                let changed:ServiceConfig=serde_json::from_slice(&std::fs::read(root.join("gui-save-next.json")).unwrap()).unwrap();
                ui.set_acp_executable(changed.executable.into());ui.set_acp_arguments(settings::format_arguments(&changed.arguments).into());ui.set_acp_port(changed.port.to_string().into());
                ui.invoke_acp_config_edited();ui.invoke_acp_save_config();stage.set(5);
            }
            if stage.get()==5 && !ui.get_acp_request_pending() && ui.get_acp_pending_apply() {
                std::fs::write(root.join("gui-pending.json"),json!({"agent_pid":ui.get_acp_agent_pid().to_string(),"saved":ui.get_acp_saved_config().to_string(),"running":ui.get_acp_running_config().to_string()}).to_string()).unwrap();stage.set(6);
            }
            if stage.get()==6 && root.join("gui-apply").exists() {ui.invoke_acp_apply_and_restart();stage.set(7);}
            if stage.get()==7 && ui.get_acp_phase().as_str()=="draining" && !root.join("gui-draining.json").exists() {
                std::fs::write(root.join("gui-draining.json"),json!({"responsive":true,"agent_pid":ui.get_acp_agent_pid().to_string()}).to_string()).unwrap();
            }
            if stage.get()==7 && ui.get_acp_ready() && !ui.get_acp_request_pending() && !ui.get_acp_pending_apply() {
                std::fs::write(root.join("gui-applied.json"),json!({"agent_pid":ui.get_acp_agent_pid().to_string(),"saved":ui.get_acp_saved_config().to_string(),"running":ui.get_acp_running_config().to_string()}).to_string()).unwrap();stage.set(8);
            }
            if stage.get()>=1 && root.join("gui-close").exists() {
                // 和gui_flow标题栏路径一致，分发窗口内的合成事件，不移动系统鼠标。
                let position=slint::LogicalPosition::new(1097.0,23.0);
                for event in [slint::platform::WindowEvent::PointerMoved{position},
                    slint::platform::WindowEvent::PointerPressed{position,button:slint::platform::PointerEventButton::Left},
                    slint::platform::WindowEvent::PointerReleased{position,button:slint::platform::PointerEventButton::Left}] {
                    ui.window().dispatch_event(event);
                }
            }
        });
        GUI_DRIVER.with(|driver|*driver.borrow_mut()=Some(timer));
    },Some(EngineTestOverrides{state_dir})).unwrap();
    GUI_DRIVER.with(|driver| *driver.borrow_mut() = None);
    assert!(
        observed.borrow().is_none(),
        "真实GUI失败: {:?}",
        observed.borrow()
    );
    std::fs::write(result_path, json!({"gui":"closed"}).to_string()).unwrap();
    true
}
fn wait_file(path: &Path) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(value) = serde_json::from_slice(&bytes) {
                return value;
            }
        }
        assert!(
            Instant::now() < deadline,
            "GUI未发布可观察结果 {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// 覆盖 AH-01/AH-06～AH-10/U-09：真实winit窗口回调保存，关窗/终止GUI后HTTP继续；重开复用；独立确认退出。
/// 合成SDK窗口E2E补充覆盖，AH-A02/AH-A06指定OpenCode和真实模型仍NOT RUN。
#[test]
fn real_gui_save_close_kill_reopen_and_confirmed_service_exit() {
    if gui_child_operation() || child_operation() {
        return;
    }
    let test = "real_gui_save_close_kill_reopen_and_confirmed_service_exit";
    let harness = Harness::new();
    let config = harness.config("normal");
    let launch_gui = |mode: &str| {
        for name in [
            "gui-ready.json",
            "gui-stopped.json",
            "gui-stop",
            "gui-close",
            "gui-confirmed.txt",
        ] {
            let path = harness.root.join(name);
            if path.exists() {
                std::fs::remove_file(path).unwrap();
            }
        }
        let result = harness
            .temp
            .path()
            .join(format!("gui-result-{}.json", uuid::Uuid::new_v4()));
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env(CHILD_ROOT, &harness.root)
            .env("JCHTOOLS_ACP_PROCESS_ACTION", "gui")
            .env("JCHTOOLS_ACP_GUI_MODE", mode)
            .env(
                "JCHTOOLS_ACP_PROCESS_CONFIG",
                serde_json::to_string(&config).unwrap(),
            )
            .env("JCHTOOLS_ACP_PROCESS_RESULT", &result)
            .env("JCHTOOLS_TEST_STATE_DIR", &harness.root)
            .env(
                "JCHTOOLS_TEST_ACP_SERVICE_EXE",
                env!("CARGO_BIN_EXE_JchTools"),
            )
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        (child, result)
    };
    let (gui, result) = launch_gui("save");
    let ready = wait_file(&harness.root.join("gui-ready.json"));
    assert_eq!(ready["phase"], "ready");
    let agent = ready["agent_pid"].as_str().unwrap().parse::<u32>().unwrap();
    let service = ready["service_pid"]
        .as_str()
        .unwrap()
        .parse::<u32>()
        .unwrap();
    assert_eq!(
        harness.call(test, "load", None)["config"],
        serde_json::to_value(&config).unwrap()
    );
    let permission = Plan::new("permission");
    let allowed = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&permission, false)),
        None,
    )
    .json();
    let allowed: Value = serde_json::from_str(completion(&allowed)).unwrap();
    assert_eq!(allowed["outcome"]["optionId"], "always");
    let mut first = Plan::new("text");
    first.barrier = true;
    harness.register_hold(&first);
    let mut response = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&first, true)),
        None,
    );
    assert_eq!(response.delta(), first.chunks[0]);
    std::fs::write(harness.root.join("gui-close"), b"close").unwrap();
    assert_eq!(finish_child(gui, &result)["gui"], "closed");
    release(&harness.audit, &first);
    assert_eq!(response.finish_stream(), first.chunks[1]);
    let after_close = Plan::new("text");
    assert_eq!(
        completion(
            &Http::open(
                harness.port,
                "POST",
                "/v1/chat/completions",
                Some(&body(&after_close, false)),
                None
            )
            .json()
        ),
        after_close.text()
    );
    let permission = Plan::new("permission");
    let allowed = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&permission, false)),
        None,
    )
    .json();
    let allowed: Value = serde_json::from_str(completion(&allowed)).unwrap();
    assert_eq!(
        allowed["outcome"]["optionId"], "always",
        "无GUI时仍必须最大授权"
    );
    let file = Plan::new("files");
    let value = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&file, false)),
        None,
    )
    .json();
    assert_eq!(completion(&value).trim_end_matches('\n'), file.tag);
    assert_eq!(
        std::fs::read_to_string(
            harness
                .root
                .join("acp-workspace")
                .join(format!("{}.txt", file.tag))
        )
        .unwrap(),
        format!("first\n{}\nlast\n", file.tag)
    );
    let (mut gui, _result) = launch_gui("reopen");
    let reopened = wait_file(&harness.root.join("gui-ready.json"));
    assert_eq!(reopened["agent_pid"], ready["agent_pid"]);
    assert_eq!(reopened["service_pid"], ready["service_pid"]);
    let mut active = Plan::new("text");
    active.barrier = true;
    harness.register_hold(&active);
    let mut response = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&active, true)),
        None,
    );
    assert_eq!(response.delta(), active.chunks[0]);
    gui.kill().unwrap();
    let killed = gui.wait().unwrap();
    assert!(!killed.success(), "本次确实终止GUI进程");
    release(&harness.audit, &active);
    assert_eq!(response.finish_stream(), active.chunks[1]);
    let new = Plan::new("text");
    assert_eq!(
        completion(
            &Http::open(
                harness.port,
                "POST",
                "/v1/chat/completions",
                Some(&body(&new, false)),
                None
            )
            .json()
        ),
        new.text()
    );
    assert_eq!(
        wait_event(&harness.audit, "started", &new.tag)["pid"],
        agent
    );
    let (gui, result) = launch_gui("reopen");
    let again = wait_file(&harness.root.join("gui-ready.json"));
    assert_eq!(again["service_pid"], ready["service_pid"]);
    std::fs::write(harness.root.join("gui-stop"), b"confirmed-stop").unwrap();
    let stopped = wait_file(&harness.root.join("gui-stopped.json"));
    assert_eq!(stopped["explicit"], true);
    let confirmation = std::fs::read_to_string(harness.root.join("gui-confirmed.txt")).unwrap();
    assert!(
        confirmation.contains("截图")
            && confirmation.contains("Xberg")
            && confirmation.contains("不自动强杀")
    );
    wait_gone(agent);
    wait_gone(service);
    assert_eq!(
        audit(&harness.audit)
            .iter()
            .filter(|entry| entry["event"] == "spawn")
            .count(),
        1,
        "当前GUI不得自动拉起"
    );
    std::fs::write(harness.root.join("gui-close"), b"close").unwrap();
    assert_eq!(finish_child(gui, &result)["gui"], "closed");
    let (gui, result) = launch_gui("reopen");
    let restarted = wait_file(&harness.root.join("gui-ready.json"));
    assert_ne!(restarted["agent_pid"], ready["agent_pid"]);
    let next = Plan::new("text");
    assert_eq!(
        completion(
            &Http::open(
                harness.port,
                "POST",
                "/v1/chat/completions",
                Some(&body(&next, false)),
                None
            )
            .json()
        ),
        next.text()
    );
    std::fs::write(harness.root.join("gui-close"), b"close").unwrap();
    assert_eq!(finish_child(gui, &result)["gui"], "closed");
}

/// 覆盖 AH-06/AH-13/AH-A09：真实后台持有的Agent异常退出，所有旧轮次失败，下一个请求重建唯一Agent。
#[test]
fn real_service_recovers_agent_exit_without_replaying_failed_requests() {
    if child_operation() {
        return;
    }
    let test = "real_service_recovers_agent_exit_without_replaying_failed_requests";
    let harness = Harness::new();
    assert!(harness
        .call(test, "save", Some(&harness.config("normal")))
        .get("error")
        .is_none());
    let status = harness.ready(test);
    // 在退出发生前持有原 Agent 身份，不能用可能已复用的 PID 证明它退出。
    let agent = ObservedProcess::open(status.agent_pid.unwrap());
    let mut victim = Plan::new("text");
    victim.barrier = true;
    harness.register_hold(&victim);
    let mut stream = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&victim, true)),
        None,
    );
    assert_eq!(stream.delta(), victim.chunks[0]);
    let crash = Plan::new("crash");
    let response = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&crash, false)),
        None,
    );
    assert_eq!(response.status, 502);
    assert!(response.json().get("error").is_some());
    let event = stream.sse();
    assert!(event.contains("error"), "{event}");
    assert!(!event.contains("\"finish_reason\":\"stop\""));
    agent.wait_gone();
    assert!(process_alive(status.service_pid.unwrap()));
    let next = Plan::new("text");
    let response = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&next, false)),
        None,
    );
    let response_status = response.status;
    let response = response.json();
    assert_eq!(response_status, 200, "退出后首次新请求必须恢复：{response}");
    assert_eq!(completion(&response), next.text());
    let recovered = harness.ready(test);
    assert_eq!(recovered.service_pid, status.service_pid);
    assert_ne!(recovered.agent_pid, status.agent_pid);
    let events = audit(&harness.audit);
    assert_eq!(
        events
            .iter()
            .filter(|entry| entry["event"] == "spawn")
            .count(),
        2
    );
    for plan in [&victim, &crash] {
        assert_eq!(
            events
                .iter()
                .filter(|entry| entry["event"] == "started" && entry["tag"] == plan.tag)
                .count(),
            1
        );
    }
}

/// 覆盖 AH-10/AH-13/AH-A11：服务退出释放仍归本应用拥有的实际terminal子进程，不留孤儿。
#[test]
fn stopping_service_releases_owned_terminal_process() {
    if child_operation() {
        return;
    }
    let test = "stopping_service_releases_owned_terminal_process";
    let harness = Harness::new();
    assert!(harness
        .call(test, "save", Some(&harness.config("normal")))
        .get("error")
        .is_none());
    let status = harness.ready(test);
    let plan = Plan::new("terminal-held");
    harness.register_hold(&plan);
    let response = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&plan, false)),
        None,
    );
    assert_eq!(response.status, 200);
    let value = response.json();
    let terminal_id: Value = serde_json::from_str(completion(&value)).unwrap();
    assert!(terminal_id.is_string());
    let pid_file = harness
        .root
        .join("acp-workspace")
        .join(format!("terminal-{}.pid", plan.tag));
    let deadline = Instant::now() + Duration::from_secs(15);
    let pid = loop {
        if let Ok(text) = std::fs::read_to_string(&pid_file) {
            if let Ok(pid) = text.parse::<u32>() {
                break pid;
            }
        }
        assert!(
            Instant::now() < deadline,
            "terminal创建回调没有执行实际命令"
        );
        thread::sleep(Duration::from_millis(10));
    };
    let (stop, result) = harness.launch(test, "stop", None);
    let stopping = harness.phase(test, "stopping");
    assert_eq!(stopping["service_pid"], status.service_pid.unwrap());
    assert_eq!(stopping["agent_pid"], status.agent_pid.unwrap());
    assert!(process_alive(status.service_pid.unwrap()));
    assert!(process_alive(status.agent_pid.unwrap()));
    assert!(!result.exists(), "自有terminal仍运行时Stop不得完成");
    assert!(process_alive(pid), "必须观察Stop正在等待存活terminal");
    // 合成终端自己完成工作；安全退出必须随后回收它，而不是只清掉terminal表。
    std::fs::write(
        harness
            .root
            .join("acp-workspace")
            .join(format!("terminal-release-{}", plan.tag)),
        b"done",
    )
    .unwrap();
    assert_eq!(finish_child(stop, &result)["ok"]["phase"], "stopped");
    wait_gone(pid);
    wait_gone(status.agent_pid.unwrap());
    wait_gone(status.service_pid.unwrap());
}

fn assert_admission_closed(port: u16) {
    use std::io::{BufRead, BufReader, Write};
    let Ok(mut socket) = std::net::TcpStream::connect(("127.0.0.1", port)) else {
        return;
    };
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let request = body(&Plan::new("text"), false).to_string();
    if write!(socket,"POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{request}",request.len()).is_err(){return;}
    let mut line = String::new();
    match BufReader::new(socket).read_line(&mut line) {
        Ok(0) => {}
        Ok(_) => assert_eq!(
            line.split_whitespace().nth(1),
            Some("503"),
            "draining不得接受新请求: {line}"
        ),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            ) => {}
        Err(error) => panic!("drain后新请求挂起或错误不可分类: {error}"),
    }
}

/// 覆盖 AH-08/AH-13：初始化失败而子进程不自行退出时，必须回收自有失败Agent。
#[test]
fn initialize_failure_reclaims_agent_that_does_not_exit_on_eof() {
    if child_operation() {
        return;
    }
    let test = "initialize_failure_reclaims_agent_that_does_not_exit_on_eof";
    let harness = Harness::new();
    let saved = harness.call(test, "save", Some(&harness.config("handshake-error-held")));
    assert_ne!(saved["ok"]["phase"], "ready");
    harness.phase(test, "error");
    let events = audit(&harness.audit);
    let spawned = events
        .iter()
        .find(|event| event["event"] == "spawn")
        .unwrap();
    let pid = u32::try_from(spawned["pid"].as_u64().unwrap()).unwrap();
    wait_gone(pid);
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        1
    );
    let stopped = harness.call(test, "stop", None);
    assert_eq!(stopped["ok"]["phase"], "stopped");
}

/// 覆盖 AH-12/AH-13：Ready后连接EOF但原Agent还存活，有限时间明确失败且不得增开第二Agent。
#[test]
fn live_retiring_agent_blocks_reconnect_with_specific_error_not_a_hang() {
    if child_operation() {
        return;
    }
    let test = "live_retiring_agent_blocks_reconnect_with_specific_error_not_a_hang";
    let harness = Harness::new();
    assert!(harness
        .call(test, "save", Some(&harness.config("disconnect-held")))
        .get("error")
        .is_none());
    let ready = harness.ready(test);
    // 在 Agent 仍存活时固定进程身份；退出观察不能重新打开可能复用的 PID。
    let agent = ObservedProcess::open(ready.agent_pid.unwrap());
    let failed = Plan::new("disconnect");
    let response = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&failed, false)),
        None,
    );
    assert_eq!(response.status, 502);
    assert!(response.json().get("error").is_some());
    assert!(agent.is_alive(), "本用例必须观察到连接退休但PID仍存活");
    let retry = Plan::new("text");
    let response = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&retry, false)),
        None,
    );
    assert_eq!(response.status, 502);
    let error = response.json();
    assert_eq!(error["error"]["code"], "agent_disconnected");
    let retiring = harness.call(test, "status", None);
    assert_eq!(retiring["ok"]["agent_pid"], ready.agent_pid.unwrap());
    assert!(agent.is_alive());
    assert_eq!(
        audit(&harness.audit)
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        1
    );
    assert!(!audit(&harness.audit)
        .iter()
        .any(|event| event["event"] == "started" && event["tag"] == retry.tag));
    std::fs::write(harness.audit.join("release-exit"), b"natural-exit").unwrap();
    agent.wait_gone();
    let next = Plan::new("text");
    let response = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&next, false)),
        None,
    );
    let status = response.status;
    let response = response.json();
    let failed_status = (status != 200).then(|| harness.call(test, "status", None));
    assert_eq!(
        status,
        200,
        "自然退出后首次新请求必须恢复：{response}；status={failed_status:?}；audit={:?}",
        audit(&harness.audit)
    );
    assert_eq!(completion(&response), next.text());
    assert_ne!(
        wait_event(&harness.audit, "started", &next.tag)["pid"],
        ready.agent_pid.unwrap()
    );
    assert!(!audit(&harness.audit)
        .iter()
        .any(|event| event["event"] == "started" && event["tag"] == retry.tag));
    assert_eq!(
        audit(&harness.audit)
            .iter()
            .filter(|event| event["event"] == "started" && event["tag"] == failed.tag)
            .count(),
        1
    );
    assert_eq!(
        audit(&harness.audit)
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        2
    );
}

/// 覆盖 AH-01/AH-09：真实窗口修改保存显示待应用；事件循环在draining中继续运行，显式应用等待HTTP完成。
#[test]
fn real_gui_pending_configuration_applies_only_after_inflight_completion() {
    if gui_child_operation() || child_operation() {
        return;
    }
    let test = "real_gui_pending_configuration_applies_only_after_inflight_completion";
    let harness = Harness::new();
    let config = harness.config("normal");
    assert!(harness
        .call(test, "save", Some(&config))
        .get("error")
        .is_none());
    let status = harness.ready(test);
    let result = harness.temp.path().join("gui-apply-result.json");
    let gui = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(CHILD_ROOT, &harness.root)
        .env("JCHTOOLS_ACP_PROCESS_ACTION", "gui")
        .env("JCHTOOLS_ACP_GUI_MODE", "reopen")
        .env(
            "JCHTOOLS_ACP_PROCESS_CONFIG",
            serde_json::to_string(&config).unwrap(),
        )
        .env("JCHTOOLS_ACP_PROCESS_RESULT", &result)
        .env("JCHTOOLS_TEST_STATE_DIR", &harness.root)
        .env(
            "JCHTOOLS_TEST_ACP_SERVICE_EXE",
            env!("CARGO_BIN_EXE_JchTools"),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let ready = wait_file(&harness.root.join("gui-ready.json"));
    assert_eq!(ready["agent_pid"], status.agent_pid.unwrap().to_string());
    let mut plan = Plan::new("text");
    plan.barrier = true;
    harness.register_hold(&plan);
    let mut stream = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&plan, true)),
        None,
    );
    assert_eq!(stream.delta(), plan.chunks[0]);
    let mut changed = config.clone();
    changed.port = unused_port();
    let next_audit = harness.temp.path().join("GUI应用后 中文 参数");
    std::fs::create_dir(&next_audit).unwrap();
    changed.arguments[1] = next_audit.to_string_lossy().into_owned();
    std::fs::write(
        harness.root.join("gui-save-next.json"),
        serde_json::to_vec(&changed).unwrap(),
    )
    .unwrap();
    let pending = wait_file(&harness.root.join("gui-pending.json"));
    assert_eq!(pending["agent_pid"], ready["agent_pid"]);
    assert!(pending["saved"]
        .as_str()
        .unwrap()
        .contains(&changed.port.to_string()));
    assert!(pending["running"]
        .as_str()
        .unwrap()
        .contains(&config.port.to_string()));
    assert_eq!(
        harness.call(test, "load", None)["config"],
        serde_json::to_value(&changed).unwrap()
    );
    std::fs::write(harness.root.join("gui-apply"), b"apply").unwrap();
    let draining = wait_file(&harness.root.join("gui-draining.json"));
    assert_eq!(draining["responsive"], true);
    assert_eq!(draining["agent_pid"], ready["agent_pid"]);
    assert!(!harness.root.join("gui-applied.json").exists());
    assert!(audit(&next_audit).is_empty());
    assert_admission_closed(harness.port);
    release(&harness.audit, &plan);
    assert_eq!(stream.finish_stream(), plan.chunks[1]);
    let applied = wait_file(&harness.root.join("gui-applied.json"));
    assert_ne!(applied["agent_pid"], ready["agent_pid"]);
    assert_eq!(applied["saved"], applied["running"]);
    let next = Plan::new("text");
    let response = Http::open(
        changed.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&next, false)),
        None,
    );
    assert_eq!(response.status, 200);
    assert_eq!(completion(&response.json()), next.text());
    wait_gone(status.agent_pid.unwrap());
    std::fs::write(harness.root.join("gui-close"), b"close").unwrap();
    assert_eq!(finish_child(gui, &result)["gui"], "closed");
}

/// 覆盖 AH-08/AH-A07：缺失命令和存在但不可执行的文件均显示具体启动错误，不冒充Ready。
#[test]
fn missing_or_unlaunchable_command_reports_start_failure() {
    if child_operation() {
        return;
    }
    let test = "missing_or_unlaunchable_command_reports_start_failure";
    for exists in [false, true] {
        let harness = Harness::new();
        let mut config = harness.config("normal");
        let invalid = harness.temp.path().join(if exists {
            "not executable.exe"
        } else {
            "missing Agent.exe"
        });
        if exists {
            std::fs::write(&invalid, b"synthetic non executable").unwrap();
        }
        config.executable = invalid.to_string_lossy().into_owned();
        let saved = harness.call(test, "save", Some(&config));
        assert_ne!(saved["ok"]["phase"], "ready");
        let error = harness.phase(test, "error");
        let message = error["error"].as_str().unwrap();
        assert!(
            message.contains("启动") && message.contains("Agent"),
            "{error}"
        );
        assert!(audit(&harness.audit).is_empty());
        assert!(error["agent_pid"].is_null());
    }
}
