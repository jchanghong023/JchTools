use std::sync::mpsc;
use std::time::Duration;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Threading::{GetCurrentThreadId, OpenThread, THREAD_TERMINATE};
use windows_sys::Win32::System::IO::CancelSynchronousIo;

/// 看门狗反复取消当前线程同步 I/O，覆盖期限到达恰在两次 I/O 之间的竞态。
pub(super) struct IoDeadline {
    done: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl IoDeadline {
    pub(super) fn new(timeout: Duration) -> Result<Self, String> {
        // SAFETY: 打开当前线程的真实句柄，供看门狗取消同步 I/O；不继承句柄。
        let id = unsafe { GetCurrentThreadId() };
        // SAFETY: 当前线程有效，不继承句柄。
        let handle = unsafe { OpenThread(THREAD_TERMINATE, 0, id) };
        if handle.is_null() {
            let error_code = std::io::Error::last_os_error().raw_os_error();
            tracing::warn!(
                event = "xberg_io_deadline_failed",
                stage = "thread_handle",
                error_code,
                "无法取得同步通信看门狗所需的线程句柄"
            );
            return Err("无法设置共享管道通信期限".into());
        }
        let raw = handle as usize;
        let (done, receiver) = mpsc::channel();
        let span = tracing::Span::current();
        let thread = std::thread::spawn(move || {
            let _entered = span.enter();
            if matches!(
                receiver.recv_timeout(timeout),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                tracing::warn!(
                    event = "xberg_io_deadline_reached",
                    stage = "synchronous_io",
                    timed_out = true,
                    timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                    "共享管道同步通信期限已到，开始取消阻塞调用"
                );
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
            if thread.join().is_err() {
                tracing::warn!(
                    event = "xberg_io_watchdog_thread_failed",
                    stage = "watchdog_join",
                    "共享管道通信看门狗线程异常退出"
                );
            }
        }
    }
}
