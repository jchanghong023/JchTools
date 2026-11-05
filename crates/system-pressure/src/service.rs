use crate::{collector::Collector, output_path, window::Window, Snapshot};
use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const INTERVAL: Duration = Duration::from_secs(2);
const SERVICE_LOCK: &str = "system-pressure-service.lock";
const CONTROL_LOCK: &str = "system-pressure-control.lock";
const READY: &str = "system-pressure.ready";
const ERROR: &str = "system-pressure.error";
const STOP: &str = "system-pressure.stop";
type Result<T> = std::result::Result<T, String>;

fn root() -> Result<PathBuf> {
    let root = jchtools_foundation::xberg_settings::state_dir()?;
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("无法创建系统压力状态目录 {}：{error}", root.display()))?;
    Ok(root)
}

fn lock_file(root: &Path, name: &str) -> Result<File> {
    let path = root.join(name);
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| format!("无法打开系统压力锁 {}：{error}", path.display()))
}

fn contended(error: &std::io::Error) -> bool {
    error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
}

fn active(root: &Path) -> Result<bool> {
    let file = lock_file(root, SERVICE_LOCK)?;
    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(false),
        Err(error) if contended(&error) => Ok(true),
        Err(error) => Err(format!("无法查询系统压力后台锁：{error}")),
    }
}

fn read_if_present(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("无法读取系统压力状态 {}：{error}", path.display())),
    }
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("无法清理系统压力状态 {}：{error}", path.display())),
    }
}

fn publish(root: &Path, name: &str, text: &str) -> Result<()> {
    let pending = root.join(format!("{name}.pending"));
    std::fs::write(&pending, text)
        .and_then(|()| std::fs::rename(&pending, root.join(name)))
        .map_err(|error| {
            format!(
                "无法发布系统压力状态 {}：{error}",
                root.join(name).display()
            )
        })
}

fn snapshot(root: &Path) -> Result<Snapshot> {
    let error = read_if_present(&root.join(ERROR))?.filter(|text| !text.is_empty());
    let running = active(root)? && error.is_none() && read_if_present(&root.join(READY))?.is_some();
    Ok(Snapshot { running, error })
}

pub(crate) fn status() -> Result<Snapshot> {
    snapshot(&root()?)
}

fn control_lock(root: &Path) -> Result<File> {
    let file = lock_file(root, CONTROL_LOCK)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(file),
            Err(error) if contended(&error) && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(format!("无法取得系统压力启停锁：{error}")),
        }
    }
}

struct PendingService(Option<Child>);
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

pub(crate) fn start(executable: PathBuf) -> Result<Snapshot> {
    use std::os::windows::process::CommandExt;
    let root = root()?;
    let _control = control_lock(&root)?;
    let mut pending = PendingService(None);
    if !active(&root)? {
        for name in [READY, ERROR, STOP] {
            remove_if_present(&root.join(name))?;
        }
        let child = Command::new(executable)
            .arg("--system-pressure-service")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x0800_0000)
            .spawn()
            .map_err(|error| format!("无法启动系统压力后台：{error}"))?;
        pending.0 = Some(child);
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let current = snapshot(&root)?;
        if let Some(error) = &current.error {
            return Err(error.clone());
        }
        if current.running {
            pending.confirmed();
            return Ok(current);
        }
        if let Some(child) = pending.0.as_mut() {
            if child
                .try_wait()
                .map_err(|error| format!("无法查询系统压力后台启动结果：{error}"))?
                .is_some()
            {
                return Err(snapshot(&root)?
                    .error
                    .unwrap_or_else(|| "系统压力后台在就绪前退出".into()));
            }
        } else if !active(&root)? {
            return Err("系统压力后台在就绪前退出".into());
        }
        if Instant::now() >= deadline {
            return Err("等待系统压力后台首次写入超时".into());
        }
        thread::sleep(Duration::from_millis(50));
    }
}

pub(crate) fn serve() -> Result<()> {
    let root = root()?;
    let lock = lock_file(&root, SERVICE_LOCK)?;
    match FileExt::try_lock_exclusive(&lock) {
        Ok(()) => {}
        Err(error) if contended(&error) => return Ok(()),
        Err(error) => return Err(format!("无法取得系统压力后台单实例锁：{error}")),
    }
    remove_if_present(&root.join(READY))?;
    remove_if_present(&root.join(ERROR))?;
    let result = record(&root);
    // 先撤销就绪，再发布错误；持锁到停止采样、所有输出写入和状态清理完成。
    let cleanup = remove_if_present(&root.join(READY));
    let result = result.and(cleanup);
    if let Err(error) = &result {
        tracing::error!(event = "system_pressure_failed", error = %error, "系统压力记录已停止：{error}");
        publish(&root, ERROR, error)
            .map_err(|state_error| format!("{error}；无法保存后台错误：{state_error}"))?;
    } else {
        tracing::info!(
            event = "system_pressure_stopped",
            "系统压力记录已停止，保留已有CSV"
        );
    }
    drop(lock);
    result
}

fn record(root: &Path) -> Result<()> {
    let path = output_path();
    let mut collector = Collector::new();
    let mut window = Window::new();
    // 首次样本立即落盘：CPU和磁盘没有前一间隔时明确N/A，内存仍是真实即时值。
    let sample = collector.sample();
    let origin = Instant::now();
    window.record(Duration::ZERO, &sample, &path)?;
    publish(root, READY, &std::process::id().to_string())?;
    tracing::info!(event = "system_pressure_started", output = %path.display(), "系统压力记录已开始，每2秒更新最近20分钟数据");
    let mut next = origin + INTERVAL;
    let stop_path = root.join(STOP);
    loop {
        if stop_path
            .try_exists()
            .map_err(|error| format!("无法读取系统压力停止请求：{error}"))?
        {
            return Ok(());
        }
        let now = Instant::now();
        if now >= next {
            let sample = collector.sample();
            window.record(origin.elapsed(), &sample, &path)?;
            next += INTERVAL;
            // 写盘或系统调度超过周期时不突发补采伪造间隔，下一次仍等待真实2秒。
            if next <= Instant::now() {
                next = Instant::now() + INTERVAL;
            }
        } else {
            thread::sleep((next - now).min(Duration::from_millis(50)));
        }
    }
}

pub(crate) fn stop() -> Result<Snapshot> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::WAIT_OBJECT_0,
        System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
    };
    let root = root()?;
    let _control = control_lock(&root)?;
    if !active(&root)? {
        return snapshot(&root);
    }
    let process = read_if_present(&root.join(READY))?
        .and_then(|text| text.parse::<u32>().ok())
        .filter(|pid| *pid != 0)
        .and_then(|pid| {
            // SAFETY: 就绪状态来自持有单实例锁的后台；只申请同步等待权限，不终止进程。
            let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
            if handle.is_null() {
                None
            } else {
                // SAFETY: OpenProcess成功创建的句柄由OwnedHandle唯一持有并关闭。
                Some(unsafe { OwnedHandle::from_raw_handle(handle.cast()) })
            }
        });
    std::fs::write(root.join(STOP), b"stop")
        .map_err(|error| format!("无法发送系统压力停止请求：{error}"))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while active(&root)? {
        if Instant::now() >= deadline {
            return Err("等待系统压力后台停止超时".into());
        }
        thread::sleep(Duration::from_millis(50));
    }
    if let Some(process) = process {
        let remaining = u32::try_from(
            deadline
                .saturating_duration_since(Instant::now())
                .as_millis(),
        )
        .unwrap_or(0);
        // SAFETY: 持有真实进程句柄直到等待结束，避免PID复用；采样锁已确认释放。
        if unsafe { WaitForSingleObject(process.as_raw_handle().cast(), remaining) }
            != WAIT_OBJECT_0
        {
            return Err("等待系统压力后台进程退出超时".into());
        }
    }
    remove_if_present(&root.join(STOP))?;
    snapshot(&root)
}
