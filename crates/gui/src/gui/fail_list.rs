//! U-10 解压失败列表：分页取数 worker、任务目录 watcher 与列表重置（自 gui.rs 拆分）。
use super::*;

/// U-10 失败列表的一行数据（worker 取数结果）：一个失败包或 X-10 卷集对应一项。
#[derive(Debug)]
pub(super) struct FailItem {
    /// 阶段标识：解压失败 / 未完全解开 / 源包清理失败（X-05 单独标明，不冒充坏包）。
    pub(super) stage: &'static str,
    /// 原路径（相对所选目录）。
    pub(super) source: String,
    /// 失败原因（任务库事件 reason 字段）。
    pub(super) reason: String,
    /// 隔离后位置；无隔离记录时为空（界面如实显示「未记录隔离位置」，不编造「已隔离」）。
    pub(super) target: String,
}

/// 失败列表分页 worker 的取数结果。
#[derive(Debug)]
pub(super) struct FailPage {
    pub(super) task: PathBuf,
    pub(super) generation: u64,
    pub(super) page: usize,
    pub(super) items: Vec<FailItem>,
    pub(super) total: u64,
    pub(super) has_more: bool,
    /// 本页最后一项的任务库事件 id：下一页的取数游标。
    pub(super) last_id: i64,
    pub(super) error: Option<String>,
}

/// 失败列表专用通道的消息。`Event` 枚举属于引擎事件协议（control.rs），这里不扩展它：
/// 解压失败列表的分页结果与任务目录发现走独立通道，由事件泵每拍排空，
/// 与计划页 worker 同一「界面线程不开库、后台分页取数」架构（H-02）。
#[derive(Debug)]
pub(super) enum FailChannelMsg {
    /// watcher 发现当前解压任务的任务库目录（引擎在状态目录下创建，界面事先不知道路径；
    /// 首项为发起解压时的清点请求代际，迟到的发现按代际整体丢弃）。
    TaskFound(PathBuf, u64),
    /// 分页取数结果。
    Page(FailPage),
}

/// 解压失败列表的取数条件（U-10/X-06/S-07）：
/// - `解压` 阶段的 `失败`（引擎报错）与 `未完全解开`（成员被跳过/排除等）记录：一个
///   失败包或卷集对应一项；
/// - `删除` 阶段的 `失败` 记录：源包清理失败按 X-05 单独标明阶段，不冒充坏包；
/// - `移入解压失败` 记录不单独成项，只作为其后最近一个失败项的隔离后位置归属。
const FAIL_ITEM_PREDICATE: &str =
    "(phase='解压' AND result IN ('失败','未完全解开')) OR (phase='删除' AND result='失败')";

/// 运行中失败列表的自动刷新间隔（U-10「运行中可查看已经发生的失败」）。
pub(super) const FAIL_REFRESH_MIN: Duration = Duration::from_secs(2);

/// watcher 等待任务库目录出现的超时：引擎在解压开始的最初几步内建库；超时即放弃，
/// 任务收尾（ExtractDone/Failed）会按事件自带的路径刷新，失败列表不会因此缺数据。
pub(super) const EXTRACT_TASK_WATCH_TIMEOUT: Duration = Duration::from_secs(120);

/// 轮询状态目录的 tasks/ 子目录，发现快照之外的新任务目录即回传（U-10）。
/// 任务库目录由引擎在 extract worker 内部创建、路径随结果事件返回时任务已结束，
/// 运行中查看失败列表必须先发现目录：启动解压前快照既有目录，本函数只做只读
/// 文件系统访问，与分页 worker 一样不进界面线程。
pub(super) fn watch_extract_task(
    tasks_root: &std::path::Path,
    known: &[PathBuf],
    generation: u64,
    sender: &mpsc::Sender<FailChannelMsg>,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(entries) = std::fs::read_dir(tasks_root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() && !known.contains(&path) {
                    let _ = sender.send(FailChannelMsg::TaskFound(path, generation));
                    return;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 发起失败列表分页取数：worker 线程开任务库查询（界面线程不开库，H-02），
/// 结果经专用通道由事件泵应用；请求代际递增，迟到的旧结果整体丢弃。
pub(super) fn request_fail_page(state: &Rc<RefCell<State>>, page: usize) {
    let (task, generation, start, sender) = {
        let mut s = state.borrow_mut();
        let Some(task) = s.extract_task.clone() else {
            return;
        };
        s.fail_gen += 1;
        (
            task,
            s.fail_gen,
            s.fail_page_starts.get(page).copied().unwrap_or(0),
            s.fail_sender.clone(),
        )
    };
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            query_fail_page(&task, start, page, generation)
        }));
        let fail = result
            .unwrap_or_else(|_| Err(anyhow::anyhow!("失败列表加载时后台操作意外退出")))
            .unwrap_or_else(|error| FailPage {
                task: task.clone(),
                generation,
                page,
                items: Vec::new(),
                total: 0,
                has_more: false,
                last_id: 0,
                error: Some(format!("{error:#}")),
            });
        let _ = sender.send(FailChannelMsg::Page(fail));
    });
}

/// 查询一页失败项（U-10）：只在 worker 线程调用。隔离后位置按事件顺序归组——
/// 一个失败包的「移入解压失败」记录紧跟该包的失败记录、且先于下一个失败记录
/// （解压按包串行处理），据此把整组卷的隔离位置合并进同一项。
pub(super) fn query_fail_page(
    task: &Path,
    start: i64,
    page: usize,
    generation: u64,
) -> Result<FailPage> {
    let db = Database::open_existing(task)?;
    let total: i64 = db.conn.query_row(
        &format!("SELECT COUNT(*) FROM events WHERE ({FAIL_ITEM_PREDICATE})"),
        [],
        |row| row.get(0),
    )?;
    // 多取一条探测「还有下一页」，随后截断到 100 行（与计划页每页 100 条同口径）。
    // 注意谓词含 OR，必须整体加括号再与 `id > ?1` 组合，否则 AND 优先级会把游标
    // 条件吞进第二个分支（回归：失败列表翻页失效、隔离位置归组为空）。
    let mut stmt = db.conn.prepare(&format!(
        "SELECT id, phase, result, source, reason FROM events WHERE ({FAIL_ITEM_PREDICATE}) AND id > ?1 ORDER BY id LIMIT 101"
    ))?;
    let query = stmt.query_map(rusqlite::params![start], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    let mut rows = query.collect::<rusqlite::Result<Vec<_>>>()?;
    let has_more = rows.len() > 100;
    rows.truncate(100);
    // 本页末项的隔离记录上界：下一个失败项的事件 id（没有则直到库尾）。
    let upper_bound: i64 = match rows.last() {
        None => 0,
        Some((last_id, ..)) => db
            .conn
            .query_row(
                &format!("SELECT MIN(id) FROM events WHERE ({FAIL_ITEM_PREDICATE}) AND id > ?1"),
                rusqlite::params![last_id],
                |row| row.get::<_, Option<i64>>(0),
            )?
            .unwrap_or(i64::MAX),
    };
    let mut items = Vec::new();
    if !rows.is_empty() {
        let first_id = rows[0].0;
        let mut qstmt = db.conn.prepare(
            "SELECT id, target, result, reason FROM events WHERE result IN ('移入解压失败','隔离失败') AND id > ?1 AND id < ?2 ORDER BY id",
        )?;
        let quarantine_query =
            qstmt.query_map(rusqlite::params![first_id, upper_bound], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?;
        let quarantines = quarantine_query.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut quarantines = quarantines.iter().peekable();
        for (index, (_, phase, result, source, reason)) in rows.iter().enumerate() {
            // 归组区间 (本项 id, 下一失败项 id)：页内以下一项为界，页末项用 upper_bound。
            let bound = rows
                .get(index + 1)
                .map_or(upper_bound, |(next_id, ..)| *next_id);
            let mut moved = Vec::new();
            let mut reason = reason.clone();
            while quarantines.peek().is_some_and(|(qid, ..)| *qid < bound) {
                let Some((_, target, result, detail)) = quarantines.next() else {
                    break;
                };
                if result == "移入解压失败" {
                    moved.push(target.as_str());
                } else {
                    reason.push_str("\n隔离失败：");
                    reason.push_str(detail);
                }
            }
            let mut target = moved.join("；");
            if moved.len() > 1 {
                use std::fmt::Write as _;
                write!(target, "（已隔离 {} 个卷）", moved.len())?;
            }
            let stage = if phase == "删除" {
                "源包清理失败（X-05）"
            } else if result == "未完全解开" {
                "未完全解开"
            } else {
                "解压失败"
            };
            items.push(FailItem {
                stage,
                source: source.clone(),
                reason,
                target,
            });
        }
    }
    Ok(FailPage {
        task: task.to_path_buf(),
        generation,
        page,
        total: u64::try_from(total.max(0)).unwrap_or(0),
        has_more,
        last_id: rows.last().map_or(0, |(id, ..)| *id),
        items,
        error: None,
    })
}

/// U-10：失败列表只保留「本次」解压任务的数据——启动下一次解压任务时清空上一任务的
/// 列表与分页游标并使其在途结果过期（任务结束/取消后、切换工具时不清空）。
pub(super) fn reset_fail_list(ui: &AppWindow, state: &mut State) {
    state.extract_task = None;
    state.fail_page = 0;
    state.fail_page_starts = vec![0];
    state.fail_gen += 1;
    ui.set_fail_ready(false);
    ui.set_fail_rows(Rc::new(VecModel::from(Vec::<FailRow>::new())).into());
    ui.set_fail_total(0);
    ui.set_fail_prev_enabled(false);
    ui.set_fail_next_enabled(false);
    ui.set_fail_page_label("第 1 页".into());
}
