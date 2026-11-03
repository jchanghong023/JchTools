use crate::{runner, storage, Snapshot};
use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, String>;

fn root() -> Result<PathBuf> {
    let path = jchtools_foundation::xberg_settings::state_dir()?;
    std::fs::create_dir_all(&path).map_err(|_| "无法创建定时任务状态目录")?;
    Ok(path)
}

fn lock_file(name: &str) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root()?.join(name))
        .map_err(|_| "无法打开定时任务后台锁".into())
}

fn contended(error: &std::io::Error) -> bool {
    error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
}

fn control_lock() -> Result<File> {
    let file = lock_file("scheduled-tasks-control.lock")?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(file),
            Err(error) if contended(&error) && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return Err("无法取得定时任务后台维护锁".into()),
        }
    }
}

pub(crate) fn status() -> Result<(bool, Option<u32>)> {
    let file = lock_file("scheduled-tasks-service.lock")?;
    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok((false, None)),
        Err(error) if contended(&error) => {
            let pid = std::fs::read_to_string(root()?.join("scheduled-tasks-service.pid"))
                .ok()
                .and_then(|text| text.parse::<u32>().ok())
                .filter(|id| *id != 0);
            Ok((true, pid))
        }
        Err(_) => Err("无法读取定时任务后台状态".into()),
    }
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("无法清理定时任务后台状态".into()),
    }
}
#[cfg(windows)]
fn protect_service_process() -> Result<()> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
        Threading::GetCurrentProcess,
    };
    static SERVICE_JOB: std::sync::OnceLock<OwnedHandle> = std::sync::OnceLock::new();
    // 子进程自创建起继承服务 Job，覆盖暂停创建到加入执行 Job 的间隙。
    // SAFETY: 两个空指针分别请求默认安全属性和匿名 Job。
    let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if handle.is_null() {
        return Err("无法创建定时任务后台进程组".into());
    }
    // SAFETY: 新建成功的句柄由 OwnedHandle 独占关闭。
    let job = unsafe { OwnedHandle::from_raw_handle(handle.cast()) };
    // SAFETY: Job 配置结构是可零初始化的 Windows POD。
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let size =
        u32::try_from(std::mem::size_of_val(&limits)).map_err(|_| "无法配置定时任务后台进程组")?;
    // SAFETY: 句柄、配置结构和长度均对应 Job 扩展限制信息。
    if unsafe {
        SetInformationJobObject(
            job.as_raw_handle().cast(),
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size,
        )
    } == 0
    {
        return Err("无法配置定时任务后台进程组".into());
    }
    // SAFETY: GetCurrentProcess 无参数，返回始终有效的当前进程伪句柄。
    let process = unsafe { GetCurrentProcess() };
    // SAFETY: 当前进程伪句柄有效，Job 已配置为关闭时终止进程树。
    if unsafe { AssignProcessToJobObject(job.as_raw_handle().cast(), process) } == 0 {
        return Err("无法加入定时任务后台进程组".into());
    }
    SERVICE_JOB
        .set(job)
        .map_err(|_| "定时任务后台进程组已登记")?;
    Ok(())
}

struct PendingService(Option<std::process::Child>);

impl Drop for PendingService {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl PendingService {
    fn confirmed(mut self) {
        drop(self.0.take());
    }
}

pub(crate) fn ensure_started(executable: PathBuf) -> Result<Snapshot> {
    let _control = control_lock()?;
    if !status()?.0 {
        let root = root()?;
        remove_if_present(&root.join("scheduled-tasks-service.stop"))?;
        remove_if_present(&root.join("scheduled-tasks-service.pid"))?;
        let mut command = Command::new(executable);
        command
            .arg("--scheduled-tasks-service")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let mut child = PendingService(Some(command.spawn().map_err(|_| "无法启动定时任务后台")?));
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let (running, pid) = status()?;
            if running && pid.is_some() {
                break;
            }
            if child
                .0
                .as_mut()
                .ok_or("无法查询定时任务后台启动状态")?
                .try_wait()
                .map_err(|_| "无法查询定时任务后台启动状态")?
                .is_some()
            {
                return Err("定时任务后台启动失败".into());
            }
            if Instant::now() >= deadline {
                return Err("等待定时任务后台启动超时".into());
            }
            thread::sleep(Duration::from_millis(50));
        }
        child.confirmed();
    }
    storage::list()
}

pub(crate) fn serve() -> Result<()> {
    let lock = lock_file("scheduled-tasks-service.lock")?;
    match FileExt::try_lock_exclusive(&lock) {
        Ok(()) => {}
        Err(error) if contended(&error) => return Ok(()),
        Err(_) => return Err("无法取得定时任务后台单实例锁".into()),
    }
    #[cfg(windows)]
    protect_service_process().inspect_err(|_| {
        tracing::error!(
            event = "scheduled_service_failed",
            stage = "process_job",
            category = "process",
            "定时任务后台进程组初始化失败"
        );
    })?;
    let root = root()?;
    remove_if_present(&root.join("scheduled-tasks-service.pid"))?;
    storage::recover()?;
    std::fs::write(
        root.join("scheduled-tasks-service.pid"),
        std::process::id().to_string(),
    )
    .map_err(|_| "无法发布定时任务后台进程身份")?;
    // The internal mode returns directly to main; retain the lock until process exit.
    static PROCESS_LOCK: std::sync::OnceLock<File> = std::sync::OnceLock::new();
    PROCESS_LOCK.set(lock).map_err(|_| "定时任务后台锁已登记")?;
    tracing::info!(
        event = "scheduled_service_started",
        category = "start",
        "定时任务后台开始运行"
    );
    let shutdown = Arc::new(AtomicBool::new(false));
    let mut workers: Vec<thread::JoinHandle<()>> = Vec::new();
    let mut last_poll_error: Option<Instant> = None;
    while !root.join("scheduled-tasks-service.stop").exists() {
        match storage::poll() {
            Ok(starts) => {
                for start in starts {
                    let shutdown = Arc::clone(&shutdown);
                    workers.push(thread::spawn(move || runner::execute(&start, &shutdown)));
                }
            }
            Err(_) => {
                if last_poll_error.is_none_or(|at| at.elapsed() >= Duration::from_secs(5)) {
                    tracing::warn!(
                        event = "scheduled_service_poll_failed",
                        stage = "poll",
                        category = "database",
                        "定时任务调度查询失败"
                    );
                    last_poll_error = Some(Instant::now());
                }
            }
        }
        let mut index = 0;
        while index < workers.len() {
            if workers[index].is_finished() {
                let worker = workers.swap_remove(index);
                let _ = worker.join();
            } else {
                index += 1;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    shutdown.store(true, Ordering::Relaxed);
    for worker in workers {
        let _ = worker.join();
    }
    tracing::info!(
        event = "scheduled_service_stopped",
        category = "stopped",
        "定时任务后台已停止"
    );
    Ok(())
}

#[cfg(windows)]
fn process_handle(pid: u32) -> Result<std::os::windows::io::OwnedHandle> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};
    // SAFETY: the PID is published by the lock-holding service; only synchronization is requested.
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return Err("无法打开定时任务后台退出等待句柄".into());
    }
    // SAFETY: OpenProcess returned a valid owned handle.
    Ok(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle.cast()) })
}

pub(crate) fn stop_service() -> Result<()> {
    let _control = control_lock()?;
    let root = root()?;
    let (running, pid) = status()?;
    if !running {
        return remove_if_present(&root.join("scheduled-tasks-service.stop"));
    }
    #[cfg(windows)]
    let process = match pid.and_then(|value| process_handle(value).ok()) {
        Some(process) => process,
        None if !status()?.0 => {
            return remove_if_present(&root.join("scheduled-tasks-service.stop"))
        }
        None => return Err("无法打开定时任务后台退出等待句柄".into()),
    };
    #[cfg(not(windows))]
    let _ = pid;
    std::fs::write(root.join("scheduled-tasks-service.stop"), b"stop")
        .map_err(|_| "无法发送定时任务后台停止请求")?;
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::{
            Foundation::WAIT_OBJECT_0, System::Threading::WaitForSingleObject,
        };
        // SAFETY: the owned synchronization handle remains valid for the entire wait.
        if unsafe { WaitForSingleObject(process.as_raw_handle().cast(), 30_000) } != WAIT_OBJECT_0 {
            return Err("等待定时任务后台退出超时".into());
        }
    }
    #[cfg(not(windows))]
    {
        let deadline = Instant::now() + Duration::from_secs(30);
        while status()?.0 {
            if Instant::now() >= deadline {
                return Err("等待定时任务后台退出超时".into());
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
    remove_if_present(&root.join("scheduled-tasks-service.stop"))
}
