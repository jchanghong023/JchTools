//! 逐文件处理的共享状态、退避与公共上下文（G-09/G-13）。
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::Duration,
};

use jchtools_file_core::control::Control;

/// 生产退避基准（第 1 次失败等待 5 秒，之后逐次翻倍，第 5 次起固定 80 秒；测试注入更小值）。
pub const BACKOFF_UNIT: Duration = Duration::from_secs(5);

/// 界面共享进度（G-13）：worker 线程写、GUI 轮询读。
#[derive(Default)]
pub struct GitShared {
    pub repo: Mutex<String>,
    pub branch: Mutex<String>,
    pub upstream: Mutex<String>,
    pub total: AtomicU64,
    pub done: AtomicU64,
    pub current: Mutex<String>,
    pub stage: Mutex<String>,
    pub retry: AtomicU64,
    /// 下一次重试的剩余秒数（等待期间倒计时）
    pub retry_wait: AtomicU64,
    pub state: Mutex<String>,
}

impl GitShared {
    pub fn new() -> Self {
        Self {
            repo: Mutex::new(String::new()),
            branch: Mutex::new(String::new()),
            upstream: Mutex::new(String::new()),
            total: AtomicU64::new(0),
            done: AtomicU64::new(0),
            current: Mutex::new(String::new()),
            stage: Mutex::new("扫描".into()),
            retry: AtomicU64::new(0),
            retry_wait: AtomicU64::new(0),
            state: Mutex::new("未开始".into()),
        }
    }
    pub(super) fn set_stage(&self, stage: &str) {
        Self::set_text(&self.stage, stage);
        tracing::info!(event = "git_stage_changed", stage, "Git 任务进入阶段");
    }
    pub(super) fn set_state(&self, state: &str) {
        Self::set_text(&self.state, state);
        tracing::info!(event = "git_state_changed", state, "Git 任务状态已更新");
    }
    pub(super) fn set_current(&self, current: &str) {
        Self::set_text(&self.current, current);
    }
    /// 当前任务终态（供收尾日志读取；锁中毒时返回空串，不影响业务）。
    pub(super) fn state(&self) -> String {
        self.state.lock().map_or_else(
            |_| {
                tracing::error!(
                    event = "git_shared_read_failed",
                    stage = "terminal_state",
                    error_type = "lock_poisoned",
                    "Git 终态共享锁中毒，保持空状态回退"
                );
                String::new()
            },
            |guard| guard.clone(),
        )
    }
    pub(super) fn set_text(slot: &Mutex<String>, value: &str) {
        if let Ok(mut guard) = slot.lock() {
            guard.clear();
            guard.push_str(value);
        } else {
            tracing::error!(
                event = "git_shared_update_failed",
                error_type = "lock_poisoned",
                "Git 共享状态锁中毒，未能更新界面状态"
            );
        }
    }
}

/// 第 n 次重试的等待时长（G-09：5→10→20→40→80→80… 秒）。
/// `unit` 是第 1 次失败的等待基准（生产 [`BACKOFF_UNIT`] = 5 秒，封顶 80 秒 = 16×unit；
/// 测试注入更小基准等比缩放）。
pub fn backoff_duration(attempt: u64, unit: Duration) -> Duration {
    let factor = match attempt {
        0 | 1 => 1,
        2 => 2,
        3 => 4,
        4 => 8,
        _ => 16,
    };
    unit.saturating_mul(factor)
}

/// 可中断等待：100ms 粒度检查取消；按秒刷新剩余等待（G-13 的下一次重试等待时间）。
/// 返回 false 表示用户已取消。
fn cancellable_wait(control: &Control, wait: Duration, shared: &GitShared) -> bool {
    let step = Duration::from_millis(100);
    shared.retry_wait.store(
        wait.as_secs().max(u64::from(!wait.is_zero())),
        Ordering::Relaxed,
    );
    let mut elapsed = Duration::ZERO;
    while elapsed < wait {
        if control.is_cancelled() {
            shared.retry_wait.store(0, Ordering::Relaxed);
            return false;
        }
        let nap = step.min(wait.saturating_sub(elapsed));
        std::thread::sleep(nap);
        elapsed += nap;
        shared
            .retry_wait
            .store(wait.saturating_sub(elapsed).as_secs(), Ordering::Relaxed);
    }
    shared.retry_wait.store(0, Ordering::Relaxed);
    true
}

/// 单个文件变更的处理结果。
pub(super) enum StepOutcome {
    Done,
    Cancelled,
    Conflict(Vec<String>),
    Fatal(String),
}

/// 逐文件处理的公共上下文：任务控制、界面共享状态、日志回调与退避基准。
pub(super) struct Ctx<'a> {
    pub(super) control: &'a Control,
    pub(super) shared: &'a GitShared,
    pub(super) log: &'a dyn Fn(&str),
    pub(super) unit: Duration,
}

impl Ctx<'_> {
    /// 失败后按退避等待；返回 false 表示用户已取消（G-09/G-15）。
    pub(super) fn wait_retry(&self, attempt: &mut u64) -> bool {
        if self.control.is_cancelled() {
            return false;
        }
        *attempt += 1;
        self.shared.retry.store(*attempt, Ordering::Relaxed);
        self.shared.set_stage("等待重试");
        let wait = backoff_duration(*attempt, self.unit);
        let attempt = *attempt;
        tracing::warn!(
            event = "git_retry_wait_started",
            failed_attempt = attempt,
            next_attempt = attempt.saturating_add(1),
            wait_ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
            "Git 失败后进入退避等待"
        );
        let started = std::time::Instant::now();
        let secs = wait.as_secs_f64();
        (self.log)(&format!(
            "第 {attempt} 次失败：等待 {secs:.1} 秒后重试（G-09 无限重试，可随时停止）"
        ));
        let ok = cancellable_wait(self.control, wait, self.shared);
        if ok {
            self.shared.set_stage("重试中");
        }
        tracing::debug!(
            event = "git_retry_wait_completed",
            failed_attempt = attempt,
            result = if ok { "retry" } else { "cancelled" },
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "Git 退避等待结束"
        );
        ok
    }
}
