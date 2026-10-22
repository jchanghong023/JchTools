#![warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    future_incompatible,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]

//! AH-06～AH-10/AH-13：真实JchTools后台进程、SQLite、单实例及SDK Agent生命周期。
//! 此处合成 Agent 不是实际模型；真实基准为 OpenCode v2.0.26 / OpenCode Zen MiMo-V2.6-Flash Free。
#![allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "fixtures/acp_api/main.rs"]
mod fixture;
use fixture::{
    consumer::{audit, body, completion, config, release, unused_port, wait_event, Http},
    Plan,
};
use jchtools_acp_api::acp_api::{runtime, settings, ServiceConfig, ServiceStatus};
use serde_json::{json, Value};
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[path = "acp_service_process/discover.rs"]
mod discover;
#[path = "acp_service_process/gui.rs"]
mod gui;
#[path = "acp_service_process/maintenance.rs"]
mod maintenance;
#[path = "acp_service_process/recover.rs"]
mod recover;
#[path = "acp_service_process/service_lifecycle.rs"]
mod service_lifecycle;

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
        "serve-held-exit" => {
            // 测试进程执行真实后台，唯有 serve 返回后才进入 OS 退出尾段。
            // 不向产品入口或 release 构建加入 hook，也不模拟 IPC/进程句柄。
            runtime::serve().unwrap();
            std::fs::write(root.join("service-exit-tail-entered"), b"entered").unwrap();
            let deadline = Instant::now() + Duration::from_secs(90);
            while !root.join("release-service-exit-tail").exists() {
                assert!(Instant::now() < deadline, "后台退出尾段没有收到放行");
                thread::sleep(Duration::from_millis(10));
            }
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
    service_executable: PathBuf,
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
            service_executable: PathBuf::from(env!("CARGO_BIN_EXE_JchTools")),
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
    fn launch(
        &self,
        test: &str,
        action: &str,
        config: Option<&ServiceConfig>,
    ) -> (ConsumerChild, PathBuf) {
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
            .env("JCHTOOLS_TEST_ACP_SERVICE_EXE", &self.service_executable)
            .env(
                "JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT",
                self.temp.path().join("absent-snapshot-assets"),
            )
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        // P2-03：仅此真实半头回归订阅 Hyper 底层 socket 的首读同步信号。
        // 测试拆分后 --exact 名带模块前缀（recover::…），按子串匹配两种形态。
        if test.contains("review_remediation_http_partial_header_") {
            command.env("JCHTOOLS_TEST_ACP_HTTP_READ_DIR", &self.root);
        }
        if let Some(config) = config {
            command.env(
                "JCHTOOLS_ACP_PROCESS_CONFIG",
                serde_json::to_string(config).unwrap(),
            );
        }
        (ConsumerChild(command.spawn().unwrap()), result)
    }
    fn call(&self, test: &str, action: &str, config: Option<&ServiceConfig>) -> Value {
        let (child, result) = self.launch(test, action, config);
        finish_child(child, &result)
    }
    fn maintenance(&self) -> ConsumerChild {
        ConsumerChild(
            Command::new(env!("CARGO_BIN_EXE_JchTools"))
                .args(["--acp-http-service", "--stop"])
                .env("JCHTOOLS_TEST_STATE_DIR", &self.root)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
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
            .args([
                "--exact",
                "maintenance::cleanup_isolated_service",
                "--nocapture",
            ])
            .env(CHILD_ROOT, &self.root)
            .env("JCHTOOLS_ACP_PROCESS_ACTION", "stop")
            .env("JCHTOOLS_ACP_PROCESS_RESULT", result)
            .env("JCHTOOLS_TEST_STATE_DIR", &self.root)
            .env("JCHTOOLS_TEST_ACP_SERVICE_EXE", &self.service_executable)
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

// Child 的析构不会结束进程；所有直接启动的测试消费者都必须在断言失败时回收。
// 这里只拥有测试/GUI 消费者，不拥有产品后台、Agent 或终端。
struct ConsumerChild(Child);
impl std::ops::Deref for ConsumerChild {
    type Target = Child;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for ConsumerChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Drop for ConsumerChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _killed = self.0.kill();
            let _exit = wait_consumer(&mut self.0, Duration::from_secs(5));
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
fn finish_child(mut child: ConsumerChild, path: &Path) -> Value {
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
    consumer: Option<ConsumerChild>,
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
// 此子进程执行真实后台，不能使用会在 Drop 强杀消费者的 ConsumerChild。
// 失败清理先放行尾段、再请求真实 Stop，只等待自然退出。
struct HeldServiceExit<'a> {
    harness: &'a Harness,
    child: Child,
}
impl Drop for HeldServiceExit<'_> {
    fn drop(&mut self) {
        let release = self.harness.root.join("release-service-exit-tail");
        if !release.exists() {
            let _released = std::fs::write(release, b"cleanup");
        }
        if matches!(self.child.try_wait(), Ok(None)) {
            let (mut stop, _) =
                self.harness
                    .launch("maintenance::cleanup_isolated_service", "stop", None);
            let _stopped = wait_consumer(&mut stop, Duration::from_secs(30));
            let _exited = wait_consumer(&mut self.child, Duration::from_secs(30));
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
            let error = std::io::Error::last_os_error();
            // 仅无效 PID 表示进程不存在；拒绝访问等查询失败不能冒充退出成功。
            assert_eq!(
                error.raw_os_error(),
                Some(windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER as i32),
                "无法观察自有进程 {pid}：{error}"
            );
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

fn assert_admission_closed(port: u16, cwd: &Path) {
    use std::io::{BufRead, BufReader, Write};
    let mut socket = match std::net::TcpStream::connect(("127.0.0.1", port)) {
        Ok(socket) => socket,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
            ) =>
        {
            return
        }
        Err(error) => panic!("无法验证drain后停止接收请求: {error}"),
    };
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let request = body(&Plan::new("text"), false, cwd).to_string();
    if let Err(error) = write!(socket,"POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{request}",request.len()) {
        assert!(
            matches!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
            ),
            "drain后请求写入失败不可作为停止接收的证据: {error}"
        );
        return;
    }
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
