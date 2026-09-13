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
    /// 冲突的目标文件（用户可见的最终路径）
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
    /// 计划页加载结果：path/actions/page + 请求代际 gen 与筛选 filter。
    /// 事件循环只应用「gen 仍是 latest 且 filter/page 与当前视图一致」的结果；
    /// 低代际晚到事件不得取走或清掉更高代际的 completed 缓存。
    PlanPage(std::path::PathBuf, Vec<crate::model::Action>, usize, u64, Option<String>),
    /// 第二项携带 (动作 id, 保存后的勾选值)，界面用它就地修正计划行，避免复选框与数据库不一致。
    SelectionSaved(std::path::PathBuf, Option<(i64, bool)>, Option<String>),
    /// 一次性提示（成功信息等），界面用中性样式展示
    Notice(String),
    /// 一次性错误（导出/计划加载失败等），界面用错误样式展示；不隐含代理/网络测试的 busy 语义
    Error(String),
    /// 计划页加载失败：带请求代际与筛选归属。过期请求（用户已切走筛选/翻页）的失败
    /// 不得把红条误报到当前正确视图上，UI 侧按 gen/filter 决定是否上屏。
    PlanLoadFailed(String,u64,Option<String>),
    /// 代理工具检测失败：只清 proxy_busy，不碰 net_test_busy
    ProxyFailed(String),
    /// 网络测试/WSL 列表失败：只清 net_test_busy，不碰 proxy_busy
    NetTestFailed(String),
    /// 代理工具检测结果快照（环境变量 / 系统代理 / 进程 / 网卡 / 本机与外网 IP / MAC）
    ProxySnapshot(crate::proxy::ProxySnapshot),
    /// 网络测试报告（ChatGPT / Google / GitHub 连通性，Windows 或 WSL2）
    NetTestReport(crate::nettest::NetTestReport),
    /// WSL 发行版列表：第二项为失败原因（无则 None）
    WslDistros(Vec<String>, Option<String>),
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
