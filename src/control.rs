use crate::config::ConflictPolicy;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::sync::{atomic::{AtomicBool, AtomicU64, Ordering}, mpsc, Arc, Condvar, Mutex};
use std::time::Duration;

#[derive(Default)]
pub struct Control {
    cancelled: AtomicBool,
    paused: AtomicBool,
    wake: Condvar,
    mutex: Mutex<()>,
    pub read_bytes: AtomicU64,
    pub scanned: AtomicU64,
    pub completed: AtomicU64,
}
impl Control {
    pub fn cancel(&self) { self.cancelled.store(true, Ordering::Release); self.wake.notify_all(); }
    pub fn pause(&self, pause: bool) { self.paused.store(pause, Ordering::Release); self.wake.notify_all(); }
    pub fn is_cancelled(&self) -> bool { self.cancelled.load(Ordering::Acquire) }
    pub fn is_paused(&self) -> bool { self.paused.load(Ordering::Acquire) }
    pub fn check_cancelled(&self) -> Result<()> {
        if self.is_cancelled() { bail!("任务已取消；不会执行后续文件操作"); }
        Ok(())
    }
    pub fn checkpoint(&self) -> Result<()> {
        self.check_cancelled()?;
        let mut guard = self.mutex.lock().map_err(|_| anyhow::anyhow!("任务控制锁损坏"))?;
        while self.is_paused() {
            self.check_cancelled()?;
            guard = self.wake.wait_timeout(guard, Duration::from_millis(100))
                .map_err(|_| anyhow::anyhow!("任务控制锁损坏"))?.0;
        }
        self.check_cancelled()
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictInfo {
    pub incoming: String,
    pub existing: String,
    pub incoming_size: u64,
    pub existing_size: u64,
    pub incoming_time: i64,
    pub existing_time: i64,
}
#[derive(Debug, Clone)]
pub struct ConflictAnswer { pub policy: ConflictPolicy, pub apply_all: bool }
#[derive(Debug)]
pub enum Event {
    Status(String),
    Log(String),
    Conflict(ConflictInfo, mpsc::SyncSender<ConflictAnswer>),
    Ready(std::path::PathBuf, crate::model::Summary),
    Done(std::path::PathBuf, crate::model::Summary),
    Failed(String),
    PlanPage(std::path::PathBuf, Vec<crate::model::Action>, usize),
    SelectionSaved(std::path::PathBuf, Option<String>),
    History(Vec<(std::path::PathBuf, String)>),
    LoadedTask(std::path::PathBuf, String, crate::config::Config, crate::model::Summary, bool),
    ConfigLoaded(crate::config::Config),
    Notice(String),
}
#[derive(Clone)]
pub struct Context {
    pub control: Arc<Control>,
    pub events: Option<mpsc::SyncSender<Event>>,
    pub decisions: Arc<dyn Fn(ConflictInfo) -> Result<ConflictAnswer> + Send + Sync>,
}
impl Default for Context {
    fn default() -> Self {
        Self { control: Arc::new(Control::default()), events: None,
            decisions: Arc::new(|_| Ok(ConflictAnswer { policy: ConflictPolicy::KeepBoth, apply_all: false })) }
    }
}
impl Context {
    pub fn emit(&self, event: Event) {
        if let Some(sender) = &self.events {
            match event {
                Event::Log(_) | Event::Status(_) => { let _ = sender.try_send(event); }
                other => { let _ = sender.send(other); }
            }
        }
    }
    pub fn status(&self, text: impl Into<String>) { self.emit(Event::Status(text.into())); }
}
