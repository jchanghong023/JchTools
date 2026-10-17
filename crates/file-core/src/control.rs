use anyhow::{bail, Result};
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
    mpsc, Arc, Condvar, Mutex,
};
use std::time::{Duration, Instant};

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
    /// C-01 勾选落库后的依赖重算结果；失败时禁止直接执行旧计划。
    PlanRecomputed(std::path::PathBuf, Result<usize, String>),
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
    /// MD 整理：输出/分片冲突需要用户确认覆盖（M-07/M-11）。文本为确认框文案；
    /// 待执行的操作保存在界面 State 的挂起槽，确认后由界面重新发起执行。
    MdNeedsConfirm(String),
    /// MD 整理：合并或拆分完成（文案含统计）。
    MdDone(String),
    /// Git 工具：任务收尾（完成/停止/冲突/失败的最终文案，G-13 总体状态）。
    GitDone(String),
}
const LOG_EVENT_BUDGET: usize = 300;
const OTHER_EVENT_BUDGET: usize = 256;

struct EventQueue {
    events: VecDeque<Event>,
    logs: usize,
    others: usize,
    senders: usize,
    receiving: bool,
}

struct SharedEvents {
    queue: Mutex<EventQueue>,
    changed: Condvar,
}

/// 任务事件的有界发送端：日志保留最新 300 条，其他事件独立占用 256 个槽位。
/// 所有事件共用 FIFO，避免末尾日志与可靠终态因分开排空而倒序。
/// 成功入队保留原事件，不额外分配；仅在队列满或接收端断开时装箱归还错误事件。
pub struct EventSender {
    shared: Arc<SharedEvents>,
}

pub struct EventReceiver {
    shared: Arc<SharedEvents>,
}

pub fn event_channel() -> (EventSender, EventReceiver) {
    let shared = Arc::new(SharedEvents {
        queue: Mutex::new(EventQueue {
            events: VecDeque::with_capacity(LOG_EVENT_BUDGET + OTHER_EVENT_BUDGET),
            logs: 0,
            others: 0,
            senders: 1,
            receiving: true,
        }),
        changed: Condvar::new(),
    });
    (
        EventSender {
            shared: shared.clone(),
        },
        EventReceiver { shared },
    )
}

impl Clone for EventSender {
    fn clone(&self) -> Self {
        self.shared
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .senders += 1;
        Self {
            shared: self.shared.clone(),
        }
    }
}

impl Drop for EventSender {
    fn drop(&mut self) {
        let mut queue = self
            .shared
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.senders -= 1;
        if queue.senders == 0 {
            self.shared.changed.notify_all();
        }
    }
}

impl EventSender {
    pub fn send(&self, event: Event) -> Result<(), mpsc::SendError<Box<Event>>> {
        match self.enqueue(event, true) {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Disconnected(event)) => Err(mpsc::SendError(event)),
            Err(mpsc::TrySendError::Full(_)) => unreachable!("可靠事件的阻塞发送不会返回队列满"),
        }
    }

    pub fn try_send(&self, event: Event) -> Result<(), mpsc::TrySendError<Box<Event>>> {
        self.enqueue(event, false)
    }

    fn enqueue(&self, event: Event, blocking: bool) -> Result<(), mpsc::TrySendError<Box<Event>>> {
        let mut queue = self
            .shared
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if !queue.receiving {
                return Err(mpsc::TrySendError::Disconnected(Box::new(event)));
            }
            if matches!(&event, Event::Log(_)) {
                if queue.logs == LOG_EVENT_BUDGET {
                    if let Some(index) = queue
                        .events
                        .iter()
                        .position(|item| matches!(item, Event::Log(_)))
                    {
                        queue.events.remove(index);
                        queue.logs -= 1;
                    } else {
                        queue.logs = 0;
                    }
                }
                queue.logs += 1;
                break;
            }
            if queue.others < OTHER_EVENT_BUDGET {
                queue.others += 1;
                break;
            }
            // 状态可淘汰，可靠事件绝不淘汰；日志预算不受可靠事件拥塞影响。
            if let Some(index) = queue
                .events
                .iter()
                .position(|item| matches!(item, Event::Status(_)))
            {
                queue.events.remove(index);
                break;
            }
            if matches!(&event, Event::Status(_)) {
                return Ok(());
            }
            if !blocking {
                return Err(mpsc::TrySendError::Full(Box::new(event)));
            }
            queue = self
                .shared
                .changed
                .wait(queue)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        queue.events.push_back(event);
        self.shared.changed.notify_all();
        Ok(())
    }
}

impl EventQueue {
    fn pop(&mut self) -> Option<Event> {
        let event = self.events.pop_front()?;
        if matches!(&event, Event::Log(_)) {
            self.logs -= 1;
        } else {
            self.others -= 1;
        }
        Some(event)
    }
}

impl EventReceiver {
    pub fn try_recv(&self) -> Result<Event, mpsc::TryRecvError> {
        let mut queue = self
            .shared
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(event) = queue.pop() {
            self.shared.changed.notify_all();
            return Ok(event);
        }
        Err(if queue.senders == 0 {
            mpsc::TryRecvError::Disconnected
        } else {
            mpsc::TryRecvError::Empty
        })
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<Event, mpsc::RecvTimeoutError> {
        let started = Instant::now();
        let mut queue = self
            .shared
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if let Some(event) = queue.pop() {
                self.shared.changed.notify_all();
                return Ok(event);
            }
            if queue.senders == 0 {
                return Err(mpsc::RecvTimeoutError::Disconnected);
            }
            let Some(remaining) = timeout
                .checked_sub(started.elapsed())
                .filter(|value| !value.is_zero())
            else {
                return Err(mpsc::RecvTimeoutError::Timeout);
            };
            queue = self
                .shared
                .changed
                .wait_timeout(queue, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    pub fn try_iter(&self) -> EventTryIter<'_> {
        EventTryIter { receiver: self }
    }
}

pub struct EventTryIter<'a> {
    receiver: &'a EventReceiver,
}

impl Iterator for EventTryIter<'_> {
    type Item = Event;

    fn next(&mut self) -> Option<Event> {
        self.receiver.try_recv().ok()
    }
}

impl Drop for EventReceiver {
    fn drop(&mut self) {
        let mut queue = self
            .shared
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.receiving = false;
        queue.events.clear();
        queue.logs = 0;
        queue.others = 0;
        // GUI 已退出时，释放仍在等待可靠事件槽位的后台线程。
        self.shared.changed.notify_all();
    }
}

#[derive(Clone, Default)]
pub struct Context {
    pub control: Arc<Control>,
    pub events: Option<EventSender>,
}
impl Context {
    pub fn emit(&self, event: Event) {
        if let Some(sender) = &self.events {
            let _ = sender.send(event);
        }
    }
    pub fn status(&self, text: impl Into<String>) {
        self.emit(Event::Status(text.into()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // S-07 / U-10：GUI 尚未排空有界事件队列时，日志必须保留最新结果，且不能阻塞 worker。
    #[test]
    fn review_f07_full_event_queue_preserves_latest_logs_without_blocking() {
        const TOTAL: usize = 1024;
        let (sender, receiver) = event_channel();
        let context = Context {
            events: Some(sender),
            ..Context::default()
        };
        let (finished, completion) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            for index in 0..TOTAL {
                context.emit(Event::Log(format!("file-{index}: processing failed")));
            }
            finished.send(()).unwrap();
        });
        if let Err(error) = completion.recv_timeout(Duration::from_secs(5)) {
            // 先断开消费端再 join，防止错误地改成阻塞 send 后连测试本身也永久挂住。
            drop(receiver);
            worker.join().unwrap();
            panic!("GUI 暂未消费事件时，日志生产线程不得阻塞：{error}");
        }
        worker.join().unwrap();
        let logs: Vec<_> = receiver
            .try_iter()
            .map(|event| match event {
                Event::Log(text) => text,
                other => panic!("只发出日志却收到其他事件：{other:?}"),
            })
            .collect();
        assert!(!logs.is_empty(), "队列满时不能丢掉全部运行日志");
        assert!(logs.len() <= 300, "S-07 / U-10：日志缓冲必须有界");
        let expected: Vec<_> = (TOTAL - logs.len()..TOTAL)
            .map(|index| format!("file-{index}: processing failed"))
            .collect();
        assert_eq!(
            logs, expected,
            "排空后必须收到最新的连续日志，而不是队列首次填满时的旧日志"
        );
    }

    #[test]
    fn review_f07_retained_logs_stay_before_reliable_terminal_events() {
        let (sender, receiver) = event_channel();
        let path = std::path::PathBuf::from("task.db");
        sender
            .send(Event::Ready(path.clone(), Default::default()))
            .unwrap();
        for index in 0..1024 {
            sender.send(Event::Log(format!("log-{index}"))).unwrap();
        }
        sender.send(Event::Done(path, Default::default())).unwrap();
        sender.send(Event::Log("最后一条日志".into())).unwrap();
        sender.send(Event::Failed("收尾失败".into())).unwrap();
        drop(sender);

        assert!(matches!(receiver.try_recv().unwrap(), Event::Ready(..)));
        for index in 725..1024 {
            assert!(
                matches!(receiver.try_recv().unwrap(), Event::Log(text) if text == format!("log-{index}"))
            );
        }
        assert!(matches!(receiver.try_recv().unwrap(), Event::Done(..)));
        assert!(matches!(receiver.try_recv().unwrap(), Event::Log(text) if text == "最后一条日志"));
        assert!(matches!(receiver.try_recv().unwrap(), Event::Failed(text) if text == "收尾失败"));
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn review_f07_reliable_budget_does_not_take_latest_log_slots() {
        let (sender, receiver) = event_channel();
        for index in 0..256 {
            sender
                .try_send(Event::Notice(format!("notice-{index}")))
                .unwrap();
        }
        for index in 0..1024 {
            sender.try_send(Event::Log(format!("log-{index}"))).unwrap();
        }
        let pending = match sender.try_send(Event::Failed("必须送达".into())) {
            Err(mpsc::TrySendError::Full(event)) => *event,
            other => panic!("可靠队列满时必须交还原事件：{other:?}"),
        };
        assert!(matches!(receiver.try_recv().unwrap(), Event::Notice(text) if text == "notice-0"));
        sender.try_send(pending).unwrap();
        for index in 1..256 {
            assert!(
                matches!(receiver.try_recv().unwrap(), Event::Notice(text) if text == format!("notice-{index}"))
            );
        }
        for index in 724..1024 {
            assert!(
                matches!(receiver.try_recv().unwrap(), Event::Log(text) if text == format!("log-{index}"))
            );
        }
        assert!(matches!(receiver.try_recv().unwrap(), Event::Failed(text) if text == "必须送达"));
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn review_f07_status_yields_capacity_to_reliable_events() {
        let (sender, receiver) = event_channel();
        sender.send(Event::Status("旧状态".into())).unwrap();
        for index in 0..255 {
            sender
                .send(Event::Notice(format!("notice-{index}")))
                .unwrap();
        }
        sender.try_send(Event::Failed("不能丢弃".into())).unwrap();
        sender
            .try_send(Event::Status("允许丢弃的新状态".into()))
            .unwrap();
        for index in 0..255 {
            assert!(
                matches!(receiver.try_recv().unwrap(), Event::Notice(text) if text == format!("notice-{index}"))
            );
        }
        assert!(matches!(receiver.try_recv().unwrap(), Event::Failed(text) if text == "不能丢弃"));
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn review_f07_receiver_drop_releases_blocked_reliable_sender() {
        let (sender, receiver) = event_channel();
        for _ in 0..256 {
            sender.send(Event::Notice("已占用".into())).unwrap();
        }
        let (started, start) = mpsc::channel();
        let (finished, completion) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            started.send(()).unwrap();
            let result = sender.send(Event::Failed("等待中的终态".into()));
            finished.send(result).unwrap();
        });
        start.recv_timeout(Duration::from_secs(5)).unwrap();
        let early = completion.recv_timeout(Duration::from_millis(50));
        drop(receiver);
        let terminal = match early {
            Err(mpsc::RecvTimeoutError::Timeout) => completion.recv_timeout(Duration::from_secs(5)),
            other => {
                worker.join().unwrap();
                panic!("可靠事件在队列满且接收端仍存活时不得提前丢弃：{other:?}");
            }
        };
        // 超时先报错，不 join 一个已证明未被唤醒的线程，避免回归本身挂死。
        let terminal = terminal.expect("接收端断开后，阻塞发送者必须在有界时间内返回");
        worker.join().unwrap();
        assert!(
            matches!(terminal, Err(mpsc::SendError(event)) if matches!(event.as_ref(), Event::Failed(text) if text == "等待中的终态"))
        );
    }

    #[test]
    fn review_f07_consumption_releases_blocked_reliable_sender() {
        let (sender, receiver) = event_channel();
        for _ in 0..256 {
            sender.send(Event::Notice("已占用".into())).unwrap();
        }
        let (finished, completion) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            finished
                .send(sender.send(Event::Failed("最后终态".into())))
                .unwrap();
        });
        assert!(matches!(receiver.try_recv().unwrap(), Event::Notice(_)));
        let result = completion.recv_timeout(Duration::from_secs(5));
        if result.is_err() {
            drop(receiver);
            panic!("消费事件后必须唤醒等待槽位的发送者：{result:?}");
        }
        worker.join().unwrap();
        result.unwrap().unwrap();
        for _ in 1..256 {
            assert!(matches!(receiver.try_recv().unwrap(), Event::Notice(_)));
        }
        assert!(matches!(receiver.try_recv().unwrap(), Event::Failed(text) if text == "最后终态"));
        assert!(matches!(
            receiver.recv_timeout(Duration::ZERO),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn review_f07_disconnect_waits_for_last_sender_and_drains_events() {
        let (sender, receiver) = event_channel();
        let last = sender.clone();
        drop(sender);
        assert!(matches!(
            receiver.recv_timeout(Duration::ZERO),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        last.send(Event::Log("断开前的日志".into())).unwrap();
        drop(last);
        assert!(
            matches!(receiver.recv_timeout(Duration::ZERO).unwrap(), Event::Log(text) if text == "断开前的日志")
        );
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(1)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));

        let (sender, receiver) = event_channel();
        drop(receiver);
        assert!(
            matches!(sender.try_send(Event::Log("未送达".into())), Err(mpsc::TrySendError::Disconnected(event)) if matches!(event.as_ref(), Event::Log(text) if text == "未送达"))
        );
        assert!(
            matches!(sender.send(Event::Status("未送达状态".into())), Err(mpsc::SendError(event)) if matches!(event.as_ref(), Event::Status(text) if text == "未送达状态"))
        );
    }

    #[test]
    fn review_f07_last_sender_drop_wakes_waiting_receiver() {
        let (sender, receiver) = event_channel();
        let (started, start) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            started.send(()).unwrap();
            receiver.recv_timeout(Duration::from_secs(5))
        });
        start.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(sender);
        assert!(matches!(
            worker.join().unwrap(),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
    }
}
