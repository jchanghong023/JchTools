//! 界面事件泵：结构定义、构造与轮询主流程及各消息应用方法（自 gui.rs 拆分）。
use super::{
    apply_acp_messages, apply_codeagent_messages, apply_idle_directory_messages, control,
    ensure_snap_supervisor, gui_activity_in_flight, launch_markdown_conversion, log_panel_text,
    log_panel_visible, push_event_log, request_fail_page, start_markdown_readiness,
    start_snap_readiness, system_dark, AppWindow, Cell, ConvertCounts, Duration, EventSender,
    FailChannelMsg, FailRow, Instant, Ordering, PathBuf, Rc, RefCell, RuntimeMode, SnapMessage,
    State, Summary, VecModel, FAIL_REFRESH_MIN,
};

/// 界面事件泵：排空事件通道、上屏日志、刷新实时指标与系统主题。
/// 由两条路径驱动，共用同一套排空/应用逻辑：
/// 1) 100ms 轮询——进度、耗时、主题等周期性刷新（绘制频率因此不超过 10Hz），
///    并作为唤醒不可用时的兜底排空（事件循环未运行/已退出）；
/// 2) 结果事件唤醒——worker 通过 `EventSender` 回传结果后立即唤醒事件循环
///    （见 `wake_event_loop`），结果上屏不再等最坏 100ms（H-02）。
pub(super) struct UiPump {
    pub(super) receiver: control::EventReceiver,
    pub(super) state: Rc<RefCell<State>>,
    /// 结果回传句柄：收尾与勾选保存后要重新发起计划页/就绪快照加载。
    pub(super) out: EventSender,
    /// 上次系统主题轮询：“跟随系统”需要感知运行期的系统深浅色切换；
    /// 轮询注册表成本极低，每 2 秒一次。开机不足 10 秒时减法会下溢 panic，构造时兜底。
    theme_poll: Cell<Instant>,
    /// 上次日志上屏时间：面板是 300 行整段重建，按时间预算限流（见 `LOG_REFRESH_MIN`）。
    log_refreshed: Cell<Instant>,
    /// 日志呈现是否落后于环形缓冲：面板不可见时只置脏（重建 300 行文本浪费），
    /// 打开后的下一次刷新按 state.logs 补齐，保持最新在上的 300 条记录。
    pub(super) log_dirty: Cell<bool>,
    /// 转换日志呈现是否落后于环形缓冲（S1-03）：CONVERTER_LOG 只置脏，由轮询
    /// 尾部在转换页可见时统一重建——一批 N 条日志只做一次整段重排；收尾与
    /// 校验失败事件仍直接上屏保证立即可见。
    pub(super) convert_log_dirty: Cell<bool>,
    /// 失败列表当前页的上次自动刷新时间：运行中低频节流刷新（U-10，见 refresh_fail_list）。
    fail_refreshed: Cell<Instant>,
}

/// 300 行可选中文本的整段重排开销显著：运行中日志合批到 2Hz，避免连续重排挤占输入处理。
/// 进度与取消仍走原来的 100ms 刷新；任务收尾日志不等待此间隔。
const LOG_REFRESH_MIN: Duration = Duration::from_millis(500);

impl UiPump {
    pub(super) fn new(
        receiver: control::EventReceiver,
        state: Rc<RefCell<State>>,
        out: EventSender,
    ) -> Self {
        Self {
            receiver,
            state,
            out,
            theme_poll: Cell::new(
                Instant::now()
                    .checked_sub(Duration::from_secs(10))
                    .unwrap_or_else(Instant::now),
            ),
            // 首次刷新立即可做：减去最小间隔而不是从现在开始计时。
            log_refreshed: Cell::new(
                Instant::now()
                    .checked_sub(LOG_REFRESH_MIN)
                    .unwrap_or_else(Instant::now),
            ),
            log_dirty: Cell::new(false),
            convert_log_dirty: Cell::new(false),
            fail_refreshed: Cell::new(
                Instant::now()
                    .checked_sub(FAIL_REFRESH_MIN)
                    .unwrap_or_else(Instant::now),
            ),
        }
    }
    /// 一次刷新：主题 → 排空事件 → 日志上屏 → 失败列表 → 实时指标。
    pub(super) fn run(&self, ui: &AppWindow) {
        self.state
            .borrow()
            .snap_foreground_busy
            .store(gui_activity_in_flight(ui), Ordering::Release);
        if self.theme_poll.get().elapsed() >= Duration::from_secs(2) {
            self.theme_poll.set(Instant::now());
            ui.set_system_dark(system_dark());
        }
        let closing = self.state.borrow().close_after;
        let terminal = self.drain(ui);
        // 所有工具终态及初始化/保存收尾共用空闲补查边界；仍忙时保留意图。
        // 刻意收窄：只等旧工具任务与转 Markdown 三阶段结束才补查就绪，
        // 不含 snap_initializing（截图初始化不阻塞转 Markdown 就绪）；
        // 与 gui_activity_in_flight 的五 getter 合取不同是故意的。
        if !ui.get_busy()
            && !ui.get_convert_initializing()
            && !ui.get_convert_preparing()
            && !ui.get_convert_runtime_saving()
            && !closing
            && !self.state.borrow().close_after
            && self.state.borrow().convert_readiness_missed
        {
            self.state.borrow_mut().convert_readiness_missed = false;
            start_markdown_readiness(ui, &self.state, &self.out);
        }
        // 隐藏时只置脏：日志面板没实例化就不重建 300 行文本；面板可见（含从隐藏切回来）
        // 的下一次刷新按环形缓冲上屏，保证打开时看到的是最新 300 条。
        // 整段重排按时间预算限流：结果事件唤醒只让结果尽快上屏/更新指标，不放大日志重排成本；
        // 收尾事件例外——失败原因与最终统计的日志必须立刻可见。
        if self.log_dirty.get()
            && log_panel_visible(ui)
            && (terminal || self.log_refreshed.get().elapsed() >= LOG_REFRESH_MIN)
        {
            self.log_refreshed.set(Instant::now());
            self.log_dirty.set(false);
            ui.set_log_text(log_panel_text(&self.state.borrow().logs).into());
        }
        // 转换日志同口径（S1-03）：面板只在转 Markdown 页实例化，离开页面时整段
        // 重建纯属浪费；置脏后在页面可见的下一次刷新按环形缓冲补齐。
        if self.convert_log_dirty.get() && ui.get_screen() == 5 {
            self.convert_log_dirty.set(false);
            ui.set_convert_log_text(log_panel_text(&self.state.borrow().convert_logs).into());
        }
        self.apply_fail_messages(ui);
        self.apply_snap_messages(ui);
        apply_acp_messages(ui, &self.state);
        apply_codeagent_messages(ui, &self.state);
        apply_idle_directory_messages(ui, &self.state.borrow());
        self.refresh_fail_list(ui);
        self.refresh_runtime(ui);
    }
    pub(super) fn apply_snap_messages(&self, ui: &AppWindow) {
        let messages: Vec<SnapMessage> = self
            .state
            .borrow()
            .snap_receiver
            .borrow_mut()
            .try_iter()
            .collect();
        for message in messages {
            match message {
                SnapMessage::Readiness(generation, result) => {
                    if self.state.borrow().snap_generation != generation {
                        continue;
                    }
                    match result {
                        Ok(()) => {
                            ui.set_snap_ready(true);
                            ui.set_snap_asset_status("组件已校验，可离线识别".into());
                            ui.set_snap_asset_detail("".into());
                        }
                        Err(error) => {
                            ui.set_snap_ready(false);
                            ui.set_snap_asset_status("组件未就绪，请前往设置修复".into());
                            ui.set_snap_asset_detail(error.into());
                        }
                    }
                }
                SnapMessage::Progress(generation, progress) => {
                    if self.state.borrow().snap_generation == generation {
                        ui.set_snap_progress(progress.into());
                    }
                }
                SnapMessage::Initialized(generation, result) => {
                    if self.state.borrow().snap_generation != generation {
                        continue;
                    }
                    self.state.borrow_mut().snap_init_cancel = None;
                    ui.set_snap_initializing(false);
                    match result {
                        Ok(()) => {
                            ui.set_snap_ready(true);
                            ui.set_snap_asset_status("组件初始化完成，可离线使用".into());
                            ui.set_snap_asset_detail("".into());
                            ui.set_snap_progress("".into());
                            ensure_snap_supervisor(&self.state, &self.out);
                        }
                        Err(error) => {
                            ui.set_snap_ready(false);
                            if error == "用户取消初始化" {
                                // U-06/S3-04：用户主动取消不是组件故障——中性状态行
                                // 提示即可，不写红色错误、不汇入「组件未就绪」详情。
                                ui.set_snap_asset_status("初始化已取消，可稍后前往设置重试".into());
                                ui.set_snap_asset_detail("".into());
                            } else {
                                ui.set_snap_asset_status("组件初始化未完成，请前往设置重试".into());
                                ui.set_snap_asset_detail(error.clone().into());
                                ui.set_snap_error(error.into());
                            }
                        }
                    }
                    if self.close_after_if_idle(ui) {
                        let _ = slint::quit_event_loop();
                    }
                }
                SnapMessage::Service(result, requested) => {
                    if requested {
                        ui.set_snap_request_pending(false);
                    }
                    match result {
                        Err(error) => {
                            ui.set_snap_connected(false);
                            if error.starts_with("后台已从托盘退出") {
                                ui.set_snap_service_status(error.into());
                                ui.set_snap_error("".into());
                            } else {
                                ui.set_snap_service_status("后台服务未连接，请查看设置页".into());
                                ui.set_snap_error(error.into());
                            }
                        }
                        Ok(value) if value["ok"] != true => {
                            ui.set_snap_connected(true);
                            ui.set_snap_error(
                                value["error"].as_str().unwrap_or("截图服务拒绝请求").into(),
                            );
                            // 请求失败不等于断线；保留之前读出的设置和当前生效热键。
                        }
                        Ok(value) => {
                            self.apply_snap_service_state(ui, &value, requested);
                        }
                    }
                }
            }
        }
    }

    /// watcher 与任务终态使用同一关联入口；迟到的同任务发现不重置当前页。
    pub(super) fn associate_extract_task(&self, ui: &AppWindow, path: PathBuf) -> bool {
        {
            let mut state = self.state.borrow_mut();
            if state.extract_task.as_ref() == Some(&path) && ui.get_fail_ready() {
                return false;
            }
            state.extract_task = Some(path);
            state.fail_page = 0;
            state.fail_page_starts = vec![0];
            state.fail_gen += 1;
        }
        ui.set_fail_ready(true);
        ui.set_fail_rows(Rc::new(VecModel::from(Vec::<FailRow>::new())).into());
        ui.set_fail_total(0);
        ui.set_fail_prev_enabled(false);
        ui.set_fail_next_enabled(false);
        ui.set_fail_page_label("加载中…".into());
        request_fail_page(&self.state, 0);
        true
    }

    /// 排空失败列表专用通道：任务目录发现与分页结果（U-10）。
    /// 迟到的结果按请求代际与当前解压任务整体丢弃，与计划页事件同一口径。
    pub(super) fn apply_fail_messages(&self, ui: &AppWindow) {
        let messages: Vec<FailChannelMsg> = self
            .state
            .borrow()
            .fail_receiver
            .borrow_mut()
            .try_iter()
            .collect();
        for message in messages {
            match message {
                FailChannelMsg::TaskFound(path, generation) => {
                    // 过期发现：用户已发起新的解压任务（清点代际已前进）或流程已结束。
                    if self.state.borrow().extract_generation != generation {
                        continue;
                    }
                    self.associate_extract_task(ui, path);
                }
                FailChannelMsg::Page(fail) => {
                    let accepted = {
                        let s = self.state.borrow();
                        s.extract_task.as_ref() == Some(&fail.task) && s.fail_gen == fail.generation
                    };
                    if !accepted {
                        continue;
                    }
                    if let Some(error) = fail.error {
                        ui.set_error_text(error.into());
                        // 取数失败按当前页恢复翻页按钮，避免永久中间态（与计划页同口径）。
                        let (page, page_count) = {
                            let s = self.state.borrow();
                            (s.fail_page, s.fail_page_starts.len())
                        };
                        ui.set_fail_prev_enabled(page > 0);
                        ui.set_fail_next_enabled(page + 1 < page_count);
                        continue;
                    }
                    {
                        let mut s = self.state.borrow_mut();
                        s.fail_page = fail.page;
                        if fail.has_more && s.fail_page_starts.len() <= fail.page + 1 {
                            s.fail_page_starts.push(fail.last_id);
                        }
                    }
                    let rows = fail
                        .items
                        .into_iter()
                        .map(|item| FailRow {
                            stage: item.stage.into(),
                            source: item.source.into(),
                            reason: item.reason.into(),
                            target: item.target.into(),
                        })
                        .collect::<Vec<_>>();
                    ui.set_fail_rows(Rc::new(VecModel::from(rows)).into());
                    // 总数为显示用途，超出 i32 的极端值饱和显示即可。
                    ui.set_fail_total(i32::try_from(fail.total).unwrap_or(i32::MAX));
                    ui.set_fail_prev_enabled(fail.page > 0);
                    ui.set_fail_next_enabled(fail.has_more);
                    ui.set_fail_page_label(
                        format!("第 {} 页 · 每页最多 100 项", fail.page + 1).into(),
                    );
                }
            }
        }
    }
    /// 运行中自动刷新失败列表当前页（U-10「运行中可查看已经发生的失败」）：
    /// 只在解压任务运行且失败列表面板可见时按低频节流发起；空闲与其他页面不取数。
    fn refresh_fail_list(&self, ui: &AppWindow) {
        if ui.get_screen() != 2 || ui.get_panel() != 2 || !ui.get_busy() {
            return;
        }
        let (extracting, has_task) = {
            let s = self.state.borrow();
            (s.runtime == RuntimeMode::Extract, s.extract_task.is_some())
        };
        if !extracting || !has_task || self.fail_refreshed.get().elapsed() < FAIL_REFRESH_MIN {
            return;
        }
        self.fail_refreshed.set(Instant::now());
        let page = self.state.borrow().fail_page;
        request_fail_page(&self.state, page);
    }
    /// 排空事件通道并应用事件；返回本轮是否处理过任务收尾事件（Ready/Done/Failed/ExtractDone），
    /// 供调用方决定是否立即上屏日志（收尾日志是例外，不受刷新预算限制）。
    /// 每次刷新最多处理 256 条：单次回调里的界面工作有上界，其余事件留给下一次处理。
    /// 独立借用下的日志上屏：写环形缓冲并标脏。仅限 borrow_mut 只服务日志的
    /// 调用点；与其他字段共用同一借用的收尾分支不得使用（会扩大借用存活范围）。
    pub(super) fn push_log(&self, text: String) {
        let mut s = self.state.borrow_mut();
        push_event_log(&mut s.logs, text);
        self.log_dirty.set(true);
    }

    /// SnapMessage::Service 的 Ok 分支：按服务状态快照上屏连接/模型/任务/热键/
    /// 自启动，退出广播时取消全部共享任务（`exiting` 只求值一次，纯读无副作用），
    /// 并按 C'-2 口径维护错误文本。
    fn apply_snap_service_state(&self, ui: &AppWindow, value: &serde_json::Value, requested: bool) {
        let exiting = value["exiting"] == true;
        if exiting {
            self.state.borrow().cancel_all_running();
        }
        ui.set_snap_connected(true);
        ui.set_snap_service_status(
            if exiting {
                "正在等待任务安全结束，随后退出后台"
            } else {
                "后台已连接 · 托盘和热键独立运行"
            }
            .into(),
        );
        ui.set_snap_model(value["model"].as_str().unwrap_or("uninitialized").into());
        ui.set_snap_task(value["task"].as_str().unwrap_or("idle").into());
        if let Some(hotkey) = value["hotkey"].as_str() {
            if !ui.get_snap_recording() && ui.get_snap_hotkey_draft() == ui.get_snap_hotkey() {
                ui.set_snap_hotkey_draft(hotkey.into());
            }
            ui.set_snap_hotkey(hotkey.into());
        }
        if let Some(autostart) = value["autostart"].as_bool() {
            if ui.get_snap_autostart_draft() == ui.get_snap_autostart() {
                ui.set_snap_autostart_draft(autostart);
            }
            ui.set_snap_autostart(autostart);
        }
        if requested
            || value["error"]
                .as_str()
                .is_some_and(|error| !error.is_empty())
        {
            ui.set_snap_error(value["error"].as_str().unwrap_or("").into());
        } else if !ui.get_snap_error().is_empty() {
            // C'-2：纯轮询收到正常状态（ok 且无错误）时清除旧错误文本
            //（如识别中的「正在识别」）；只在当前非空时写，避免每 2 秒
            // 空写引发无谓的界面刷新。
            ui.set_snap_error("".into());
        }
    }

    /// CONVERTER_PREFLIGHT 分支（自 apply_converter_status 抽出）：接受代际 → 消费
    /// options → 忙/关窗仲裁 → launch_markdown_conversion 或复位 cancel → 错误上屏
    /// → close_after 补查。状态借用与副作用顺序原样保留。
    fn apply_converter_preflight(&self, ui: &AppWindow, rest: &str) {
        let mut fields = rest.splitn(3, '|');
        let generation = fields.next().and_then(|value| value.parse::<u64>().ok());
        let ok = fields.next() == Some("1");
        let message = fields.next().unwrap_or_default().to_string();
        let accepted = generation.is_some_and(|generation| {
            let s = self.state.borrow();
            s.convert_preparing && s.convert_preflight_generation == generation
        });
        if accepted {
            let options = {
                let mut s = self.state.borrow_mut();
                s.convert_preparing = false;
                s.convert_pending_options.take()
            };
            ui.set_convert_preparing(false);
            if ok {
                if let Some(options) = options {
                    let closing = self.state.borrow().close_after;
                    // 刻意收窄：预检已接受，convert_preparing 已清 false；这里不查
                    // convert_preparing/snap_initializing，与 gui_activity_in_flight
                    // 的五 getter 合取不同是故意的。
                    if closing
                        || ui.get_busy()
                        || ui.get_convert_initializing()
                        || ui.get_convert_runtime_saving()
                    {
                        self.state.borrow_mut().convert_cancel = None;
                        ui.set_convert_status(
                            if closing {
                                "正在等待其他任务安全结束，随后关闭"
                            } else {
                                "其他任务或组件操作仍在进行；启动检查已通过，请稍后重新开始"
                            }
                            .into(),
                        );
                    } else {
                        launch_markdown_conversion(ui, &self.state, &self.out, options);
                    }
                } else {
                    self.state.borrow_mut().convert_cancel = None;
                }
            } else {
                self.state.borrow_mut().convert_cancel = None;
                if message == "用户取消检查" {
                    ui.set_convert_status("检查已取消，可稍后重试".into());
                } else {
                    ui.set_convert_status("开始前检查失败，请修正后重试".into());
                    if ui.get_screen() == 5 {
                        ui.set_error_text(message.into());
                    } else {
                        ui.set_convert_detail_text(message.into());
                        ui.set_convert_detail_open(false);
                    }
                }
            }
            if self.close_after_if_idle(ui) {
                let _ = slint::quit_event_loop();
            }
        }
    }

    /// CONVERTER_FILE_FINISHED 分支（自 apply_converter_status 抽出）：解析单文件
    /// 终态并推进计数与日志。副作用顺序原样保留。
    fn apply_converter_file_finished(&self, ui: &AppWindow, rest: &str) {
        let mut fields = rest.splitn(5, '|');
        let partial = fields.next() == Some("1");
        let success = fields.next() == Some("1");
        let relative = fields.next().unwrap_or_default();
        let message = fields.next().unwrap_or_default();
        let detail = format!(
            "{}：{}{}",
            if partial {
                "部分提取"
            } else if success {
                "已完成"
            } else {
                "失败"
            },
            relative,
            if message.is_empty() {
                String::new()
            } else {
                format!(" · {message}")
            }
        );
        // 该文件已结束：周期刷新不再把它当作「正在处理」，改报运行耗时。
        {
            let mut s = self.state.borrow_mut();
            s.convert_current.clear();
            let counts = &mut s.convert_counts;
            counts.completed += 1;
            if partial {
                counts.partial += 1;
            } else if success {
                counts.success += 1;
            } else {
                counts.failed += 1;
            }
            ui.set_convert_progress(counts.progress());
            ui.set_convert_progress_note(counts.note().into());
        }
        ui.set_convert_metrics(detail.clone().into());
        if !success || partial {
            let mut state = self.state.borrow_mut();
            push_event_log(&mut state.convert_logs, detail);
            ui.set_convert_log_text(log_panel_text(&state.convert_logs).into());
        }
    }

    /// Event::Status 中 6 个 CONVERTER_* 协议前缀的解码与上屏（转 Markdown 页专属）；
    /// 命中任一前缀返回 true。前缀互斥，判定顺序与拆分前一致，各分支内的副作用
    /// 顺序原样保留。
    pub(super) fn apply_converter_status(&self, ui: &AppWindow, text: &str) -> bool {
        if let Some(log) = text.strip_prefix("CONVERTER_LOG|") {
            let mut s = self.state.borrow_mut();
            push_event_log(&mut s.convert_logs, log.to_string());
            // S1-03：与通用日志的 log_dirty 同口径——只置脏，由轮询尾部按面板
            // 可见性统一重建上屏，一批 N 条日志只做一次整段重排。
            self.convert_log_dirty.set(true);
        } else if let Some(rest) = text.strip_prefix("CONVERTER_PREFLIGHT|") {
            self.apply_converter_preflight(ui, rest);
        } else if let Some(rest) = text.strip_prefix("CONVERTER_READINESS|") {
            let mut fields = rest.splitn(3, '|');
            let generation = fields.next().and_then(|value| value.parse::<u64>().ok());
            let ready = fields.next() == Some("1");
            let message = fields.next().unwrap_or_default();
            let current = generation.is_some_and(|generation| {
                let s = self.state.borrow();
                s.convert_readiness_generation == generation && s.convert_init_cancel.is_none()
            });
            if current && ui.get_busy() {
                // S1-05：当前代就绪结果在任务运行中无法上屏——标记「结果被错过」，
                // 由任务终态（DONE/FAIL 收尾）补查一次，避免页面长期停留过期状态。
                self.state.borrow_mut().convert_readiness_missed = true;
            }
            if current && !ui.get_busy() {
                ui.set_convert_ready(ready);
                if ready {
                    ui.set_convert_detail_text("".into());
                    ui.set_convert_detail_open(false);
                } else if !message.is_empty() {
                    ui.set_convert_detail_text(message.into());
                    ui.set_convert_detail_open(false);
                }
                ui.set_convert_status(
                    if ready {
                        "已安装组件就绪，可离线使用"
                    } else {
                        "组件未就绪，请前往设置检查基础资产或下载音频资产"
                    }
                    .into(),
                );
            }
        } else if let Some(progress) = text.strip_prefix("CONVERTER_INIT|") {
            ui.set_convert_status(progress.into());
        } else if let Some(rest) = text.strip_prefix("CONVERTER_STARTED|") {
            if let Ok(total) = rest.parse::<usize>() {
                let mut s = self.state.borrow_mut();
                s.convert_counts = ConvertCounts {
                    total: Some(total),
                    ..ConvertCounts::default()
                };
                ui.set_convert_progress(s.convert_counts.progress());
                ui.set_convert_progress_note(s.convert_counts.note().into());
                ui.set_convert_metrics(format!("待处理 {total} 个文件").into());
            }
        } else if let Some(rest) = text.strip_prefix("CONVERTER_FILE_STARTED|") {
            let mut fields = rest.splitn(4, '|');
            let index = fields.next().and_then(|v| v.parse::<usize>().ok());
            let total = fields.next().and_then(|v| v.parse::<usize>().ok());
            let relative = fields.next().unwrap_or_default();
            if let (Some(index), Some(total)) = (index, total) {
                let mut s = self.state.borrow_mut();
                let counts = &mut s.convert_counts;
                counts.total = Some(total);
                counts.completed = index.saturating_sub(1);
                // index 已包含扫描期终态（已有结果、重名跳过和扫描失败）。
                counts.skipped = counts
                    .completed
                    .saturating_sub(counts.success + counts.partial + counts.failed);
                ui.set_convert_progress(counts.progress());
                ui.set_convert_progress_note(counts.note().into());
                relative.clone_into(&mut s.convert_current);
                ui.set_convert_metrics(format!("正在处理 {relative}").into());
            }
        } else if let Some(relative) = text.strip_prefix("CONVERTER_FILE_CANCELLED|") {
            let detail = format!("已取消：{relative}；未完成结果不提交");
            ui.set_convert_metrics(detail.clone().into());
            let mut state = self.state.borrow_mut();
            state.convert_current.clear();
            push_event_log(&mut state.convert_logs, detail);
            ui.set_convert_log_text(log_panel_text(&state.convert_logs).into());
        } else if let Some(rest) = text.strip_prefix("CONVERTER_FILE_FINISHED|") {
            self.apply_converter_file_finished(ui, rest);
        } else {
            return false;
        }
        true
    }

    /// SETTINGS_LOADED（启动快照）与 SETTINGS_READY（设置操作收尾）共用的上屏。
    /// `terminal` 为真表示这是设置操作自己的终态：清掉该操作置起的保存/初始化
    /// 标志，且仅在没有任何运行中任务/前置检查/截图初始化时消费 close_after
    /// （U-09/S1-01：任务运行中到达的收尾事件不得提前退出事件循环，关闭由任务
    /// 终态统一完成，镜像 on_close_requested 的守卫口径）；为假（启动快照）时
    /// 两者都不碰——迟到快照不得抹掉运行中的初始化状态。
    pub(super) fn apply_settings_snapshot(&self, ui: &AppWindow, payload: &str, terminal: bool) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
            return;
        };
        ui.set_settings_loaded(true);
        ui.set_settings_load_failed(false);
        ui.set_settings_document_status(
            value["document_status"].as_str().unwrap_or("未下载").into(),
        );
        ui.set_settings_snapshot_status(
            value["snapshot_status"].as_str().unwrap_or("未下载").into(),
        );
        ui.set_settings_media_status(value["media_status"].as_str().unwrap_or("未下载").into());
        ui.set_settings_ffmpeg_status(value["ffmpeg_status"].as_str().unwrap_or("未下载").into());
        ui.set_settings_audio_model_status(
            value["audio_model_status"]
                .as_str()
                .unwrap_or("未下载")
                .into(),
        );
        ui.set_settings_release(value["release"].as_str().unwrap_or_default().into());
        let active = value["active"].as_str().unwrap_or_default();
        ui.set_convert_runtime_dir(active.into());
        ui.set_convert_runtime_confirmed(!active.is_empty());
        ui.set_settings_status(
            value["status"]
                .as_str()
                .unwrap_or(if active.is_empty() {
                    "未找到共享运行目录，请使用安装包修复 / 重装，或重新解包便携 ZIP"
                } else {
                    "离线检查完成；音频可选资产未下载不影响文档与截图 OCR"
                })
                .into(),
        );
        if let Some(warning) = value["migration_warning"].as_str() {
            // 旧来源仍可用不等于迁移已保存，不能显示为迁移成功。
            ui.set_settings_status(warning.into());
        }
        if terminal {
            ui.set_settings_audio_failed(false);
        }
        if terminal {
            ui.set_convert_runtime_saving(false);
            ui.set_convert_initializing(false);
            self.state.borrow_mut().convert_init_cancel = None;
        }
        if !active.is_empty() {
            ensure_snap_supervisor(&self.state, &self.out);
            start_markdown_readiness(ui, &self.state, &self.out);
            start_snap_readiness(ui, &self.state, &self.out);
        }
        if terminal {
            // 设置操作自身的标志已在上面清理；统一守卫仍会等待其它任务、转换预检/
            // 初始化及截图初始化完成后再消费 close_after。
            if self.close_after_if_idle(ui) {
                let _ = slint::quit_event_loop();
            }
        }
    }

    /// 仅在所有 GUI 管理的操作均空闲后消费已确认的关闭请求。
    pub(super) fn close_after_if_idle(&self, ui: &AppWindow) -> bool {
        let idle = !gui_activity_in_flight(ui);
        idle && std::mem::take(&mut self.state.borrow_mut().close_after)
    }
}

/// 摘要上屏：任务收尾与失败重载共用（失败重载的摘要与计划页取自同一次后台开库）。
pub(super) fn apply_summary(ui: &AppWindow, state: &mut State, summary: &Summary) {
    ui.set_summary(summary.description().into());
    // 计数为显示用途，超出 i32 的极端值饱和显示即可。
    ui.set_plan_delete_count(i32::try_from(summary.planned_delete).unwrap_or(i32::MAX));
    ui.set_plan_move_count(i32::try_from(summary.planned_move).unwrap_or(i32::MAX));
    ui.set_plan_git_count(i32::try_from(summary.planned_git).unwrap_or(i32::MAX));
    ui.set_plan_empty_count(i32::try_from(summary.planned_empty).unwrap_or(i32::MAX));
    ui.set_metrics(
        format!(
            "扫描 {} 个文件 · 错误 {} 项 · 已永久删除 {} 项",
            summary.scanned, summary.errors, summary.deleted
        )
        .into(),
    );
    state.planned = summary.planned_delete + summary.planned_move + summary.planned_empty;
}
