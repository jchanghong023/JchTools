//! 界面事件泵的实时指标与进度刷新：refresh_runtime 按 RuntimeMode 分口径上屏（U-03/U-12/T-22/G-13）（自 ui_pump_events.rs 拆分）。
use super::{bytes, AppWindow, Ordering, RuntimeMode, UiPump};

impl UiPump {
    /// 实时指标与进度：只在真正运行时刷新，空闲时不碰 metrics（否则耗时一直涨）。
    pub(super) fn refresh_runtime(&self, ui: &AppWindow) {
        if !ui.get_busy() {
            return;
        }
        let s = self.state.borrow();
        let Some(control) = &s.control else {
            // 转 Markdown 任务无共享 Control（独立取消原子量）：共享进度条不得
            // 固定显示「准备中」——如实指向转 Markdown 页的实时进度（U-03/H-02）。
            if s.runtime == RuntimeMode::MarkdownConverter {
                ui.set_progress(-1.0);
                ui.set_progress_note("转换任务进行中，实时进度见转 Markdown 页".into());
                // T-22：运行期间实时显示当前文件与耗时（busy 门禁保证空闲不刷）；
                // 尚未进入单文件处理（扫描/文件间隙）时只报运行时长。
                let elapsed = s.started.elapsed().as_secs_f64().max(0.001);
                let activity = if s.convert_current.is_empty() {
                    format!("已运行 {elapsed:.1}s")
                } else {
                    format!("正在处理 {} · 耗时 {elapsed:.1}s", s.convert_current)
                };
                let counts = &s.convert_counts;
                ui.set_convert_metrics(
                    format!(
                        "{activity} · 成功 {} · 部分提取 {} · 失败 {} · 跳过 {}",
                        counts.success, counts.partial, counts.failed, counts.skipped
                    )
                    .into(),
                );
            } else {
                ui.set_progress(-1.0);
                ui.set_progress_note("准备中".into());
            }
            return;
        };
        // Git：从共享进度读取全部界面字段（G-13；不写整理流程计数器）。
        if s.runtime == RuntimeMode::Git {
            if let Some(shared) = &s.git_shared {
                if let Ok(branch) = shared.branch.lock() {
                    ui.set_git_branch(branch.as_str().into());
                }
                if let Ok(upstream) = shared.upstream.lock() {
                    ui.set_git_upstream(upstream.as_str().into());
                }
                if let Ok(current) = shared.current.lock() {
                    ui.set_git_current(current.as_str().into());
                }
                if let Ok(stage) = shared.stage.lock() {
                    ui.set_git_stage(stage.as_str().into());
                }
                if let Ok(state_text) = shared.state.lock() {
                    ui.set_git_state(state_text.as_str().into());
                }
                let retry = shared.retry.load(Ordering::Relaxed);
                ui.set_git_retry(i32::try_from(retry).unwrap_or(i32::MAX));
                ui.set_git_retry_wait(
                    i32::try_from(shared.retry_wait.load(Ordering::Relaxed)).unwrap_or(i32::MAX),
                );
                let done = shared.done.load(Ordering::Relaxed);
                let total = shared.total.load(Ordering::Relaxed);
                ui.set_git_done(i32::try_from(done).unwrap_or(i32::MAX));
                ui.set_git_total(i32::try_from(total).unwrap_or(i32::MAX));
                let elapsed = s.started.elapsed().as_secs_f64().max(0.001);
                if total > 0 {
                    // 进度分数为显示用途（Slint progress 即 f32），整数→浮点无受检 API。
                    // [quality-baseline approved 2026-10-03] 同类显示转换，经用户裁定保留
                    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
                    let progress = (done as f64 / total as f64).clamp(0.0, 1.0) as f32;
                    ui.set_progress(progress);
                    ui.set_progress_note(format!("{done} / {total}").into());
                } else {
                    ui.set_progress(-1.0);
                    ui.set_progress_note("".into());
                }
                ui.set_metrics(
                    format!("已完成 {done} / {total} · 重试 {retry} 次 · 耗时 {elapsed:.1}s")
                        .into(),
                );
            }
            return;
        }
        // MD：worker 经 control 上报已完成/总数（不写整理的读取量指标）。
        if s.runtime == RuntimeMode::Md {
            let done = control.completed.load(Ordering::Relaxed);
            let planned = control.planned().unwrap_or(0);
            let elapsed = s.started.elapsed().as_secs_f64().max(0.001);
            ui.set_metrics(format!("已处理 {done} / {planned} · 耗时 {elapsed:.1}s").into());
            if planned > 0 {
                // 进度分数为显示用途（Slint progress 即 f32），整数→浮点无受检 API。
                // [quality-baseline approved 2026-10-03] 同类显示转换，经用户裁定保留
                #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
                let progress = (done as f64 / planned as f64).clamp(0.0, 1.0) as f32;
                ui.set_progress(progress);
                ui.set_progress_note(format!("{done} / {planned}").into());
            } else {
                ui.set_progress(-1.0);
                ui.set_progress_note("".into());
            }
            return;
        }
        let read = control.read_bytes.load(Ordering::Relaxed);
        let elapsed = s.started.elapsed().as_secs_f64().max(0.001);
        let done = control.completed.load(Ordering::Relaxed);
        let scanned = control.scanned.load(Ordering::Relaxed);
        // 执行分母：worker 数出的「仍勾选且待执行」项数优先（U-03）；尚未数出时沿用摘要里的
        // 全量分母（旧口径是启动前同步数出，这里把这段等待挪到后台，界面先见到反馈）。
        let planned = control.planned().unwrap_or(s.planned);
        // 执行阶段不再扫描，写“扫描 0 个文件”只会让人误以为没扫到东西，这里只报执行进度。
        if s.applying {
            ui.set_metrics(
                format!("执行中：已处理 {done} / {planned} 项 · 耗时 {elapsed:.1}s").into(),
            );
        } else if s.extracting {
            // 解压不写整理流程的计数器（read_bytes/completed 只由哈希与计划执行累加），
            // 沿用整理口径会显示恒为 0 的「读取 / 平均读取 / 计划项」。这里只报解压
            // 自身真实可得的包进度与耗时（U-03：总量未知时不得编造数字）。
            ui.set_metrics(format!("解压中：已处理 {done} 包 · 耗时 {elapsed:.1}s").into());
        } else {
            // 吞吐速率为显示用途，u64→f64 的精度损失无意义。
            // [quality-baseline approved 2026-09-19] 显示用途转换，经用户裁定保留
            #[allow(clippy::cast_precision_loss)]
            let rate_mib_s = read as f64 / elapsed / 1_048_576.0;
            ui.set_metrics(format!("扫描 {} 个文件 · 读取 {} · 已处理 {} 个计划项 · 耗时 {:.1}s · 平均读取 {:.1} MiB/s",scanned,bytes(read),done,elapsed,rate_mib_s).into());
        }
        // 执行阶段按计划项计数；分析阶段总量未知（扫描/哈希/解压包大小不能提前预知）
        if s.applying && planned > 0 {
            // 进度分数为显示用途，整数→浮点的精度损失无意义。
            // [quality-baseline approved 2026-09-19] Slint progress 属性即 f32，整数→浮点无受检 API
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
            let progress = (done as f64 / planned as f64).clamp(0.0, 1.0) as f32;
            ui.set_progress(progress);
            ui.set_progress_note(format!("{done} / {planned} 项").into());
        } else if s.applying {
            // 执行阶段没有可执行的计划项（全部取消勾选或空计划）时，不要显示“已扫描 0 个文件”
            // 让人误以为没扫到东西。
            ui.set_progress(-1.0);
            ui.set_progress_note("没有待执行的计划项".into());
        } else if s.extracting {
            // 解压期间不显示整理口径的进度说明；已处理包数与耗时由上面的实时指标行给出。
            ui.set_progress(-1.0);
            ui.set_progress_note("".into());
        } else {
            ui.set_progress(-1.0);
            ui.set_progress_note(format!("已扫描 {scanned} 个文件 · 读取 {}", bytes(read)).into());
        }
    }
}
