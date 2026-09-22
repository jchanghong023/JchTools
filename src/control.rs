use anyhow::{bail, Result};
use std::sync::{
    atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
    mpsc, Arc, Condvar, Mutex,
};
use std::time::Duration;

pub struct Control {
    cancelled: AtomicBool,
    paused: AtomicBool,
    wake: Condvar,
    mutex: Mutex<()>,
    pub read_bytes: AtomicU64,
    pub scanned: AtomicU64,
    pub completed: AtomicU64,
    /// 执行阶段的进度分母（仍勾选且待执行的计划项数，U-03）。界面侧的 worker 在启动执行前
    /// 数出后写入：界面线程不为此打开任务库（H-02）。-1 表示尚未数出，界面沿用摘要里的全量分母。
    planned: AtomicI64,
}
impl Default for Control {
    fn default() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            wake: Condvar::new(),
            mutex: Mutex::new(()),
            read_bytes: AtomicU64::new(0),
            scanned: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            planned: AtomicI64::new(-1),
        }
    }
}
impl Control {
    /// 发布进度分母（勾选且待执行的计划项数）：由后台 worker 在启动执行前写入。
    pub fn set_planned(&self, planned: u64) {
        // 计数为显示用途：超出 i64 的极端值饱和显示，不阻断执行。
        let value = i64::try_from(planned).unwrap_or(i64::MAX);
        self.planned.store(value, Ordering::Release);
    }
    /// 读取进度分母；`None` 表示后台尚未数出。
    pub fn planned(&self) -> Option<u64> {
        let value = self.planned.load(Ordering::Acquire);
        (value >= 0).then(|| value.unsigned_abs())
    }
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
/// 任务库快照（计划页就绪判定与失败收尾摘要所需）：由界面侧 worker 线程一次开库读出，
/// 界面线程只拿纯数据与「当前配置/目录」比较——界面线程不再自己打开任务库或规范化路径
/// （大目录与网络盘上会卡住界面；H-02「不以阻塞界面换取后台吞吐」）。
#[derive(Debug, Clone)]
pub struct PlanSnapshot {
    /// 任务库 status；库打不开或字段缺失时为 None（界面按不可执行处理）
    pub status: Option<String>,
    /// 任务库内的配置快照，已剔除纯外观的 theme（界面侧同样剔除后再比较）
    pub config_json: Option<serde_json::Value>,
    /// 任务库记录的根目录，是否等于「发起请求时界面目录」的规范化路径
    pub root_matches: bool,
    /// 发起请求时界面目录的原文：界面据此判断快照是否仍对应当前输入
    pub requested_directory: String,
    /// 任务库摘要；只有失败收尾的请求读取（信息与计划页取自同一次开库）
    pub summary: Option<crate::model::Summary>,
}
#[derive(Debug)]
pub enum Event {
    Status(String),
    Log(String),
    Ready(std::path::PathBuf, crate::model::Summary),
    Done(std::path::PathBuf, crate::model::Summary),
    Failed(String),
    /// 计划页加载结果：path/actions/page + 页面请求代际 gen 与筛选 filter + 就绪快照
    /// 及其请求代际 state_gen（见 `PlanState`）。事件循环只应用「gen 仍是页面代际、
    /// filter 与当前视图一致」的页面；快照另按 state_gen 判定过期。
    PlanPage(
        std::path::PathBuf,
        Vec<crate::model::Action>,
        usize,
        u64,
        Option<String>,
        u64,
        Option<PlanSnapshot>,
    ),
    /// 只取就绪快照的结果（切工具、目录编辑、勾选保存后重算就绪，不重载计划页）：
    /// 第二项是快照请求代际，迟到的低代际快照不得改写当前就绪/可勾选状态；
    PlanState(std::path::PathBuf, u64, Option<PlanSnapshot>),
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
