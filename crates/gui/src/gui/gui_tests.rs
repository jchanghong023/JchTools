//! 无头 GUI 测试：Slint 平台绑定初始化线程，因此所有用例经专用工作线程串行执行，
//! 每个用例开始前重置为初始状态，互不干扰且与显示器/事件循环解耦。
#![warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]
use super::*;
use slint::Model;
use std::sync::{mpsc, Mutex, OnceLock};

type ReviewDirectoryProbe = dyn Fn() -> bool + Send + Sync;

pub(super) fn review_remediation_directory_probes(
) -> &'static Mutex<HashMap<PathBuf, Arc<ReviewDirectoryProbe>>> {
    static PROBES: std::sync::LazyLock<Mutex<HashMap<PathBuf, Arc<ReviewDirectoryProbe>>>> =
        std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
    &PROBES
}

#[derive(Default)]
pub(super) struct ReviewProbeGate {
    pub(super) entered: std::sync::atomic::AtomicBool,
    released: Mutex<bool>,
    changed: std::sync::Condvar,
}

impl ReviewProbeGate {
    pub(super) fn probe(&self, result: bool) -> bool {
        self.entered.store(true, Ordering::Release);
        let mut released = self
            .released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !*released {
            released = self
                .changed
                .wait(released)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        result
    }
    pub(super) fn release(&self) {
        *self
            .released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.changed.notify_all();
    }
}

pub(super) struct ReviewProbeRelease(pub(super) Arc<ReviewProbeGate>);

impl Drop for ReviewProbeRelease {
    fn drop(&mut self) {
        self.0.release();
    }
}

pub(super) struct ReviewProbeRegistration {
    pub(super) paths: Vec<PathBuf>,
    pub(super) gates: Vec<Arc<ReviewProbeGate>>,
}

impl ReviewProbeRegistration {
    pub(super) fn new() -> Self {
        Self {
            paths: Vec::new(),
            gates: Vec::new(),
        }
    }
    pub(super) fn insert(&mut self, path: PathBuf, gate: Arc<ReviewProbeGate>, result: bool) {
        self.paths.push(path.clone());
        self.gates.push(gate.clone());
        review_remediation_directory_probes()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(path, Arc::new(move || gate.probe(result)));
    }
}

impl Drop for ReviewProbeRegistration {
    fn drop(&mut self) {
        for gate in &self.gates {
            gate.release();
        }
        let mut probes = review_remediation_directory_probes()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for path in &self.paths {
            probes.remove(path);
        }
    }
}

pub(super) fn review_remediation_edit_without_waiting(ui: &AppWindow, gate: Arc<ReviewProbeGate>) {
    let (returned, callback_returned) = mpsc::channel();
    let (decision, result) = mpsc::channel();
    let watchdog = std::thread::spawn(move || {
        let responsive = callback_returned
            .recv_timeout(Duration::from_secs(2))
            .is_ok();
        if !responsive {
            gate.release();
        }
        let _ = decision.send(responsive);
    });
    ui.invoke_root_edited();
    let _ = returned.send(());
    let responsive = result.recv().unwrap();
    watchdog.join().unwrap();
    assert!(responsive, "root_edited 必须在目录 probe 未放行时返回");
}

pub(super) fn review_remediation_configure_conversion(app: &GuiTestApp, root: &Path) {
    let input = root.join("input");
    let output = root.join("output");
    std::fs::create_dir_all(&input).unwrap();
    std::fs::create_dir_all(&output).unwrap();
    app.ui.invoke_select_tool("markdown-converter".into());
    app.ui
        .set_convert_runtime_dir(root.display().to_string().into());
    app.ui.set_convert_runtime_confirmed(true);
    app.ui
        .set_convert_input_dir(input.display().to_string().into());
    app.ui
        .set_convert_output_dir(output.display().to_string().into());
}

pub(super) struct GuiTestApp {
    pub(super) ui: AppWindow,
    pub(super) state: Rc<RefCell<State>>,
    pub(super) pump: UiPump,
}

impl GuiTestApp {
    fn reset(&self) {
        // 上一用例可能残留未排空的事件：先丢弃，避免串到本用例的断言里。
        while self.pump.receiver.try_iter().next().is_some() {}
        *self.state.borrow_mut() = initial_state().unwrap();
        self.ui.set_ready(false);
        self.ui.set_error_text("".into());
        self.ui.set_notice_text("".into());
        self.ui.set_theme(0);
        self.ui.set_acp_executable("".into());
        self.ui.set_acp_arguments("".into());
        self.ui.set_acp_port("8765".into());
        self.ui.set_acp_saved_config("正在读取…".into());
        self.ui.set_acp_running_config("正在读取…".into());
        self.ui.set_acp_phase("unconfigured".into());
        self.ui
            .set_acp_status("正在后台读取配置并连接模型服务…".into());
        self.ui.set_acp_status_known(false);
        self.ui.set_acp_ready(false);
        self.ui.set_acp_pending_apply(false);
        self.ui.set_acp_request_pending(false);
        self.ui.set_acp_explicit_stopped(false);
        self.ui.set_acp_service_pid("".into());
        self.ui.set_acp_agent_pid("".into());
        self.ui.set_acp_executing(0);
        self.ui.set_acp_waiting(0);
        self.ui.set_acp_operation("".into());
        self.ui.set_acp_action_error("".into());
        self.ui.set_acp_service_error("".into());
        self.ui.set_confirm_kind(0);
        self.ui.set_confirm_pending(false);
        self.ui.set_section(0);
        self.ui.set_panel(0);
        self.ui.set_show_advanced(false);
        self.ui.set_screen(0);
        self.ui.set_active_tool_id("directory-organizer".into());
        self.ui.set_quarantined_count(0);
        self.ui.set_acknowledge(false);
        self.ui.set_directory("".into());
        self.ui.set_has_task(false);
        self.ui.set_busy(false);
        // 转换页的编辑属性不属于 State；每个 GUI 用例必须显式恢复，
        // 否则一个超大 timeout 输入会污染后续用例。
        self.ui.set_convert_input_dir("".into());
        self.ui.set_convert_output_dir("".into());
        self.ui.set_convert_flat(false);
        self.ui.set_convert_pdf(true);
        self.ui.set_convert_office(true);
        self.ui.set_convert_images(true);
        self.ui.set_convert_media(true);
        self.ui.set_convert_other(true);
        self.ui.set_convert_timeout_secs("21600".into());
        self.ui.set_convert_timeout_error("".into());
        self.ui.set_convert_ready(false);
        self.ui.set_convert_runtime_confirmed(false);
        self.ui.set_convert_initializing(false);
        self.ui.set_convert_preparing(false);
        self.ui.set_convert_runtime_saving(false);
        self.ui.set_convert_runtime_dir("".into());
        self.ui.set_settings_loaded(false);
        self.ui.set_settings_load_failed(false);
        self.ui.set_settings_document_status("未下载".into());
        self.ui.set_settings_snapshot_status("未下载".into());
        self.ui.set_settings_media_status("未下载".into());
        self.ui.set_settings_ffmpeg_status("未下载".into());
        self.ui.set_settings_audio_model_status("未下载".into());
        self.ui.set_settings_release("".into());
        self.ui.set_settings_audio_failed(false);
        self.ui.set_settings_status("正在读取配置…".into());
        self.ui.set_convert_status("尚未检查可选组件".into());
        self.ui.set_convert_detail_text("".into());
        self.ui.set_convert_detail_open(false);
        self.ui.set_convert_metrics("尚未开始".into());
        self.ui.set_convert_progress(-1.0);
        self.ui.set_convert_progress_note("".into());
        self.ui.set_snap_asset_status("尚未检查可选组件".into());
        self.ui.set_snap_asset_detail("".into());
        self.ui.set_snap_detail_open(false);
        self.ui.set_progress(-1.0);
        self.ui.set_progress_note("".into());
        self.ui.set_plan_filter(0);
        self.ui.set_plan_prev_enabled(false);
        self.ui.set_plan_next_enabled(false);
        self.ui.set_summary("".into());
        self.ui.set_metrics("".into());
        self.ui.set_status("".into());
        self.ui.set_confirm_text("".into());
        self.ui
            .set_plans(Rc::new(VecModel::from(Vec::<PlanRow>::new())).into());
        self.ui.set_md_subpage(0);
        self.ui.set_fail_ready(false);
        self.ui
            .set_fail_rows(Rc::new(VecModel::from(Vec::<FailRow>::new())).into());
        self.ui.set_fail_total(0);
        self.ui.set_fail_prev_enabled(false);
        self.ui.set_fail_next_enabled(false);
        self.ui.set_fail_page_label("第 1 页".into());
        self.ui.set_git_branch("".into());
        self.ui.set_git_upstream("".into());
        self.ui.set_git_current("".into());
        self.ui.set_git_stage("".into());
        self.ui.set_git_retry(0);
        self.ui.set_git_retry_wait(0);
        self.ui.set_git_total(0);
        self.ui.set_git_done(0);
        self.ui.set_git_state("".into());
        reset_tool_list(&self.ui);
        refresh(&self.ui, &self.state.borrow());
    }
}

type Job = Box<dyn FnOnce(&GuiTestApp) + Send>;

fn gui() -> &'static Mutex<mpsc::Sender<Job>> {
    static JOBS: OnceLock<Mutex<mpsc::Sender<Job>>> = OnceLock::new();
    JOBS.get_or_init(|| {
        // C'-1：装配先隔离截图服务端点——本装配的 State 常驻进程，监督线程
        // 可能在任意用例中被触发（如切换 snap-ocr 页的真实导航回调），隔离
        // 变量一经设置不恢复。设置在共享进程环境锁内，与 snap_ocr_assets /
        // markdown_assets 改写同一变量的用例互不覆盖；覆盖生效后管道名与
        // 资产根都脱离生产位置，ensure 链路只能对测试专用名失败返回。
        {
            let _lock = jchtools_foundation::test_support::env_lock()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::env::set_var(
                "JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT",
                snap_test_isolated_root(),
            );
        }
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("gui-test-worker".into())
            .spawn(move || {
                i_slint_backend_testing::init_no_event_loop();
                // 每个用例拥有独立窗口、状态和事件通道。仅排空共享通道无法
                // 隔离上一用例尚未结束的后台线程，它们可能稍后继续发送事件。
                while let Ok(job) = rx.recv() {
                    let ui = AppWindow::new().unwrap();
                    let state = Rc::new(RefCell::new(initial_state().unwrap()));
                    ui.set_tool_count(i32::try_from(registry::tools().len()).unwrap_or(i32::MAX));
                    let tools = full_tool_rows();
                    ui.set_tools(Rc::new(VecModel::from(tools)).into());
                    // 真实事件泵：后台 worker（计划加载、就绪快照、勾选保存）把结果发到通道，
                    // 用例用 `pump_until` 排空并应用，走的是与事件循环完全相同的代码路径。
                    let (event_tx, event_rx) = control::event_channel();
                    let out = EventSender::new(event_tx);
                    wire_sync(&ui, &state, &out);
                    wire_md_git(&ui, &state, &out);
                    // 截图 OCR 回调同样接入无头装配：录制热键等纯界面逻辑可经真实回调断言；
                    // 仅装配不启动任何资产/服务线程（那些由用户主动回调触发）。
                    wire_snap_ocr(&ui, &state, &out);
                    wire_markdown_converter(&ui, &state, &out);
                    wire_settings(&ui, &state, &out);
                    wire_acp_http(&ui, &state);
                    wire_codeagent(&ui, &state);
                    wire_task_lifecycle(&ui, &state, &out);
                    refresh(&ui, &state.borrow());
                    let pump = UiPump::new(event_rx, state.clone(), out);
                    let app = GuiTestApp { ui, state, pump };
                    app.reset();
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(&app)));
                }
            })
            .expect("启动 GUI 测试工作线程");
        Mutex::new(tx)
    })
}

/// GUI 测试的截图资产隔离根（C'-1）：仓库 `.tmp/` 下固定目录，进程内首次
/// 访问时创建，与生产 state dir 完全分离。目录常驻进程不删除，由
/// `python scripts/make_tmp.py clean` 统一清理（`.tmp/` 约定）。
pub(super) fn snap_test_isolated_root() -> PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let root = manifest_dir
            .ancestors()
            .find(|path| {
                path.join("ui/app.slint").is_file() && path.join("resources/rules.json").is_file()
            })
            .expect("GUI 测试需要仓库根目录")
            .join(".tmp")
            .join("gui-test-snap-assets");
        let _ = std::fs::create_dir_all(&root);
        root
    })
    .clone()
}

/// 驱动后台结果落地：真实 worker 线程发送、真实事件泵应用，最多等 30 秒。
/// Windows 上逐条启动 Git 的开销可能超过 5 秒；不以该偶然耗时替代功能断言。
/// `done` 为真即返回；超时返回 false，由调用方断言给出可读失败原因。
pub(super) fn pump_until(app: &GuiTestApp, done: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        app.pump.run(&app.ui);
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

pub(super) fn with_gui<R: Send + 'static>(
    job: impl FnOnce(&GuiTestApp) -> R + Send + 'static,
) -> Result<R, String> {
    let (result_tx, result_rx) = mpsc::sync_channel::<Result<R, String>>(1);
    gui()
        .lock()
        .unwrap()
        .send(Box::new(move |app| {
            // 捕获用例内的 panic 并把消息经结果通道带回：worker 线程必须存活以服务后续用例，
            // 同时失败原因要随测试结果透出，而不是被 RecvError 掩盖。
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(app)));
            match outcome {
                Ok(value) => {
                    let _ = result_tx.send(Ok(value));
                }
                Err(payload) => {
                    let msg = if let Some(m) = payload.downcast_ref::<&'static str>() {
                        (*m).to_string()
                    } else if let Some(m) = payload.downcast_ref::<String>() {
                        m.clone()
                    } else {
                        "未知 panic".to_string()
                    };
                    let _ = result_tx.send(Err(msg));
                }
            }
        }))
        .unwrap();
    result_rx.recv().unwrap()
}

pub(super) fn rule_value_at(ui: &AppWindow, key: &str) -> Option<String> {
    let rules = ui.get_rules();
    (0..rules.row_count()).find_map(|i| {
        let row = rules.row_data(i).unwrap();
        (row.key.as_str() == key).then(|| row.value.to_string())
    })
}

pub(super) fn with_settings_fixture(action: impl FnOnce(&Path)) {
    let _lock = jchtools_foundation::test_support::env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = tempfile::tempdir().unwrap();
    struct Restore(Vec<(&'static str, Option<std::ffi::OsString>)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            for (key, value) in &self.0 {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
    let keys = [
        "JCHTOOLS_TEST_STATE_DIR",
        "JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT",
    ];
    let _restore = Restore(
        keys.into_iter()
            .map(|key| (key, std::env::var_os(key)))
            .collect(),
    );
    std::env::set_var(keys[0], root.path().join("state"));
    std::env::set_var(keys[1], root.path().join("markdown"));
    action(root.path());
}

pub(super) fn deliver_settings_loaded(app: &GuiTestApp) {
    let generation = app.state.borrow().settings_generation;
    app.pump
        .out
        .send(Event::Notice(format!(
            "SETTINGS_LOADED|{generation}|{}",
            settings_payload().unwrap()
        )))
        .unwrap();
    app.pump.run(&app.ui);
}

pub(super) fn temp_test_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jchtools-gui-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 构造 MD 输入/输出目录：`file_count` 个带内容的 .md 文件。
pub(super) fn make_md_fixture(tag: &str, file_count: usize) -> (PathBuf, PathBuf, PathBuf) {
    let dir = temp_test_dir(tag);
    let docs = dir.join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    let content = "# 标题\n\n正文内容\n".repeat(8192); // 约 120KB/文件
    for i in 0..file_count {
        std::fs::write(docs.join(format!("doc{i:03}.md")), &content).unwrap();
    }
    let out_dir = dir.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    (docs, out_dir, dir)
}

/// 构造带失败事件的解压任务库（U-10）：返回任务库目录。
/// 事件 id 顺序即写入顺序；与 archive.rs 的真实写入序列一致：
/// 失败记录在前，其卷集的「移入解压失败」紧跟其后，成功包与任务级记录穿插。
pub(super) fn make_extract_task_db(tag: &str) -> PathBuf {
    let dir = temp_test_dir(tag);
    let db = Database::create(&dir).unwrap();
    // 失败包 1：解压失败（引擎报错），两个卷整组隔离
    db.log("解压", "bad.7z", "", "失败", "压缩包已损坏", 0)
        .unwrap();
    db.log(
        "解压",
        "bad.7z",
        "解压失败/bad.7z",
        "移入解压失败",
        "解压失败：压缩包已损坏",
        0,
    )
    .unwrap();
    db.log(
        "解压",
        "bad.7z.002",
        "解压失败/bad.7z.002",
        "移入解压失败",
        "解压失败：压缩包已损坏",
        0,
    )
    .unwrap();
    // 失败包 2：未完全解开（成员被跳过），一个卷已隔离
    db.log(
        "解压",
        "part.zip",
        "",
        "未完全解开",
        "有成员被跳过或未落盘（排除规则）；原包保留",
        0,
    )
    .unwrap();
    db.log(
        "解压",
        "part.zip",
        "解压失败/part.zip",
        "移入解压失败",
        "未能完全解开：有成员被跳过",
        0,
    )
    .unwrap();
    // 成功包与任务级记录：不得出现在失败列表
    db.log(
        "解压",
        "ok.zip",
        "",
        "成功",
        "已完全解开；原包与分卷按 X-05 永久删除",
        0,
    )
    .unwrap();
    // 源包清理失败：X-05 单独阶段，不冒充坏包
    db.log(
        "删除",
        "ok2.zip",
        "",
        "失败",
        "源包清理未完成：删除时出错",
        0,
    )
    .unwrap();
    db.log("任务", "", "", "完成", "解压结束：成功 1 包", 0)
        .unwrap();
    dir
}

/// 从指标行的「耗时 N.Ns」/「总耗时 N.Ns」提取耗时秒数（T-22 测试辅助）：
/// `find("耗时")` 同时命中「总耗时」后半段，取其后首个 f64。
pub(super) fn elapsed_in_metrics(metrics: &str) -> Option<f64> {
    let tail = &metrics[metrics.find("耗时")? + "耗时".len()..];
    let end = tail.find('s')?;
    tail[..end].trim().parse().ok()
}
