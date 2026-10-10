//! 真实 ACP 验收支持：只调用产品 facade 和既有 ureq；不实现 HTTP/ACP 协议栈。
//! 所有消费者是本测试的独立子进程；不会修改测试宿主或用户的全局环境/认证/配置。
use jchtools::acp_api::{runtime, ServiceConfig, ServicePhase, ServiceStatus};
use serde_json::{json, Value};
use std::{
    cell::RefCell,
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        },
        Threading::{OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION},
    },
};

pub(crate) const MODEL: &str = "opencode/mimo-v2.6-flash-free";
const TEST: &str = "real_opencode_models_json_and_incremental_sse_single_agent";
const ACTION: &str = "JCHTOOLS_OPENCODE_TEST_ACTION";
const RESULT: &str = "JCHTOOLS_OPENCODE_TEST_RESULT";
const INPUT: &str = "JCHTOOLS_OPENCODE_TEST_INPUT";
const PORT: &str = "JCHTOOLS_OPENCODE_TEST_PORT";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

pub(crate) fn child_operation() -> bool {
    let Ok(action) = std::env::var(ACTION) else {
        return false;
    };
    let result = operation(&action);
    let value = match result {
        Ok(value) => json!({"ok": value}),
        Err(error) => json!({"error": error}),
    };
    std::fs::write(
        std::env::var_os(RESULT).expect("消费者结果路径缺失"),
        value.to_string(),
    )
    .unwrap();
    true
}
fn operation(action: &str) -> Result<Value, String> {
    match action {
        "save" => {
            let config: ServiceConfig =
                serde_json::from_str(&std::env::var(INPUT).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            runtime::save_config(&config)
                .map(|status| json!(status))
                .map_err(|e| e.to_string())
        }
        "ensure" => runtime::ensure_started()
            .map(|status| json!(status))
            .map_err(|e| e.to_string()),
        "status" => runtime::status()
            .map(|status| json!(status))
            .map_err(|e| e.to_string()),
        "stop" => runtime::stop()
            .map(|status| json!(status))
            .map_err(|e| e.to_string()),
        "models" | "json" | "sse" => http_request(action),
        _ => Err(format!("未知真实 ACP 消费者操作：{action}")),
    }
}
fn http_request(action: &str) -> Result<Value, String> {
    let port: u16 = std::env::var(PORT)
        .map_err(|e| e.to_string())?
        .parse::<u16>()
        .map_err(|e| e.to_string())?;
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(REQUEST_TIMEOUT)
        .timeout_write(Duration::from_secs(5))
        .timeout(REQUEST_TIMEOUT)
        .build();
    let started = Instant::now();
    let response = if action == "models" {
        agent
            .get(&format!("http://127.0.0.1:{port}/v1/models"))
            .call()
    } else {
        agent
            .post(&format!("http://127.0.0.1:{port}/v1/chat/completions"))
            .set("Content-Type", "application/json")
            .send_string(&std::env::var(INPUT).map_err(|e| e.to_string())?)
    };
    let response = match response {
        Ok(response) => response,
        Err(ureq::Error::Status(code, response)) => {
            let mut body = String::new();
            response
                .into_reader()
                .take(65536)
                .read_to_string(&mut body)
                .map_err(|e| e.to_string())?;
            return Err(format!("真实 HTTP {action} 返回 {code}：{body}"));
        }
        Err(error) => return Err(format!("真实 HTTP {action} 失败：{error}")),
    };
    if response.status() != 200 {
        return Err(format!("真实 HTTP {action} 返回 {}", response.status()));
    }
    let content_type = response
        .header("content-type")
        .unwrap_or_default()
        .to_owned();
    if action != "sse" {
        if !content_type.starts_with("application/json") {
            return Err(format!("JSON Content-Type 错误：{content_type}"));
        }
        let body: Value = serde_json::from_reader(response.into_reader().take(1024 * 1024))
            .map_err(|e| e.to_string())?;
        return Ok(json!({"body":body,"elapsed_ms":started.elapsed().as_millis()}));
    }
    if !content_type.starts_with("text/event-stream") {
        return Err(format!("SSE Content-Type 错误：{content_type}"));
    }
    // ureq 解码 HTTP；这里只消费 SSE 公开事件，实时记录首次文本而非先收全文再拆分。
    let mut reader = BufReader::new(response.into_reader().take(1024 * 1024));
    let mut line = String::new();
    let mut data = String::new();
    let mut text = String::new();
    let mut events = Vec::new();
    let mut content_deltas = 0;
    let mut first_delta_ms = None;
    let mut first_delta_status = Value::Null;
    let mut finish_reason = Value::Null;
    loop {
        line.clear();
        if reader
            .read_line(&mut line)
            .map_err(|e| format!("SSE 读取失败：{e}"))?
            == 0
        {
            return Err("真实 SSE 在 [DONE] 前截断".into());
        }
        if !line.trim_end_matches(['\r', '\n']).is_empty() {
            if let Some(part) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(
                    part.strip_prefix(' ')
                        .unwrap_or(part)
                        .trim_end_matches(['\r', '\n']),
                );
            }
            continue;
        }
        if data.is_empty() {
            continue;
        }
        if data == "[DONE]" {
            if finish_reason != "stop" {
                return Err(format!("[DONE] 前缺少正常 finish_reason：{finish_reason}"));
            }
            let done_ms = started.elapsed().as_millis();
            let mut trailing = String::new();
            reader
                .read_to_string(&mut trailing)
                .map_err(|e| format!("SSE 结束体读取失败：{e}"))?;
            if !trailing.trim().is_empty() {
                return Err(format!("[DONE] 后仍有额外 SSE 数据：{trailing}"));
            }
            return Ok(
                json!({"text":text,"events":events,"content_deltas":content_deltas,"first_delta_ms":first_delta_ms,"first_delta_status":first_delta_status,"finish_reason":finish_reason,"done":true,"done_ms":done_ms}),
            );
        }
        let event: Value =
            serde_json::from_str(&data).map_err(|e| format!("真实 SSE JSON 无效：{e}；{data}"))?;
        if event.get("error").is_some() {
            return Err(format!("真实 ACP SSE 错误：{event}"));
        }
        if event["object"] != "chat.completion.chunk" || event["model"] != MODEL {
            return Err(format!("真实 SSE 类型/模型不匹配：{event}"));
        }
        if let Some(delta) = event["choices"][0]["delta"]["content"]
            .as_str()
            .filter(|delta| !delta.is_empty())
        {
            if !finish_reason.is_null() {
                return Err("finish_reason 后仍收到文本增量".into());
            }
            content_deltas += 1;
            text.push_str(delta);
            if first_delta_ms.is_none() {
                first_delta_ms = Some(started.elapsed().as_millis());
                first_delta_status =
                    json!(runtime::status().map_err(|e| format!("首次增量状态读取失败：{e}"))?);
            }
        }
        if !event["choices"][0]["finish_reason"].is_null() {
            if !finish_reason.is_null() {
                return Err("SSE 重复结束事件".into());
            }
            finish_reason = event["choices"][0]["finish_reason"].clone();
        }
        events.push(event);
        data.clear();
    }
}

// 只强制回收本测试直接 spawn 的短命消费者，不触碰它启动的产品后台/Agent。
struct Consumer(Child);
impl Consumer {
    fn poll(&mut self) -> Option<ExitStatus> {
        self.0.try_wait().expect("消费者进程查询失败")
    }
    fn wait(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.poll() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for Consumer {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _killed = self.0.kill();
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.0.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

pub(crate) struct Harness {
    pub(crate) root: PathBuf,
    pub(crate) executable: PathBuf,
    port: u16,
    cleanup_processes: RefCell<Vec<OwnedProcess>>,
}
impl Harness {
    pub(crate) fn new() -> Self {
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let root = repo
            .join(".tmp")
            .join(format!("acp-opencode-{}", uuid::Uuid::new_v4().simple()));
        // 使用现有目录工厂，不自行生成测试工作区，不调用全局 clean。
        let mut generator = Consumer(
            Command::new("python")
                .arg(repo.join("scripts/make_tmp.py"))
                .args(["workspace", "--destination"])
                .arg(&root)
                .current_dir(&repo)
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("启动 make_tmp.py 失败"),
        );
        assert!(
            generator
                .wait(Duration::from_secs(20))
                .expect("make_tmp.py 超时")
                .success(),
            "make_tmp.py workspace 失败"
        );
        let executable = std::env::var_os("JCHTOOLS_TEST_OPENCODE_EXE").map_or_else(
            || PathBuf::from("C:/Users/jiang/AppData/Roaming/npm/node_modules/@opencode/cli/bin/opencode.exe"),
            PathBuf::from,
        );
        let reservation = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = reservation.local_addr().unwrap().port();
        let harness = Self {
            root,
            executable,
            port,
            cleanup_processes: RefCell::new(Vec::new()),
        };
        assert!(
            harness.executable.is_absolute() && harness.executable.is_file(),
            "必须提供真实 OpenCode 绝对程序路径；无 OMP/fixture fallback：{}",
            harness.executable.display()
        );
        harness
    }
    pub(crate) fn config(&self) -> ServiceConfig {
        ServiceConfig {
            executable: self.executable.to_str().unwrap().into(),
            arguments: vec!["acp".into()],
            port: self.port,
        }
    }
    pub(crate) fn record(&self, name: &str, value: &Value) {
        std::fs::write(
            self.root.join(name),
            serde_json::to_vec_pretty(value).unwrap(),
        )
        .unwrap();
    }
    pub(crate) fn version(&self) -> String {
        let path = self.root.join("opencode-version.txt");
        let mut version = Consumer(
            Command::new(&self.executable)
                .arg("--version")
                .current_dir(&self.root)
                .stdout(File::create(&path).unwrap())
                .stderr(File::create(self.root.join("version-stderr.txt")).unwrap())
                .spawn()
                .expect("真实 OpenCode --version 启动失败"),
        );
        assert!(
            version
                .wait(Duration::from_secs(20))
                .expect("OpenCode --version 超时")
                .success(),
            "真实 OpenCode 版本命令失败"
        );
        std::fs::read_to_string(path).unwrap().trim().to_owned()
    }
    fn launch(&self, action: &str, input: Option<&Value>) -> (Consumer, PathBuf) {
        let result = self
            .root
            .join(format!("{action}-{}.json", uuid::Uuid::new_v4().simple()));
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--ignored", "--exact", TEST, "--nocapture"])
            .current_dir(&self.root)
            .env(ACTION, action)
            .env(RESULT, &result)
            .env(PORT, self.port.to_string())
            .env("JCHTOOLS_TEST_STATE_DIR", self.root.join("state"))
            .env(
                "JCHTOOLS_TEST_ACP_SERVICE_EXE",
                env!("CARGO_BIN_EXE_JchTools"),
            )
            .env(
                "JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT",
                self.root.join("absent-snapshot-assets"),
            )
            .env("TMP", &self.root)
            .env("TEMP", &self.root)
            .stdout(Stdio::null())
            .stderr(
                File::create(self.root.join(format!(
                    "{action}-consumer-stderr-{}.txt",
                    uuid::Uuid::new_v4().simple()
                )))
                .unwrap(),
            );
        if let Some(input) = input {
            command.env(INPUT, input.to_string());
        }
        (
            Consumer(command.spawn().expect("真实 ACP 消费者启动失败")),
            result,
        )
    }
    pub(crate) fn call(&self, action: &str, input: Option<&Value>) -> Value {
        let (mut child, result) = self.launch(action, input);
        let timeout = if action == "stop" {
            Duration::from_secs(35)
        } else {
            Duration::from_secs(20)
        };
        assert!(
            child
                .wait(timeout)
                .unwrap_or_else(|| panic!("消费者 {action} 超时；证据：{}", self.root.display()))
                .success(),
            "消费者 {action} 异常退出；证据：{}",
            self.root.display()
        );
        let value: Value = serde_json::from_slice(&std::fs::read(result).unwrap()).unwrap();
        if let Ok(status) = serde_json::from_value::<ServiceStatus>(value["ok"].clone()) {
            let mut owned = self.cleanup_processes.borrow_mut();
            for pid in [status.service_pid, status.agent_pid].into_iter().flatten() {
                if !owned.iter().any(|process| process.pid == pid) {
                    if let Some(process) = OwnedProcess::try_open(pid) {
                        owned.push(process);
                    }
                }
            }
        }
        value
    }
    pub(crate) fn ready(&self) -> ServiceStatus {
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            let result = self.call("status", None);
            let status: ServiceStatus = serde_json::from_value(result["ok"].clone())
                .unwrap_or_else(|e| panic!("实际后台状态读取失败：{e}；{result}"));
            assert_ne!(
                status.phase,
                ServicePhase::Error,
                "真实 OpenCode/官方 SDK 初始化不兼容：{status:?}；证据：{}",
                self.root.display()
            );
            if status.phase == ServicePhase::Ready {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "真实后台未在有界时间内就绪：{status:?}"
            );
            thread::sleep(Duration::from_millis(100));
        }
    }
    pub(crate) fn observe(&self, service_pid: u32, agent_pid: u32) {
        let result = self.call("status", None);
        let status: ServiceStatus = serde_json::from_value(result["ok"].clone())
            .unwrap_or_else(|e| panic!("status 失败：{e}；{result}"));
        let children = direct_agents(
            service_pid,
            self.executable.file_name().unwrap().to_str().unwrap(),
        );
        let mut history = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("status.jsonl"))
            .unwrap();
        writeln!(
            history,
            "{}",
            json!({"status":status,"direct_agent_pids":children})
        )
        .unwrap();
        assert_eq!(
            status.phase,
            ServicePhase::Ready,
            "真实后台不再就绪：{status:?}"
        );
        assert_eq!(status.service_pid, Some(service_pid), "后台发生替换");
        assert_eq!(status.agent_pid, Some(agent_pid), "Agent 发生替换");
        assert!(status.executing <= 1, "本阶段只能单并发：{status:?}");
        assert_eq!(status.waiting, 0, "顺序请求不应产生等待队列");
        assert_eq!(
            children,
            vec![agent_pid],
            "系统进程快照未观察到唯一自有 OpenCode"
        );
    }
    pub(crate) fn request(
        &self,
        action: &str,
        input: Option<&Value>,
        service_pid: u32,
        agent_pid: u32,
    ) -> Value {
        let (mut child, path) = self.launch(action, input);
        let deadline = Instant::now() + REQUEST_TIMEOUT + Duration::from_secs(10);
        loop {
            self.observe(service_pid, agent_pid);
            if let Some(status) = child.poll() {
                assert!(
                    status.success(),
                    "HTTP 消费者 {action} 异常退出；证据：{}",
                    self.root.display()
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "真实 HTTP {action} 超时；证据：{}",
                self.root.display()
            );
            thread::sleep(Duration::from_millis(150));
        }
        let value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(
            value.get("ok").is_some(),
            "真实链路 {action} 失败：{value}；证据：{}",
            self.root.display()
        );
        value["ok"].clone()
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        // 不 panic、不无限等待、不强杀产品；失败路径仍向唯一隔离管道请求产品安全 Stop。
        let cleanup =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.call("stop", None)));
        match cleanup {
            Ok(result) => {
                let _written = std::fs::write(self.root.join("cleanup.json"), result.to_string());
                if result.get("error").is_some() {
                    eprintln!("自有服务安全清理失败：{result}；{}", self.root.display());
                }
            }
            Err(_) => eprintln!(
                "自有服务安全清理超时/异常；未强杀后台或 Agent；证据：{}",
                self.root.display()
            ),
        }
        let mut exits = Vec::new();
        for process in self.cleanup_processes.get_mut().iter() {
            // SAFETY: 只等待先前观察到的自有进程句柄；不会误等复用 PID 的用户进程。
            let exited = unsafe { WaitForSingleObject(process.handle, 15_000) } == WAIT_OBJECT_0;
            exits.push(json!({"pid":process.pid,"exited":exited}));
            if !exited {
                eprintln!(
                    "安全清理后自有进程 {} 未退出；未强杀；{}",
                    process.pid,
                    self.root.display()
                );
            }
        }
        let _written = std::fs::write(
            self.root.join("cleanup-processes.json"),
            json!(exits).to_string(),
        );
        // 保留本次诊断到 .tmp，供父代理报告/清理；不删除其他验收或用户产物。
    }
}

pub(crate) struct OwnedProcess {
    handle: HANDLE,
    pid: u32,
}
impl OwnedProcess {
    fn try_open(pid: u32) -> Option<Self> {
        // SAFETY: 只对本次 status 记录的自有进程取查询/等待句柄，不授予终止权限。
        let handle =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | 0x0010_0000, 0, pid) };
        if handle.is_null() {
            None
        } else {
            Some(Self { handle, pid })
        }
    }
    pub(crate) fn open(pid: u32) -> Self {
        Self::try_open(pid).unwrap_or_else(|| {
            panic!(
                "无法观察自有进程 {pid}：{}",
                std::io::Error::last_os_error()
            )
        })
    }
    pub(crate) fn wait_gone(&self) {
        // SAFETY: 句柄保持到 Drop，等待有界；进程身份由句柄固定，避免 PID 复用。
        let result = unsafe { WaitForSingleObject(self.handle, 15_000) };
        assert_ne!(
            result, WAIT_TIMEOUT,
            "安全 Stop 后自有进程 {} 未退出",
            self.pid
        );
        assert_eq!(result, WAIT_OBJECT_0, "自有进程 {} 退出观察失败", self.pid);
    }
}
impl Drop for OwnedProcess {
    fn drop(&mut self) {
        // SAFETY: 唯一持有的有效句柄只关闭一次；不终止进程。
        unsafe { CloseHandle(self.handle) };
    }
}
fn direct_agents(service_pid: u32, filename: &str) -> Vec<u32> {
    // SAFETY: 只读进程快照；不打开其他用户进程、不读取环境或凭据。
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    assert_ne!(snapshot, INVALID_HANDLE_VALUE, "进程快照失败");
    // 复用只关闭句柄的所有者，确保异常路径也关闭 snapshot。
    let snapshot = OwnedProcess {
        handle: snapshot,
        pid: 0,
    };
    // SAFETY: PROCESSENTRY32W 为普通 C 数据结构，零初始化后设置公开 API 要求的大小。
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = u32::try_from(std::mem::size_of::<PROCESSENTRY32W>()).unwrap();
    let mut pids = Vec::new();
    // SAFETY: 有效快照和已初始化大小的可写结构。
    let mut found = unsafe { Process32FirstW(snapshot.handle, &raw mut entry) };
    while found != 0 {
        let len = entry
            .szExeFile
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(entry.szExeFile.len());
        let image = String::from_utf16_lossy(&entry.szExeFile[..len]);
        if entry.th32ParentProcessID == service_pid && image.eq_ignore_ascii_case(filename) {
            pids.push(entry.th32ProcessID);
        }
        // SAFETY: 同一有效快照和结构继续只读枚举。
        found = unsafe { Process32NextW(snapshot.handle, &raw mut entry) };
    }
    pids.sort_unstable();
    pids
}
