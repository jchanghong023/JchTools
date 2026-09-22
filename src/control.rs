use anyhow::{bail, Result};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc, Arc, Condvar, Mutex,
};
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
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.wake.notify_all();
    }
    pub fn pause(&self, pause: bool) {
        self.paused.store(pause, Ordering::Release);
        self.wake.notify_all();
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }
    pub fn check_cancelled(&self) -> Result<()> {
        if self.is_cancelled() {
            bail!("任务已取消；不会执行后续文件操作");
        }
        Ok(())
    }
    pub fn checkpoint(&self) -> Result<()> {
        self.check_cancelled()?;
        let mut guard = self
            .mutex
            .lock()
            .map_err(|_| anyhow::anyhow!("任务控制锁损坏"))?;
        while self.is_paused() {
            self.check_cancelled()?;
            guard = self
                .wake
                .wait_timeout(guard, Duration::from_millis(100))
                .map_err(|_| anyhow::anyhow!("任务控制锁损坏"))?
                .0;
        }
        self.check_cancelled()
    }
}
#[derive(Debug)]
pub enum Event {
    Status(String),
    Log(String),
    Ready(std::path::PathBuf, crate::model::Summary),
    Done(std::path::PathBuf, crate::model::Summary),
    Failed(String),
    /// 计划页加载结果：path/actions/page + 请求代际 gen 与筛选 filter。
    /// 事件循环只应用「gen 仍是 latest 且 filter/page 与当前视图一致」的结果；
    /// 低代际晚到事件不得取走或清掉更高代际的 completed 缓存。
    PlanPage(
        std::path::PathBuf,
        Vec<crate::model::Action>,
        usize,
        u64,
        Option<String>,
    ),
    /// 第二项携带 (动作 id, 保存后的勾选值)，界面用它就地修正计划行，避免复选框与数据库不一致。
    SelectionSaved(std::path::PathBuf, Option<(i64, bool)>, Option<String>),
    /// 一次性提示（成功信息等），界面用中性样式展示
    Notice(String),
    /// 一次性错误（导出/计划加载失败等），界面用错误样式展示；不隐含代理/网络测试的 busy 语义
    Error(String),
    /// 计划页加载失败：带请求代际与筛选归属。过期请求（用户已切走筛选/翻页）的失败
    /// 不得把红条误报到当前正确视图上，UI 侧按 gen/filter 决定是否上屏。
    PlanLoadFailed(String, u64, Option<String>),
    /// 「开始解压」确认框的压缩包清点结果（X-02）：Err 为清点失败原因。首项是请求
    /// 代际：用户返回检查后改目录再次发起清点时，迟到的低代际事件不得刷新文案或
    /// 解除 confirm-pending 门禁（与 PlanPage/PlanLoadFailed 的 gen 同一口径）。
    ExtractCount(u64, Result<u64, String>),
    /// 「递归解压」一段式运行结束（X-02）：不生成计划、无 ready 态，界面只收尾摘要。
    ExtractDone(std::path::PathBuf, crate::model::Summary),
}
#[derive(Clone, Default)]
pub struct Context {
    pub control: Arc<Control>,
    pub events: Option<mpsc::SyncSender<Event>>,
}
impl Context {
    pub fn emit(&self, event: Event) {
        if let Some(sender) = &self.events {
            match event {
                Event::Log(_) | Event::Status(_) => {
                    let _ = sender.try_send(event);
                }
                other => {
                    let _ = sender.send(other);
                }
            }
        }
    }
    pub fn status(&self, text: impl Into<String>) {
        self.emit(Event::Status(text.into()));
    }
}
