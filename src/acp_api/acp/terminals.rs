//! AH-13：终端属于 session/handle；仅显式 kill/release 终止，Stop 等自然回收。
use agent_client_protocol::schema::v1::{
    CreateTerminalRequest, CreateTerminalResponse, KillTerminalRequest, KillTerminalResponse,
    ReleaseTerminalRequest, ReleaseTerminalResponse, SessionId, TerminalExitStatus, TerminalId,
    TerminalOutputRequest, TerminalOutputResponse, WaitForTerminalExitRequest,
    WaitForTerminalExitResponse,
};
use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex, PoisonError},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::{mpsc, oneshot, watch},
};

use super::callbacks::{invalid, io_error};

#[derive(Clone)]
pub(super) struct Terminals {
    inner: Arc<Mutex<Table>>,
    workspace: PathBuf,
}
#[derive(Default)]
struct Table {
    sealed: bool,
    entries: HashMap<TerminalId, Entry>,
}
#[derive(Clone)]
struct Entry {
    session: SessionId,
    control: mpsc::UnboundedSender<Control>,
    state: watch::Receiver<Snapshot>,
}
#[derive(Clone, Default)]
struct Snapshot {
    output: String,
    truncated: bool,
    exit: Option<TerminalExitStatus>,
    output_complete: bool,
    error: bool,
    reaped: bool,
}
enum Control {
    Kill(oneshot::Sender<Result<(), agent_client_protocol::Error>>),
    Release(oneshot::Sender<Result<(), agent_client_protocol::Error>>),
    Drain(oneshot::Sender<Result<(), agent_client_protocol::Error>>),
}

impl Terminals {
    pub(super) fn new(workspace: PathBuf) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Table::default())),
            workspace,
        }
    }
    pub(super) fn create(
        &self,
        request: CreateTerminalRequest,
    ) -> Result<CreateTerminalResponse, agent_client_protocol::Error> {
        // 创建和登记同属一个同步临界区，shutdown 不会漏掉已启动的进程。
        let mut table = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if table.sealed {
            return Err(invalid("终端服务已停止"));
        }
        let cwd = request.cwd.unwrap_or_else(|| self.workspace.clone());
        if !cwd.is_absolute() || request.command.is_empty() {
            return Err(invalid("终端程序或绝对工作目录无效"));
        }
        let mut command = Command::new(request.command);
        command
            .args(request.args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for variable in request.env {
            command.env(variable.name, variable.value);
        }
        #[cfg(windows)]
        command.creation_flags(0x0800_0000 | 0x0000_0004);
        let mut child = command.spawn().map_err(|_| io_error("终端进程启动失败"))?;
        let tree = match TerminalTree::attach_and_resume(&child) {
            Ok(tree) => Some(tree),
            Err(error) => {
                let _ = child.start_kill();
                return Err(error);
            }
        };
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            terminate_owned_tree(tree.as_ref(), &mut child, false)?;
            // 失败的创建没有交付 handle；只回收这次创建的自有子树。
            return Err(io_error("终端输出管道不可用"));
        };
        let id = TerminalId::new(uuid::Uuid::new_v4().to_string());
        let (control, mut commands) = mpsc::unbounded_channel();
        let (state, receiver) = watch::channel(Snapshot::default());
        table.entries.insert(
            id.clone(),
            Entry {
                session: request.session_id,
                control,
                state: receiver,
            },
        );
        drop(table);
        let limit = request.output_byte_limit;
        tokio::spawn(async move {
            let (chunks, mut output) = mpsc::channel(16);
            let out_reader = tokio::spawn(read_pipe(stdout, chunks.clone()));
            let err_reader = tokio::spawn(read_pipe(stderr, chunks.clone()));
            drop(chunks);
            let mut exited = false;
            let mut pipes_done = false;
            let mut exit_failed = false;
            let mut closing: Vec<oneshot::Sender<Result<(), agent_client_protocol::Error>>> =
                Vec::new();
            let mut kills: Vec<oneshot::Sender<Result<(), agent_client_protocol::Error>>> =
                Vec::new();
            let mut controls_closed = false;
            let mut drained: Vec<oneshot::Sender<Result<(), agent_client_protocol::Error>>> =
                Vec::new();
            let mut tree_done = false;
            loop {
                if exited && pipes_done && tree_done {
                    state.send_modify(|s| s.reaped = !exit_failed);
                    for reply in drained.drain(..) {
                        let _ = reply.send(if exit_failed {
                            Err(io_error("终端退出回收失败"))
                        } else {
                            Ok(())
                        });
                    }
                    for reply in kills.drain(..) {
                        let _ = reply.send(if exit_failed {
                            Err(io_error("终端退出回收失败"))
                        } else {
                            Ok(())
                        });
                    }
                    if !closing.is_empty() {
                        for reply in closing.drain(..) {
                            let _ = reply.send(if exit_failed {
                                Err(io_error("终端退出回收失败"))
                            } else {
                                Ok(())
                            });
                        }
                        break;
                    }
                    if controls_closed {
                        break;
                    }
                }
                tokio::select! {
                    result = wait_owned_tree(tree.as_ref()), if exited && !tree_done => {
                        tree_done = true;
                        if result.is_err() {
                            exit_failed = true;
                            state.send_modify(|s| s.error = true);
                        }
                    }
                    result = child.wait(), if !exited => {
                        exited = true;
                        exit_failed = result.is_err();
                        state.send_modify(|s| match result {
                            // Windows returns a DWORD through i32; retain its exact bit pattern.
                            Ok(status) => s.exit = Some(TerminalExitStatus::new().exit_code(status.code().map(i32::cast_unsigned))),
                            Err(_) => s.error = true,
                        });
                    }
                    chunk = output.recv(), if !pipes_done => match chunk {
                        Some(Ok(text)) => state.send_modify(|s| append_output(s, &text, limit)),
                        Some(Err(())) => state.send_modify(|s| s.error = true),
                        None => {
                            pipes_done = true;
                            state.send_modify(|s| s.output_complete = true);
                        },
                    },
                    command = commands.recv(), if !controls_closed => match command {
                        Some(Control::Kill(reply)) => {
                            if terminate_owned_tree(tree.as_ref(), &mut child, exited).is_err() { let _ = reply.send(Err(io_error("终端终止失败"))); }
                            else { kills.push(reply); }
                        }
                        Some(Control::Release(reply)) => {
                            if terminate_owned_tree(tree.as_ref(), &mut child, exited).is_err() { let _ = reply.send(Err(io_error("终端释放失败"))); }
                            else { closing.push(reply); }
                        }
                        Some(Control::Drain(reply)) => drained.push(reply),
                        // 服务退出/控制端丢失不是 Agent 的 terminal/kill。
                        None => controls_closed = true,
                    }
                }
            }
            let _ = out_reader.await;
            let _ = err_reader.await;
        });
        Ok(CreateTerminalResponse::new(id))
    }
    fn entry(
        &self,
        session: &SessionId,
        id: &TerminalId,
    ) -> Result<Entry, agent_client_protocol::Error> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entries
            .get(id)
            .filter(|entry| &entry.session == session)
            .cloned()
            .ok_or_else(|| invalid("终端不存在或不属于此会话"))
    }
    pub(super) fn output(
        &self,
        request: &TerminalOutputRequest,
    ) -> Result<TerminalOutputResponse, agent_client_protocol::Error> {
        let entry = self.entry(&request.session_id, &request.terminal_id)?;
        let state = entry.state.borrow();
        if state.error {
            return Err(io_error("终端输出或等待失败"));
        }
        Ok(
            TerminalOutputResponse::new(state.output.clone(), state.truncated)
                .exit_status(state.exit.clone()),
        )
    }
    pub(super) async fn wait(
        &self,
        request: WaitForTerminalExitRequest,
    ) -> Result<WaitForTerminalExitResponse, agent_client_protocol::Error> {
        let mut state = self.entry(&request.session_id, &request.terminal_id)?.state;
        loop {
            {
                let snapshot = state.borrow();
                if snapshot.error {
                    return Err(io_error("终端等待失败"));
                }
                // 进程退出通知可以早于最后一批 stdout/stderr；wait 后输出必须完整可读。
                if snapshot.output_complete {
                    if let Some(exit) = &snapshot.exit {
                        return Ok(WaitForTerminalExitResponse::new(exit.clone()));
                    }
                }
            }
            state
                .changed()
                .await
                .map_err(|_| io_error("终端状态通道已关闭"))?;
        }
    }
    pub(super) async fn kill(
        &self,
        request: KillTerminalRequest,
    ) -> Result<KillTerminalResponse, agent_client_protocol::Error> {
        let entry = self.entry(&request.session_id, &request.terminal_id)?;
        let (reply, response) = oneshot::channel();
        entry
            .control
            .send(Control::Kill(reply))
            .map_err(|_| io_error("终端控制通道已关闭"))?;
        response
            .await
            .map_err(|_| io_error("终端控制通道已关闭"))??;
        Ok(KillTerminalResponse::new())
    }
    pub(super) async fn release(
        &self,
        request: ReleaseTerminalRequest,
    ) -> Result<ReleaseTerminalResponse, agent_client_protocol::Error> {
        let entry = self.entry(&request.session_id, &request.terminal_id)?;
        Self::control(entry, false).await?;
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entries
            .remove(&request.terminal_id);
        Ok(ReleaseTerminalResponse::new())
    }
    async fn control(entry: Entry, drain: bool) -> Result<(), agent_client_protocol::Error> {
        let (reply, response) = oneshot::channel();
        let command = if drain {
            Control::Drain(reply)
        } else {
            Control::Release(reply)
        };
        if entry.control.send(command).is_err() {
            return if drain && entry.state.borrow().reaped {
                Ok(())
            } else {
                Err(io_error("终端释放通道已关闭"))
            };
        }
        match response.await {
            Ok(result) => result,
            Err(_) if drain && entry.state.borrow().reaped => Ok(()),
            Err(_) => Err(io_error("终端释放通道已关闭")),
        }
    }
    // 只封创建；保留表中的 session/handle，使 Agent 在等待期间仍能管理终端。
    pub(super) async fn shutdown(&self) -> Result<(), agent_client_protocol::Error> {
        let entries = {
            let mut table = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            table.sealed = true;
            table
                .entries
                .iter()
                .map(|(id, entry)| (id.clone(), entry.clone()))
                .collect::<Vec<_>>()
        };
        let mut result = Ok(());
        for (id, entry) in entries {
            match Self::control(entry, true).await {
                Ok(()) => {
                    self.inner
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .entries
                        .remove(&id);
                }
                Err(error) if result.is_ok() => result = Err(error),
                Err(_) => {}
            }
        }
        result
    }
}
fn append_output(snapshot: &mut Snapshot, text: &str, limit: Option<u64>) {
    snapshot.output.push_str(text);
    if let Some(limit) = limit {
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        if snapshot.output.len() > limit {
            let mut start = snapshot.output.len() - limit;
            while !snapshot.output.is_char_boundary(start) {
                start += 1;
            }
            snapshot.output.drain(..start);
            snapshot.truncated = true;
        }
    }
}
async fn read_pipe(mut pipe: impl AsyncRead + Unpin, sender: mpsc::Sender<Result<String, ()>>) {
    let mut buffer = [0u8; 8192];
    let mut pending = Vec::new();
    loop {
        let Ok(count) = pipe.read(&mut buffer).await else {
            let _ = sender.send(Err(())).await;
            return;
        };
        if count == 0 {
            if !pending.is_empty() {
                let _ = sender
                    .send(Ok(String::from_utf8_lossy(&pending).into_owned()))
                    .await;
            }
            return;
        }
        pending.extend_from_slice(&buffer[..count]);
        loop {
            match std::str::from_utf8(&pending) {
                Ok(text) => {
                    if sender.send(Ok(text.to_owned())).await.is_err() {
                        return;
                    }
                    pending.clear();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid > 0 {
                        let text = String::from_utf8_lossy(&pending[..valid]).into_owned();
                        if sender.send(Ok(text)).await.is_err() {
                            return;
                        }
                        pending.drain(..valid);
                    }
                    if let Some(length) = error.error_len() {
                        pending.drain(..length);
                        if sender.send(Ok("�".to_owned())).await.is_err() {
                            return;
                        }
                    } else {
                        break;
                    }
                }
            }
        }
    }
}

// 只把本回调创建的暂停进程加入独有 Job；恢复后派生的子树继承所有权。
// 不把 Agent 或用户自行启动的进程加入本终端 Job。
#[cfg(windows)]
struct TerminalTree(std::os::windows::io::OwnedHandle);
#[cfg(windows)]
impl TerminalTree {
    fn attach_and_resume(
        child: &tokio::process::Child,
    ) -> Result<Self, agent_client_protocol::Error> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::{
            Foundation::INVALID_HANDLE_VALUE,
            System::{
                Diagnostics::ToolHelp::{
                    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD,
                    THREADENTRY32,
                },
                JobObjects::{AssignProcessToJobObject, CreateJobObjectW},
                Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
            },
        };
        // SAFETY: Null attributes/name create a non-inheritable, unnamed Job.
        // A successful return is a new owned handle, transferred below exactly once.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io_error("终端进程组创建失败"));
        }
        // SAFETY: CreateJobObjectW returned a non-null newly owned handle.
        // This is its only ownership transfer; job keeps it live until drop.
        let job = Self(unsafe { OwnedHandle::from_raw_handle(handle.cast()) });
        // 不设置 KILL_ON_JOB_CLOSE：隐式关闭不是 Agent 授权的终止操作。
        // The new Child is still suspended and cannot yet spawn unowned descendants.
        let child_handle = child
            .raw_handle()
            .ok_or_else(|| io_error("终端进程句柄不可用"))?;
        // SAFETY: job owns the live Job handle and child borrows its live process
        // handle for this call; neither is closed or transferred by assignment.
        if unsafe { AssignProcessToJobObject(handle, child_handle.cast()) } == 0 {
            return Err(io_error("终端进程组关联失败"));
        }
        // 与既有 process.rs 相同：稳定 Rust 未公开主线程句柄，通过快照匹配此 PID。
        // SAFETY: Snapshot flags need no pointers; success creates an owned handle
        // that remains live through enumeration and is closed by OwnedHandle below.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io_error("暂停终端线程查询失败"));
        }
        // SAFETY: The snapshot is newly owned and is not INVALID_HANDLE_VALUE.
        // This is its only transfer into an owner responsible for closing it.
        let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot.cast()) };
        // SAFETY: THREADENTRY32 is a C structure of integers, all valid when zero.
        let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
        let entry_size = u32::try_from(std::mem::size_of::<THREADENTRY32>())
            .map_err(|_| io_error("暂停终端线程信息大小无效"))?;
        entry.dwSize = entry_size;
        // SAFETY: snapshot owns a live thread snapshot; entry is a writable
        // THREADENTRY32 with the SDK-required byte size and lives through the call.
        let mut present = unsafe { Thread32First(snapshot.as_raw_handle().cast(), &raw mut entry) };
        while present != 0 {
            if Some(entry.th32OwnerProcessID) == child.id() {
                // SAFETY: The enumerated thread belongs to this suspended Child;
                // access is limited to resume and the returned handle is not inherited.
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                if thread.is_null() {
                    return Err(io_error("暂停终端线程打开失败"));
                }
                // SAFETY: OpenThread returned a non-null newly owned handle.
                // This is its only transfer; thread closes it on every exit path.
                let thread = unsafe { OwnedHandle::from_raw_handle(thread.cast()) };
                // SAFETY: thread owns a live handle with THREAD_SUSPEND_RESUME
                // access, held throughout this call; resumption transfers no ownership.
                if unsafe { ResumeThread(thread.as_raw_handle().cast()) } == u32::MAX {
                    return Err(io_error("暂停终端线程恢复失败"));
                }
                return Ok(job);
            }
            entry.dwSize = entry_size;
            // SAFETY: The live snapshot and correctly sized writable entry remain
            // owned by this scope; enumeration neither retains pointers nor closes handles.
            present = unsafe { Thread32Next(snapshot.as_raw_handle().cast(), &raw mut entry) };
        }
        Err(io_error("暂停终端主线程不存在"))
    }
    fn terminate(&self) -> Result<(), agent_client_protocol::Error> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        // SAFETY: self owns a live handle to this terminal's exclusive Job for
        // the call's duration; termination neither closes it nor touches other Jobs.
        if unsafe { TerminateJobObject(self.0.as_raw_handle().cast(), 1) } == 0 {
            return Err(io_error("终端进程树终止失败"));
        }
        Ok(())
    }
    async fn wait_empty(&self) -> Result<(), agent_client_protocol::Error> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::{
            JobObjectBasicAccountingInformation, QueryInformationJobObject,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        };
        let info_size =
            u32::try_from(std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>())
                .map_err(|_| io_error("终端进程树信息大小无效"))?;
        loop {
            // SAFETY: This SDK accounting structure contains only integer fields;
            // zero is valid for every field.
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: self owns a live Job; info is an aligned writable buffer of
            // exactly info_size bytes for this information class, valid for the call.
            // The optional returned-size pointer is null; the API retains no pointers.
            if unsafe {
                QueryInformationJobObject(
                    self.0.as_raw_handle().cast(),
                    JobObjectBasicAccountingInformation,
                    (&raw mut info).cast(),
                    info_size,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                return Err(io_error("终端进程树回收查询失败"));
            }
            if info.ActiveProcesses == 0 {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

// 产品只支持 Windows；此分支只保持 Rust 源码中非 Windows 类型定义完整。
#[cfg(not(windows))]
struct TerminalTree;
#[cfg(not(windows))]
impl TerminalTree {
    fn attach_and_resume(_: &tokio::process::Child) -> Result<Self, agent_client_protocol::Error> {
        Ok(Self)
    }
}

fn terminate_owned_tree(
    tree: Option<&TerminalTree>,
    child: &mut tokio::process::Child,
    exited: bool,
) -> Result<(), agent_client_protocol::Error> {
    #[cfg(windows)]
    if let Some(tree) = tree {
        return tree.terminate();
    }
    #[cfg(not(windows))]
    let _ = tree;
    if !exited
        && child
            .try_wait()
            .map_err(|_| io_error("终端退出查询失败"))?
            .is_none()
    {
        child.start_kill().map_err(|_| io_error("终端终止失败"))?;
    }
    Ok(())
}

async fn wait_owned_tree(tree: Option<&TerminalTree>) -> Result<(), agent_client_protocol::Error> {
    #[cfg(windows)]
    if let Some(tree) = tree {
        return tree.wait_empty().await;
    }
    #[cfg(not(windows))]
    let _ = tree;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{append_output, Snapshot};
    // 覆盖 AH-13 / AH-A11：按字节裁前缀，但不切坏多字节字符。
    #[test]
    fn output_limit_preserves_utf8_and_tail() {
        let mut state = Snapshot::default();
        append_output(&mut state, "ab中文z", Some(5));
        assert_eq!(state.output, "文z");
        assert!(state.truncated);
        append_output(&mut state, "新", Some(0));
        assert_eq!(state.output, "");
    }
}
