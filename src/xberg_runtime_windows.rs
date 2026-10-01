//! Windows 命名管道代理。锁与管道名不包含运行目录，保存不同目录也不会另起引擎。
use super::startup_config;
use crate::xberg_settings;
use fs2::FileExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, GetCurrentThreadId, OpenThread, THREAD_TERMINATE,
};

#[link(name = "kernel32")]
extern "system" {
    fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> i32;
}
use windows_sys::Win32::System::IO::CancelSynchronousIo;

const QUERY_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_MESSAGE: u64 = 140 * 1024 * 1024;
struct PendingRequest {
    sender: mpsc::Sender<Value>,
    lane: Option<&'static str>,
}
type Pending = Arc<Mutex<HashMap<String, PendingRequest>>>;
const BROKER_PROTOCOL: u64 = 2;

fn identity() -> Result<String, String> {
    let mut session = 0;
    // SAFETY: 当前进程 ID 与有效可写 session 指针。
    let pid = unsafe { GetCurrentProcessId() };
    // SAFETY: session 指针有效，pid 为当前进程。
    let ok = unsafe { ProcessIdToSessionId(pid, &raw mut session) };
    if ok == 0 {
        return Err("无法查询 Windows 登录会话".into());
    }
    let root = xberg_settings::state_dir()?;
    let digest = Sha256::digest(format!("{}:{session}", root.display()).as_bytes());
    Ok(format!("{digest:x}"))
}

fn pipe_name() -> Result<String, String> {
    Ok(format!(r"\\.\pipe\jchtools-xberg-{}", identity()?))
}

fn stopped_marker() -> Result<PathBuf, String> {
    Ok(xberg_settings::state_dir()?.join(format!("background-stopped-{}", identity()?)))
}
pub(super) fn resume_background() -> Result<(), String> {
    match std::fs::remove_file(stopped_marker()?) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("恢复后台启动状态失败：{e}")),
    }
}
pub(super) fn background_allowed() -> Result<(), String> {
    if stopped_marker()?.exists() {
        Err("后台已从托盘退出；重新打开 JchTools 后自动启动".into())
    } else {
        Ok(())
    }
}
pub(super) fn background_control(stop: bool) -> Result<Value, String> {
    background_command(if stop { "broker-stop" } else { "broker-state" })
}
pub(super) fn force_background_exit() -> Result<Value, String> {
    background_command("broker-force-stop")
}
fn background_command(command: &str) -> Result<Value, String> {
    if command != "broker-state" {
        let marker = stopped_marker()?;
        if let Some(parent) = marker.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(marker, b"stopped").map_err(|e| e.to_string())?;
    }
    let name = pipe_name()?;
    let pipe = match OpenOptions::new().read(true).write(true).open(name) {
        Ok(pipe) => pipe,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({"ok":true,"running":false,"active":0}))
        }
        Err(e) => return Err(format!("后台控制连接失败：{e}")),
    };
    exchange(
        pipe,
        &json!({"request":{"id":"background-control", "command":command}}),
        QUERY_TIMEOUT,
    )
}

/// 看门狗反复取消当前线程同步 I/O，覆盖期限到达恰在两次 I/O 之间的竞态。
struct IoDeadline {
    done: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl IoDeadline {
    fn new(timeout: Duration) -> Result<Self, String> {
        // SAFETY: 打开当前线程的真实句柄，供看门狗取消同步 I/O；不继承句柄。
        let id = unsafe { GetCurrentThreadId() };
        // SAFETY: 当前线程有效，不继承句柄。
        let handle = unsafe { OpenThread(THREAD_TERMINATE, 0, id) };
        if handle.is_null() {
            return Err("无法设置共享管道通信期限".into());
        }
        let raw = handle as usize;
        let (done, receiver) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            if matches!(
                receiver.recv_timeout(timeout),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                loop {
                    // SAFETY: 本线程独占 raw 句柄，目标线程在 guard 析构前存活。
                    unsafe {
                        CancelSynchronousIo(raw as _);
                    }
                    if !matches!(
                        receiver.recv_timeout(Duration::from_millis(5)),
                        Err(mpsc::RecvTimeoutError::Timeout)
                    ) {
                        break;
                    }
                }
            }
            // SAFETY: raw 是本看门狗独占、未关闭的线程句柄。
            unsafe {
                CloseHandle(raw as _);
            }
        });
        Ok(Self {
            done,
            thread: Some(thread),
        })
    }
}
impl Drop for IoDeadline {
    fn drop(&mut self) {
        let _ = self.done.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn read_json(reader: &mut impl BufRead) -> Result<Value, String> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_MESSAGE + 1)
        .read_until(b'\n', &mut bytes)
        .map_err(|e| format!("共享 Xberg 读取失败：{e}"))?;
    if bytes.len() as u64 > MAX_MESSAGE || bytes.last() != Some(&b'\n') {
        return Err("共享 Xberg 响应中断或超过消息上限".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| "共享 Xberg 返回了无效 JSON".into())
}
fn write_json(writer: &mut impl Write, value: &Value) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_MESSAGE {
        return Err("Xberg 请求超过消息上限".into());
    }
    bytes.push(b'\n');
    writer
        .write_all(&bytes)
        .and_then(|()| writer.flush())
        .map_err(|e| format!("共享 Xberg 写入失败：{e}"))
}

fn connect(start: bool) -> Result<File, String> {
    let name = pipe_name()?;
    let open = || OpenOptions::new().read(true).write(true).open(&name);
    if let Ok(pipe) = open() {
        return Ok(pipe);
    }
    if start {
        background_allowed()?;
        // 代理是 JchTools/截图服务自身的内部模式；只有赢得会话锁的代理可创建引擎。
        let executable =
            if cfg!(debug_assertions) && std::env::var_os("JCHTOOLS_TEST_STATE_DIR").is_some() {
                std::env::var_os("JCHTOOLS_TEST_BROKER_EXE").map(PathBuf::from)
            } else {
                None
            }
            .map_or_else(|| std::env::current_exe().map_err(|e| e.to_string()), Ok)?;
        Command::new(executable)
            .arg("--xberg-broker")
            .creation_flags(0x0800_0000)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("无法启动共享 Xberg 代理：{e}"))?;
    }
    let end = Instant::now() + QUERY_TIMEOUT;
    loop {
        if let Ok(pipe) = open() {
            return Ok(pipe);
        }
        if Instant::now() >= end {
            return Err("共享 Xberg 代理连接超时".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn exchange(mut pipe: File, envelope: &Value, timeout: Duration) -> Result<Value, String> {
    let _deadline = IoDeadline::new(timeout)?;
    write_json(&mut pipe, envelope)?;
    let response = read_json(&mut BufReader::new(pipe))?;
    if response["jchtools_broker_protocol"] != BROKER_PROTOCOL {
        return Err("常驻 Xberg 代理协议不兼容，请结束旧版本会话后重试；未启动第二个引擎".into());
    }
    if response["id"] != envelope["request"]["id"] {
        return Err("共享 Xberg 响应 ID 不匹配".into());
    }
    Ok(response)
}

pub(super) fn request(
    root: &Path,
    value: &Value,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<Value, String> {
    let root = std::fs::canonicalize(root).map_err(|e| format!("Xberg 目录不可读：{e}"))?;
    let pipe = connect(true)?;
    let envelope = json!({"runtime_dir":root, "request":value});
    let cloned = envelope.clone();
    let (send, receive) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let _ = send.send(exchange(pipe, &cloned, timeout + QUERY_TIMEOUT));
    });
    let end = Instant::now() + timeout;
    let mut cancellation_sent = false;
    let mut cancellation_error = None;
    loop {
        match receive.recv_timeout(Duration::from_millis(10)) {
            Ok(result) => {
                let _ = reader.join();
                return match (result, cancellation_error) {
                    (Err(error), Some(cancel_error)) => Err(format!("取消接口失败：{cancel_error}；{error}；代理仍保留未结束任务，阻止同场景重入")),
                    (result, _) => result,
                };
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = reader.join();
                return Err("共享 Xberg 响应线程退出".into());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if !cancellation_sent && (cancel.load(Ordering::Acquire) || Instant::now() >= end) {
            cancellation_sent = true;
            let cancellation = json!({"runtime_dir":root,"request":{"id":format!("cancel-{}", value["id"].as_str().unwrap_or_default()),"command":"cancel","target_id":value["id"]}});
            let result =
                connect(false).and_then(|pipe| exchange(pipe, &cancellation, QUERY_TIMEOUT));
            if let Err(error) = result.and_then(super::checked) {
                cancellation_error = Some(error);
            }
            // 接受取消不等于任务结束；继续等待原请求的终态，禁止伪报成功。
        }
    }
}

struct Engine {
    child: Child,
    root: PathBuf,
    outgoing: mpsc::SyncSender<Value>,
    pending: Pending,
    broken: Arc<AtomicBool>,
    _job: Job,
}
impl Engine {
    fn spawn(root: &Path) -> Result<Self, String> {
        reject_existing_engine()?;
        let mut command = Command::new(root.join("xberg.exe"));
        command
            .args(["worker", "--no-config-discovery", "--config-json"])
            .arg(startup_config(root)?.to_string())
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(0x0800_0000);
        for name in [
            "HF_HUB_OFFLINE",
            "HUGGINGFACE_HUB_OFFLINE",
            "TRANSFORMERS_OFFLINE",
            "HF_DATASETS_OFFLINE",
            "NO_COLOR",
        ] {
            command.env(name, "1");
        }
        command
            .env("ORT_DYLIB_PATH", root.join("onnxruntime.dll"))
            .env("HF_HOME", root)
            .env("HF_HUB_CACHE", root.join("models"))
            .env("XBERG_MAX_REQUEST_BODY_BYTES", "104857600")
            .env("XBERG_API_ALLOW_LOCAL_URI_INPUTS", "1")
            .env("XBERG_ORT_EP", "cpu")
            .env("XBERG_SENSEVOICE_MODEL_DIR", root.join("models"))
            .env("XBERG_SHERPA_DLL_DIR", root.join("sherpa-onnx"))
            .env("XBERG_FFMPEG_DLL_DIR", root.join("ffmpeg"))
            .env(
                "XBERG_CACHE_DIR",
                xberg_settings::state_dir()?.join("xberg-cache"),
            )
            .env(
                "XBERG_PERF_LOG_DIR",
                std::env::temp_dir().join("JchTools-xberg-perf"),
            );
        let mut child = command
            .spawn()
            .map_err(|e| format!("共享 Xberg 启动失败：{e}"))?;
        let job = match Job::attach(&child) {
            Ok(job) => job,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let mut stdin = child.stdin.take().ok_or("Xberg stdin 缺失")?;
        let stdout = child.stdout.take().ok_or("Xberg stdout 缺失")?;
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let broken = Arc::new(AtomicBool::new(false));
        let (outgoing, incoming) = mpsc::sync_channel(32);
        let writer_pending = Arc::clone(&pending);
        let writer_broken = Arc::clone(&broken);
        std::thread::spawn(move || {
            for request in incoming {
                let result = IoDeadline::new(QUERY_TIMEOUT)
                    .and_then(|_guard| write_json(&mut stdin, &request));
                if result.is_err() {
                    fail_pending(&writer_pending, &writer_broken);
                    break;
                }
            }
        });
        let reader_pending = Arc::clone(&pending);
        let reader_broken = Arc::clone(&broken);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Ok(response) = read_json(&mut reader) {
                let Some(id) = response["id"].as_str() else {
                    break;
                };
                let sender = reader_pending
                    .lock()
                    .ok()
                    .and_then(|mut entries| entries.remove(id));
                if let Some(sender) = sender {
                    let _ = sender.sender.send(response);
                }
            }
            fail_pending(&reader_pending, &reader_broken);
        });
        Ok(Self {
            child,
            root: root.to_path_buf(),
            outgoing,
            pending,
            broken,
            _job: job,
        })
    }
    fn submit(&self, request: Value) -> Result<mpsc::Receiver<Value>, String> {
        let id = request["id"].as_str().ok_or("请求 ID 无效")?.to_string();
        let (send, receive) = mpsc::channel();
        let mut pending = self.pending.lock().map_err(|_| "共享请求表异常")?;
        if self.broken.load(Ordering::Acquire) {
            return Err("共享 Xberg 通信已中断；不会为仍存活的引擎启动副本".into());
        }
        if pending.len() >= 64 || pending.contains_key(&id) {
            return Err("共享 Xberg 请求队列已满或 ID 重复".into());
        }
        let lane = match request["command"].as_str() {
            Some("extract" | "transcribe") => Some("document"),
            Some("ocr_snapshot") => Some("snapshot"),
            _ => None,
        };
        if lane.is_some() && pending.values().any(|entry| entry.lane == lane) {
            return Err("该场景的上一个请求尚未确认结束，请等待其终态；其他场景可继续使用".into());
        }
        pending.insert(id.clone(), PendingRequest { sender: send, lane });
        if self.outgoing.try_send(request).is_err() {
            pending.remove(&id);
            return Err("共享 Xberg 请求队列不可用".into());
        }
        Ok(receive)
    }
}

/// 升级期间旧截图服务可能仍持有独占引擎。不能接管未知 stdio，也不能另开副本。
fn reject_existing_engine() -> Result<(), String> {
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    // SAFETY: 只读获取进程目录快照，不访问进程内容。
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err("无法检查已有 Xberg 进程，未启动新引擎".into());
    }
    // SAFETY: 快照句柄所有权转移给 File，析构仅调用 CloseHandle。
    let snapshot = unsafe { File::from_raw_handle(snapshot.cast()) };
    // SAFETY: PROCESSENTRY32W 是 C POD 结构，dwSize 按 API 要求填写。
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = u32::try_from(std::mem::size_of_val(&entry)).map_err(|e| e.to_string())?;
    let mut session = 0;
    // SAFETY: 有效进程 ID 与可写的 session 指针。
    let pid = unsafe { GetCurrentProcessId() };
    // SAFETY: session 是有效输出指针，pid 是当前进程。
    if unsafe { ProcessIdToSessionId(pid, &raw mut session) } == 0 {
        return Err("无法确认当前登录会话".into());
    }
    // SAFETY: 有效快照与已初始化长度的输出结构。
    let mut found = unsafe { Process32FirstW(snapshot.as_raw_handle().cast(), &raw mut entry) };
    while found != 0 {
        let length = entry
            .szExeFile
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(entry.szExeFile.len());
        if String::from_utf16_lossy(&entry.szExeFile[..length]).eq_ignore_ascii_case("xberg.exe") {
            let mut other_session = u32::MAX;
            // SAFETY: 快照提供的进程 ID，输出指针有效；已退出进程的查询允许失败。
            if unsafe { ProcessIdToSessionId(entry.th32ProcessID, &raw mut other_session) } != 0
                && other_session == session
            {
                return Err(format!("当前会话已有 Xberg（PID {}），无法接管其通信；请先结束旧实例。未启动第二个引擎", entry.th32ProcessID));
            }
        }
        // SAFETY: 同一有效快照及输出结构，逐项只读枚举。
        found = unsafe { Process32NextW(snapshot.as_raw_handle().cast(), &raw mut entry) };
    }
    Ok(())
}

fn fail_pending(pending: &Pending, broken: &AtomicBool) {
    broken.store(true, Ordering::Release);
    if let Ok(mut entries) = pending.lock() {
        for (id, send) in entries.drain() {
            let _ = send.sender.send(json!({"id":id,"ok":false,"error_kind":"process_exited","error":"共享 Xberg 通信中断或进程退出"}));
        }
    }
}

// Job 只由代理拥有，GUI/截图服务退出不会关闭此句柄。
struct Job(windows_sys::Win32::Foundation::HANDLE);
// SAFETY: 句柄只在 Engine 的互斥锁内转移/访问；Drop 只执行一次 CloseHandle。
unsafe impl Send for Job {}
impl Job {
    fn attach(child: &Child) -> Result<Self, String> {
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        // SAFETY: 创建无继承的未命名 job，不传外部指针。
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err("无法创建共享引擎进程组".into());
        }
        let job = Self(handle);
        // SAFETY: C POD 结构允许全零初始化。
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: 有效 job 及同步读取的配置结构。
        if unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                u32::try_from(std::mem::size_of_val(&limits)).map_err(|e| e.to_string())?,
            )
        } == 0
        {
            return Err("无法配置共享引擎进程组".into());
        }
        // SAFETY: child 为当前代理创建的有效进程，job 由本代理持有。
        if unsafe { AssignProcessToJobObject(handle, child.as_raw_handle().cast()) } == 0 {
            return Err("无法将共享引擎加入进程组".into());
        }
        Ok(job)
    }
}
impl Drop for Job {
    fn drop(&mut self) {
        /* SAFETY: 独占有效 job 句柄。 */
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn create_pipe(name: &str) -> Result<File, String> {
    use windows_sys::Win32::Security::{
        Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW, SECURITY_ATTRIBUTES,
    };
    use windows_sys::Win32::System::Pipes::CreateNamedPipeW;
    let wide = |value: &str| value.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let name = wide(name);
    let sddl = wide("D:P(A;;GA;;;SY)(A;;GA;;;OW)");
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: 有效宽字符串和可写输出指针；descriptor 随后由 LocalFree 释放。
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &raw mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err("无法设置共享管道权限".into());
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>())
            .map_err(|e| e.to_string())?,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    // SAFETY: 属性与名字在调用期间存活；拒绝远程客户端，允许多个本地请求并发连接。
    let raw = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            3,
            8,
            64,
            65536,
            65536,
            0,
            &raw const attributes,
        )
    };
    // SAFETY: descriptor 为上方 API 分配的内存，管道创建已复制所需内容。
    unsafe {
        LocalFree(descriptor);
    }
    if raw == INVALID_HANDLE_VALUE {
        return Err(format!(
            "创建共享管道失败：{}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: 将新建管道句柄唯一所有权交给 File。
    Ok(unsafe { File::from_raw_handle(raw.cast()) })
}

fn handle(pipe: File, engine: &Mutex<Option<Engine>>, stopping: &AtomicBool) -> Result<(), String> {
    let mut reader = BufReader::new(pipe);
    let envelope = {
        let _guard = IoDeadline::new(QUERY_TIMEOUT)?;
        read_json(&mut reader)?
    };
    let request = envelope["request"].clone();
    if matches!(
        request["command"].as_str(),
        Some("broker-state" | "broker-stop" | "broker-force-stop")
    ) {
        if request["command"] == "broker-stop" {
            stopping.store(true, Ordering::Release);
        }
        let mut slot = engine.lock().map_err(|_| "后台状态异常")?;
        if request["command"] == "broker-force-stop" {
            let document_active = slot.as_ref().is_some_and(|e| {
                e.pending
                    .lock()
                    .map_or(true, |p| p.values().any(|v| v.lane == Some("document")))
            });
            if document_active {
                return write_json(
                    reader.get_mut(),
                    &json!({"id":request["id"],"ok":false,"error":"文档或媒体任务尚未结束，不能强制退出","jchtools_broker_protocol":BROKER_PROTOCOL}),
                );
            }
            stopping.store(true, Ordering::Release);
            if let Some(mut current) = slot.take() {
                current.child.kill().map_err(|e| e.to_string())?;
                current.child.wait().map_err(|e| e.to_string())?;
                fail_pending(&current.pending, &current.broken);
            }
        }
        let active = slot
            .as_ref()
            .and_then(|e| {
                e.pending
                    .lock()
                    .ok()
                    .map(|p| p.values().filter(|v| v.lane.is_some()).count())
            })
            .unwrap_or(0);
        let response = json!({"id":request["id"],"ok":true,"running":true,"active":active,"stopping":stopping.load(Ordering::Acquire),"jchtools_broker_protocol":BROKER_PROTOCOL});
        return write_json(reader.get_mut(), &response);
    }
    let root = PathBuf::from(envelope["runtime_dir"].as_str().ok_or("缺少 Xberg 目录")?);
    let response = (|| {
        let mut slot = engine.lock().map_err(|_| "共享引擎状态异常")?;
        if stopping.load(Ordering::Acquire) && request["command"] != "cancel" {
            return Err("后台正在退出，不接受新请求".into());
        }
        if let Some(current) = slot.as_mut() {
            if current
                .child
                .try_wait()
                .map_err(|e| e.to_string())?
                .is_some()
            {
                slot.take();
            }
        }
        if let Some(current) = slot.as_mut() {
            if current.root != root {
                let saved = std::fs::canonicalize(xberg_settings::required()?)
                    .map_err(|e| e.to_string())?;
                if saved != root {
                    return Err("请求使用旧目录，请读取最新共享配置".into());
                }
                if current
                    .pending
                    .lock()
                    .map_err(|_| "共享请求表异常")?
                    .values()
                    .any(|p| p.lane.is_some())
                {
                    return Err("新来源已保存，等待当前任务安全结束后切换".into());
                }
                current.child.kill().map_err(|e| e.to_string())?;
                current.child.wait().map_err(|e| e.to_string())?;
                slot.take();
            }
        }
        if slot.is_none() {
            let saved = std::fs::canonicalize(xberg_settings::required()?)
                .map_err(|e| format!("已保存的 Xberg 目录不可读：{e}"))?;
            if saved != root {
                return Err("请求目录与 SQLite 保存的共享 Xberg 目录不一致".into());
            }
            *slot = Some(Engine::spawn(&saved)?);
        }
        let current = slot.as_ref().ok_or("共享引擎初始化失败")?;
        if current.root != root {
            return Err("Xberg 新目录已保存；常驻引擎仍使用原目录。请从托盘退出后台并重新打开 JchTools 后使用新目录，不会中断当前任务或启动第二个引擎".into());
        }
        if request["command"] == "keepalive" {
            return Ok(
                json!({"id":request["id"],"ok":true,"jchtools_xberg_pid":current.child.id()}),
            );
        }
        let receive = current.submit(request.clone())?;
        let timeout =
            Duration::from_millis(request["timeout_ms"].as_u64().unwrap_or(15_000)) + QUERY_TIMEOUT;
        let pid = current.child.id();
        drop(slot); // 等待响应时不持有引擎锁；截图、取消和查询可立即提交。
        let result = receive
            .recv_timeout(timeout)
            .map_err(|_| "共享 Xberg 未返回请求终态；未终止其他任务".to_string());
        // 超时/客户端离开不删除在途项；只有引擎终态或通信失效才能解除场景占用。
        result.map(|mut response| {
            response["jchtools_xberg_pid"] = json!(pid);
            response["jchtools_broker_pid"] = json!(std::process::id());
            response
        })
    })();
    let mut response = response.unwrap_or_else(|error: String| json!({"id":request["id"],"ok":false,"error_kind":"shared_runtime","error":error}));
    response["jchtools_broker_protocol"] = json!(BROKER_PROTOCOL);
    response["jchtools_broker_pid"] = json!(std::process::id());
    let _guard = IoDeadline::new(QUERY_TIMEOUT)?;
    write_json(reader.get_mut(), &response)?;
    // SAFETY: 有效管道，等待客户端取走响应后断开；受上方 I/O 期限保护。
    unsafe {
        windows_sys::Win32::Storage::FileSystem::FlushFileBuffers(
            reader.get_ref().as_raw_handle().cast(),
        );
    }
    Ok(())
}

/// 代理是内部常驻进程，只经命名管道通信，不使用任何继承来的标准句柄。
/// Windows 上 Rust std 即使把本进程 stdio 全部置为 null，CreateProcess 仍会沿
/// 可继承句柄链把祖父进程（cargo / PowerShell 的 `& cmd 2>&1` 捕获）的匿名管道
/// 写端带进本进程；代理常驻会让上层捕获永远等不到 EOF，测试与验收管线悬挂
/// （回归：tests/xberg_shared_process.rs broker_does_not_hold_client_capture_pipes）。
/// 此处仅在 serve() 最前执行：本进程尚未创建任何自有管道，std 句柄是 NUL
/// 字符设备，能命中的只有继承来的管道。
fn close_inherited_pipes() {
    use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE};
    use windows_sys::Win32::Storage::FileSystem::{GetFileType, FILE_TYPE_PIPE};
    for value in (4usize..65536).step_by(4) {
        // SAFETY: handle 只是本进程句柄表的探测值，转换本身无副作用。
        let handle: HANDLE = value as HANDLE;
        let mut flags = 0u32;
        // SAFETY: 探测本进程句柄表中的值；无效值由返回 0 过滤，flags 是有效输出指针。
        let known = unsafe { GetHandleInformation(handle, &raw mut flags) };
        if known == 0 {
            continue;
        }
        // SAFETY: handle 已被 GetHandleInformation 确认有效；GetFileType 只读句柄类型。
        let file_type = unsafe { GetFileType(handle) };
        if file_type != FILE_TYPE_PIPE {
            continue;
        }
        // SAFETY: 关闭本进程内确认有效的继承管道句柄；此后不再有任何引用。
        let closed = unsafe { CloseHandle(handle) };
        debug_assert_ne!(closed, 0);
    }
}

pub(super) fn serve() -> Result<(), String> {
    close_inherited_pipes();
    let root = xberg_settings::state_dir()?;
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(format!("xberg-{}.lock", identity()?)))
        .map_err(|e| e.to_string())?;
    match lock.try_lock_exclusive() {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
        Err(e) => return Err(format!("共享引擎锁失败：{e}")),
    }
    let name = pipe_name()?;
    background_allowed()?;
    let engine: Arc<Mutex<Option<Engine>>> = Arc::new(Mutex::new(None));
    let stopping = Arc::new(AtomicBool::new(false));
    let watch_engine = engine.clone();
    let watch_stopping = stopping.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(100));
        if !watch_stopping.load(Ordering::Acquire) {
            continue;
        }
        if let Ok(mut slot) = watch_engine.lock() {
            let active = slot.as_ref().is_some_and(|e| {
                e.pending
                    .lock()
                    .map_or(true, |p| p.values().any(|v| v.lane.is_some()))
            });
            if active {
                continue;
            }
            if let Some(mut current) = slot.take() {
                let _ = current.child.kill();
                let _ = current.child.wait();
            }
            std::process::exit(0);
        }
    });
    loop {
        let pipe = create_pipe(&name)?;
        // SAFETY: 同步管道的有效句柄；客户端先连接的 ERROR_PIPE_CONNECTED 也有效。
        let connected = unsafe {
            windows_sys::Win32::System::Pipes::ConnectNamedPipe(
                pipe.as_raw_handle().cast(),
                std::ptr::null_mut(),
            )
        };
        // SAFETY: 读取本线程紧邻 API 调用的错误码。
        if connected == 0 && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
            continue;
        }
        let engine = Arc::clone(&engine);
        let stopping = stopping.clone();
        std::thread::spawn(move || {
            let _ = handle(pipe, &engine, &stopping);
        });
    }
}
