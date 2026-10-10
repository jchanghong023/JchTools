//! AH-A13/AH-A14：真实 JchTools 控制管道与 HTTP 模型发现；不调用模型。
//! ACP Agent 是隔离合成 fixture，不代表真实 OpenCode/供应商端到端验收。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use jchtools::acp_api::{
    runtime, settings, ServiceConfig, ServiceDiscovery, ServicePhase, ServiceStatus,
};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const CHILD_ROOT: &str = "JCHTOOLS_DISCOVERY_TEST_ROOT";
const CHILD_TEST: &str = "discovery_isolated_consumer";

#[test]
fn discovery_isolated_consumer() {
    let Some(root) = std::env::var_os(CHILD_ROOT) else {
        return;
    };
    let action = std::env::var("JCHTOOLS_DISCOVERY_TEST_ACTION").unwrap();
    let result = match action.as_str() {
        "discover" => runtime::discover().map(|value| serde_json::to_value(value).unwrap()),
        "save" => {
            let config: ServiceConfig =
                serde_json::from_str(&std::env::var("JCHTOOLS_DISCOVERY_TEST_CONFIG").unwrap())
                    .unwrap();
            settings::save_config(&config).map(|()| Value::Null)
        }
        "apply" => runtime::apply_and_restart().map(|value| serde_json::to_value(value).unwrap()),
        "stop" => runtime::stop().map(|value| serde_json::to_value(value).unwrap()),
        "status" => runtime::status().map(|value| serde_json::to_value(value).unwrap()),
        "wire" => Ok(raw_discover(&PathBuf::from(root))),
        _ => panic!("未知隔离消费者操作 {action}"),
    };
    let output = match result {
        Ok(value) => json!({"ok": value}),
        Err(error) => json!({"error": error}),
    };
    std::fs::write(
        std::env::var_os("JCHTOOLS_DISCOVERY_TEST_RESULT").unwrap(),
        output.to_string(),
    )
    .unwrap();
}

struct Harness {
    temp: tempfile::TempDir,
    root: PathBuf,
    audit: PathBuf,
    service: Option<Child>,
}

impl Harness {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("状态 中文 空格");
        let audit = temp.path().join("fixture-audit");
        Self {
            temp,
            root,
            audit,
            service: None,
        }
    }

    fn isolated_command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .env(CHILD_ROOT, &self.root)
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
        command
    }

    fn call(&self, action: &str, config: Option<&ServiceConfig>) -> Value {
        let path = self
            .temp
            .path()
            .join(format!("result-{}.json", uuid::Uuid::new_v4()));
        let mut command = self.isolated_command(std::env::current_exe().unwrap());
        command
            .args(["--exact", CHILD_TEST, "--nocapture"])
            .env("JCHTOOLS_DISCOVERY_TEST_ACTION", action)
            .env("JCHTOOLS_DISCOVERY_TEST_RESULT", &path);
        if let Some(config) = config {
            command.env(
                "JCHTOOLS_DISCOVERY_TEST_CONFIG",
                serde_json::to_string(config).unwrap(),
            );
        }
        let mut child = command.spawn().unwrap();
        wait_exit(&mut child);
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    fn config(&self, port: u16) -> ServiceConfig {
        ServiceConfig {
            executable: env!("CARGO_BIN_EXE_jchtools-acp-fixture").into(),
            arguments: vec![
                "--agent".into(),
                self.audit.to_str().unwrap().into(),
                "normal".into(),
                "private-secret-must-not-leak".into(),
            ],
            port,
        }
    }

    fn start(&mut self) {
        assert!(self.service.is_none());
        self.service = Some(
            self.isolated_command(env!("CARGO_BIN_EXE_JchTools"))
                .arg("--acp-http-service")
                .stdin(Stdio::null())
                .spawn()
                .unwrap(),
        );
    }

    fn discover_phase(&self, phase: ServicePhase) -> ServiceDiscovery {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let result = self.call("discover", None);
            if let Some(value) = result.get("ok").filter(|value| !value.is_null()) {
                let descriptor: ServiceDiscovery = serde_json::from_value(value.clone()).unwrap();
                if descriptor.phase == phase {
                    return descriptor;
                }
            }
            assert!(Instant::now() < deadline, "后台未进入 {phase:?}: {result}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn stop(&mut self) {
        let result = self.call("stop", None);
        assert!(result.get("error").is_none(), "{result}");
        wait_exit(self.service.as_mut().unwrap());
        self.service = None;
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(mut service) = self.service.take() {
            let _result = self.call("stop", None);
            wait_exit(&mut service);
        }
    }
}

fn wait_exit(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(exit) = child.try_wait().unwrap() {
            assert!(exit.success(), "隔离进程退出失败: {exit}");
            return;
        }
        assert!(Instant::now() < deadline, "隔离进程未收尾: {}", child.id());
        thread::sleep(Duration::from_millis(10));
    }
}

fn unused_port() -> u16 {
    std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn models(descriptor: &ServiceDiscovery) -> Value {
    let response = ureq::get(&format!(
        "{}/v1/models",
        descriptor.base_url.as_ref().unwrap()
    ))
    .timeout(Duration::from_secs(10))
    .call()
    .unwrap();
    serde_json::from_reader(response.into_reader()).unwrap()
}

#[test]
fn absent_discovery_does_not_read_saved_config_create_state_or_start_service() {
    let harness = Harness::new();
    assert_eq!(harness.call("discover", None), json!({"ok": null}));
    assert!(!harness.root.exists(), "发现不存在的后台不得创建配置目录");
    std::fs::create_dir(&harness.root).unwrap();
    std::fs::write(harness.root.join("config.sqlite3"), b"invalid SQLite").unwrap();
    assert_eq!(harness.call("discover", None), json!({"ok": null}));
    assert_eq!(
        std::fs::read(harness.root.join("config.sqlite3")).unwrap(),
        b"invalid SQLite"
    );
    assert!(!harness.audit.exists(), "发现不得启动 Agent");
    assert!(!harness.root.join("acp-workspace").exists());
}

#[test]
fn ready_discovery_wire_is_minimal_uses_running_port_and_tracks_apply_and_restart() {
    let mut harness = Harness::new();
    let original = harness.config(unused_port());
    assert_eq!(harness.call("save", Some(&original)), json!({"ok": null}));
    assert_eq!(harness.call("discover", None), json!({"ok": null}));
    harness.start();
    let first = harness.discover_phase(ServicePhase::Ready);
    let pid = harness.service.as_ref().unwrap().id();
    let first_models = models(&first);
    assert_eq!(first_models["data"][0]["id"], "fixture-model-a");
    assert_eq!(first_models["data"][1]["id"], "fixture-model-b");
    let audit_before = std::fs::read(harness.audit.join("audit.jsonl")).unwrap();
    let wire = harness.call("wire", None);
    assert_eq!(
        wire,
        json!({"ok": {
            "server_pid": pid,
            "response": {"protocol": 1, "pid": pid, "result": {"Ok": {
                "protocol_version": 1,
                "service_id": "jchtools-acp-http",
                "instance_id": first.instance_id,
                "phase": "ready",
                "base_url": format!("http://127.0.0.1:{}", original.port),
                "execution_mode": "server_agent",
                "capabilities": {"text":true,"streaming":true,"client_tools":false,"server_tools":true}
            }}}
        }})
    );
    assert_eq!(harness.discover_phase(ServicePhase::Ready), first);
    assert_eq!(
        std::fs::read(harness.audit.join("audit.jsonl")).unwrap(),
        audit_before,
        "Discover 不得建立 ACP 会话或触发 Agent 请求"
    );
    let mut changed = harness.config(unused_port());
    while changed.port == original.port {
        changed.port = unused_port();
    }
    assert_eq!(harness.call("save", Some(&changed)), json!({"ok": null}));
    let status = harness.call("status", None);
    assert_eq!(status["ok"]["saved_config"]["port"], changed.port);
    assert_eq!(status["ok"]["running_config"]["port"], original.port);
    let unapplied = harness.discover_phase(ServicePhase::Ready);
    assert_eq!(unapplied, first);
    assert_eq!(models(&unapplied), first_models);

    // 损坏的磁盘配置不污染当前 Ready 投影；发现不读取它，也不改变后台。
    let db = rusqlite::Connection::open(harness.root.join("config.sqlite3")).unwrap();
    db.execute(
        "UPDATE app_settings SET value='broken-private-config' WHERE key='acp_http_config'",
        [],
    )
    .unwrap();
    assert_eq!(harness.discover_phase(ServicePhase::Ready), first);
    let stored: String = db
        .query_row(
            "SELECT value FROM app_settings WHERE key='acp_http_config'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, "broken-private-config");
    drop(db);
    assert_eq!(harness.call("save", Some(&changed)), json!({"ok": null}));
    assert!(harness.call("apply", None).get("error").is_none());
    let applied = harness.discover_phase(ServicePhase::Ready);
    assert_eq!(applied.instance_id, first.instance_id);
    assert_eq!(
        applied.base_url,
        Some(format!("http://127.0.0.1:{}", changed.port))
    );
    assert_eq!(models(&applied), first_models);
    harness.stop();
    assert_eq!(harness.call("discover", None), json!({"ok": null}));
    harness.start();
    let recreated = harness.discover_phase(ServicePhase::Ready);
    assert_ne!(recreated.instance_id, first.instance_id);
    assert_eq!(recreated.base_url, applied.base_url);
    assert_eq!(models(&recreated), first_models);
    harness.stop();
}

#[test]
fn existing_unconfigured_and_failed_backends_discover_with_null_base_url() {
    let mut harness = Harness::new();
    harness.start();
    let unconfigured = harness.discover_phase(ServicePhase::Unconfigured);
    assert!(unconfigured.base_url.is_none());
    assert!(!harness.audit.exists());
    assert_eq!(
        harness.discover_phase(ServicePhase::Unconfigured),
        unconfigured
    );
    harness.stop();

    std::fs::write(harness.root.join("config.sqlite3"), b"invalid SQLite").unwrap();
    harness.start();
    let failed = harness.discover_phase(ServicePhase::Error);
    assert!(failed.base_url.is_none());
    assert_ne!(failed.instance_id, unconfigured.instance_id);
    assert!(!harness.audit.exists());
    harness.stop();
}

#[test]
fn descriptor_never_advertises_nonready_or_saved_only_endpoints() {
    let instance = uuid::Uuid::new_v4();
    let config = ServiceConfig {
        executable: "private-executable".into(),
        arguments: vec!["private-secret".into()],
        port: 43210,
    };
    for phase in [
        ServicePhase::Unconfigured,
        ServicePhase::Starting,
        ServicePhase::Draining,
        ServicePhase::Stopping,
        ServicePhase::Stopped,
        ServicePhase::Error,
    ] {
        let status = ServiceStatus {
            phase,
            running_config: Some(config.clone()),
            saved_config: Some(config.clone()),
            error: Some("private-error".into()),
            ..ServiceStatus::default()
        };
        let descriptor = ServiceDiscovery::from_status(instance, &status);
        assert_eq!(descriptor.phase, phase);
        assert!(descriptor.base_url.is_none());
        assert_eq!(descriptor.instance_id, instance);
        let wire = serde_json::to_value(&descriptor).unwrap();
        assert_eq!(wire.as_object().unwrap().len(), 7);
        assert!(wire["base_url"].is_null());
        assert!(!wire.to_string().contains("private"));
    }
    let saved_only = ServiceStatus {
        phase: ServicePhase::Ready,
        saved_config: Some(config),
        ..ServiceStatus::default()
    };
    assert!(ServiceDiscovery::from_status(instance, &saved_only)
        .base_url
        .is_none());
}

// 独立消费者按公开用户 SID/会话/隔离根契约组装管道，不枚举端口或管道。
fn raw_discover(root: &Path) -> Value {
    use sha2::{Digest, Sha256};
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::{LocalFree, HANDLE},
        Security::{
            Authorization::ConvertSidToStringSidW, EqualSid, GetTokenInformation, TokenUser,
            TOKEN_QUERY, TOKEN_USER,
        },
        System::{
            Pipes::{GetNamedPipeServerProcessId, GetNamedPipeServerSessionId},
            Threading::{
                GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
            },
        },
    };
    #[link(name = "kernel32")]
    extern "system" {
        fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> i32;
    }
    struct LocalSid(*mut std::ffi::c_void);
    impl Drop for LocalSid {
        fn drop(&mut self) {
            // SAFETY: 本结构唯一持有转换 API 分配的 SID 字符串。
            unsafe { LocalFree(self.0) };
        }
    }
    fn token_user(process: HANDLE) -> Vec<usize> {
        let mut token = std::ptr::null_mut();
        assert_ne!(
            // SAFETY: process 在查询期间有效，输出 token 指针有效。
            unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) },
            0
        );
        // SAFETY: OpenProcessToken 成功返回唯一拥有的句柄，转交 RAII。
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let mut size = 0;
        // SAFETY: 零长查询获取 TokenUser 所需缓冲区大小。
        unsafe {
            GetTokenInformation(
                token.as_raw_handle().cast(),
                TokenUser,
                std::ptr::null_mut(),
                0,
                &raw mut size,
            )
        };
        assert!(size >= u32::try_from(std::mem::size_of::<TOKEN_USER>()).unwrap());
        let mut data = vec![
            0_usize;
            usize::try_from(size)
                .unwrap()
                .div_ceil(std::mem::size_of::<usize>())
        ];
        assert_ne!(
            // SAFETY: 缓冲区正确对齐且至少 size 字节，输出长度有效。
            unsafe {
                GetTokenInformation(
                    token.as_raw_handle().cast(),
                    TokenUser,
                    data.as_mut_ptr().cast(),
                    size,
                    &raw mut size,
                )
            },
            0
        );
        data
    }
    // SAFETY: 取得当前进程的有效伪句柄，不持有需关闭的资源。
    let process = unsafe { GetCurrentProcess() };
    let data = token_user(process);
    // SAFETY: 成功查询返回有效 TOKEN_USER；SID 在 data 生命周期内有效。
    let user = unsafe { &*data.as_ptr().cast::<TOKEN_USER>() };
    let mut sid = std::ptr::null_mut();
    assert_ne!(
        // SAFETY: 输出 SID 字符串由系统分配，成功后转交 RAII。
        unsafe { ConvertSidToStringSidW(user.User.Sid, &raw mut sid) },
        0
    );
    let allocation = LocalSid(sid.cast());
    let mut length = 0;
    // SAFETY: 成功转换产生以零终止的 UTF-16 字符串。
    while unsafe { *sid.wrapping_add(length) } != 0 {
        length += 1;
    }
    // SAFETY: 前述扫描确定合法字符串范围。
    let sid_text = String::from_utf16(unsafe { std::slice::from_raw_parts(sid, length) }).unwrap();
    drop(allocation);
    let mut session = 0;
    assert_ne!(
        // SAFETY: 查询当前进程的实际会话 ID，输出指针有效。
        unsafe { ProcessIdToSessionId(std::process::id(), &raw mut session) },
        0
    );
    let digest = Sha256::digest(root.as_os_str().to_string_lossy().as_bytes());
    let name = format!(r"\\.\pipe\jchtools-acp-http-{sid_text}-{session}-{digest:x}");
    let mut pipe = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(name)
        .unwrap();
    let mut actual_pid = 0;
    assert_ne!(
        // SAFETY: pipe 独占有效管道句柄，输出 PID 指针有效。
        unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle().cast(), &raw mut actual_pid) },
        0
    );
    let mut server_session = 0;
    assert_ne!(
        // SAFETY: 已连接的管道有效，会话输出指针有效。
        unsafe {
            GetNamedPipeServerSessionId(pipe.as_raw_handle().cast(), &raw mut server_session)
        },
        0
    );
    assert_eq!(server_session, session, "独立消费者拒绝跨登录会话服务");
    // SAFETY: PID 来自内核管道端点，只使用最小进程查询权限，不改变进程。
    let peer = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, actual_pid) };
    assert!(!peer.is_null(), "必须读取真实管道服务端身份");
    // SAFETY: OpenProcess 成功返回本次唯一拥有的句柄，转交 RAII。
    let peer = unsafe { OwnedHandle::from_raw_handle(peer) };
    let peer_data = token_user(peer.as_raw_handle().cast());
    // SAFETY: 成功 TokenUser 查询返回正确对齐且存活的 TOKEN_USER。
    let peer_user = unsafe { &*peer_data.as_ptr().cast::<TOKEN_USER>() };
    assert_ne!(
        // SAFETY: 两个 SID 在各自仍存活的 TokenUser 缓冲区内。
        unsafe { EqualSid(peer_user.User.Sid, user.User.Sid) },
        0,
        "独立消费者拒绝其他用户的服务"
    );
    let mut peer_session = 0;
    assert_ne!(
        // SAFETY: actual_pid 来自内核端点，peer 持有此进程，输出指针有效。
        unsafe { ProcessIdToSessionId(actual_pid, &raw mut peer_session) },
        0
    );
    assert_eq!(peer_session, session);
    let request = b"\"Discover\"";
    pipe.write_all(&u32::try_from(request.len()).unwrap().to_le_bytes())
        .unwrap();
    pipe.write_all(request).unwrap();
    pipe.flush().unwrap();
    let mut prefix = [0_u8; 4];
    pipe.read_exact(&mut prefix).unwrap();
    let length = usize::try_from(u32::from_le_bytes(prefix)).unwrap();
    assert!(length <= 1024 * 1024);
    let mut bytes = vec![0_u8; length];
    pipe.read_exact(&mut bytes).unwrap();
    json!({"response":serde_json::from_slice::<Value>(&bytes).unwrap(),"server_pid":actual_pid})
}
