use crate::{
    archive::{self, SevenZip},
    config::{self, Config, DeleteMode},
    control::{Context as TaskContext, Control, Event},
    db::Database,
    fsutil, hashing,
    model::{Action, ActionKind, Snapshot, Summary},
    planner,
    platform::{self, DeleteResult},
    rules,
};
use anyhow::{bail, Context as _, Result};
use rayon::prelude::*;
use rusqlite::params;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Arc, Mutex},
};

pub struct Job {
    pub root: PathBuf,
    pub config: Config,
    pub context: TaskContext,
    pub db: Database,
    pub summary: Summary,
}
/// H-06：选定根目录直接含 .git（目录或文件）时整次处理不执行的统一提示。
/// 只允许为识别边界做必要的目录项检查，识别后不读取、不改动任何内容。
const ROOT_GIT_MESSAGE: &str = "所选根目录直接含 .git（Git 仓库或工作树）；按 H-06 整次处理不执行。请改选不含 .git 的子目录后重试。";
/// X-07：所选根本身名为「解压失败」时不开始解压的统一提示（确认框清点与解压一致拒绝）。
const ROOT_QUARANTINE_MESSAGE: &str = "所选目录本身名为「解压失败」（隔离容器）；按 X-07 不开始解压。请先将待重试的包移出隔离容器后重试。";
/// X-07：所选根本身名为「解压失败」（不区分大小写）时拒绝开始解压。
/// 容器名本身没有大小写变体，ASCII 折叠与 archive.rs 的组件名判定同口径。
fn root_is_quarantine(root: &Path) -> bool {
    root.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(archive::QUARANTINE_DIR_NAME))
}
/// 哈希阶段一次从任务库取多少条候选。与哈希线程数解耦（线程数只决定并行度，
/// 批量只决定分页次数），避免「调线程数」同时改变两个量而无法判断。
const HASH_BATCH: usize = 256;
/// 执行阶段每批动作数：一批一个事务。每动作 4 条语句若各自自动提交，实测单条约 40µs
/// （2 万项执行阶段 9.2s 里约 3.6s 花在提交上）；合批后每动作提交摊到 7µs。
/// 64 是实测折中：256 批次只再快 6%，但崩溃/取消时未提交记录的窗口放大 4 倍；
/// 64 与界面计划页（101 行）同量级，用户看到的进度滞后不超过一页。
const APPLY_BATCH: usize = 64;
impl Job {
    pub fn log(
        &self,
        phase: &str,
        source: &str,
        target: &str,
        result: &str,
        reason: &str,
        size: u64,
    ) -> Result<()> {
        self.db.log(phase, source, target, result, reason, size)?;
        self.context.emit(Event::Log(format!(
            "[{phase}] {result} | {source}{} | {reason}",
            if target.is_empty() {
                String::new()
            } else {
                format!(" → {target}")
            }
        )));
        Ok(())
    }
    pub fn delete_path(
        &mut self,
        path: &Path,
        expected: Option<&Snapshot>,
        mode: DeleteMode,
        reason: &str,
        physical_free: bool,
    ) -> Result<DeleteResult> {
        let relative = fsutil::relative_string(&self.root, path)?;
        fsutil::safe_join(&self.root, &relative)?;
        let size = expected.map_or(0, |s| s.size);
        // Record intent before a mutation. This is an audit trail, not a recovery journal.
        self.log("删除", &relative, "", "准备", reason, size)?;
        let result = platform::remove(path, mode, &self.context.control)?;
        match result {
            DeleteResult::Kept => {
                self.summary.skipped += 1;
            }
            // 多硬链接源的内容仍由其他链接持有：逻辑大小与 candidate_bytes 同口径，
            // physical_free=false（硬链接替换）时不计入 permanent_bytes，
            // 避免「已永久删除字节」虚高（S-06）。
            DeleteResult::Permanent => {
                self.summary.deleted += 1;
                if physical_free && expected.is_none_or(|s| s.links <= 1) {
                    self.summary.permanent_bytes =
                        self.summary.permanent_bytes.saturating_add(size);
                }
            }
        }
        self.log(
            "删除",
            &relative,
            "",
            match result {
                DeleteResult::Kept => "保留",
                DeleteResult::Permanent => "已永久删除",
            },
            reason,
            size,
        )?;
        // 已经不在磁盘上的文件不能再参与后续按名/按大小的查找：否则同名成员合入时会去读取
        // 一个刚被删除的路径，把整包解压误判为失败。
        if result != DeleteResult::Kept {
            self.db.deactivate_file_rel(&relative)?;
        }
        Ok(result)
    }
}
#[derive(Debug, Clone)]
pub struct TaskResult {
    pub directory: PathBuf,
    pub summary: Summary,
}
/// 清理硬链接执行的崩溃残留（.jchtools-link-{uuid}）：崩溃发生在「源已删、改名回
/// 原路径前」时残留无法自愈，扫描对其永久剪枝且无其它回收路径。残留是指向 keeper
/// 内容的硬链接，删除后内容仍由保留文件持有。24 小时阈值与 clean_orphan_staging
/// 一致，避免误删并发任务的临时文件。
/// S-01/F02：清理名单只来自分析阶段登记的「本次处理范围内疑似残留」（见 scan 的
/// link_residues：扫描时未命中任何范围剪枝规则——glob 排除、非递归深层、隐藏/系统
/// 范围外、隔离容器、Git 整树排除等都不得越出），执行时逐项复核既有谓词：名称前缀、
/// 仍是硬链接（链接数 ≥ 2，内容另有链接持有）、修改超过 24 小时；任一不满足即保留。
/// 普通同名文件可能是用户文件或从压缩包解出的同名成员，静默删除即数据丢失。
/// 残留是本工具自有临时文件：不经计划行、不按用户删除方式处置，一律永久删除；
/// 分析日志已在确认前明示数量与清理条件（S-01：不得范围外静默删除）。
fn clean_orphan_link_temps(job: &mut Job) -> Result<usize> {
    let rels: Vec<String> = job.db.get("link_residues").unwrap_or_default();
    if rels.is_empty() {
        return Ok(0);
    }
    let now = std::time::SystemTime::now();
    let mut removed = 0;
    for rel in &rels {
        job.context.control.checkpoint()?;
        let Ok(path) = fsutil::safe_join(&job.root, rel) else {
            continue;
        };
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with(".jchtools-link-") {
            continue;
        }
        let stale = fs::symlink_metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age.as_secs() > 24 * 3600);
        if !stale {
            continue;
        }
        let residue = fsutil::snapshot(&path).is_ok_and(|s| s.links >= 2);
        if residue && fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}
/// 解析 S-05 的受保护目录：Windows 读取 SystemRoot 并规范化（缺失或不可访问时
/// 报错拒绝开始，不得静默失去系统目录保护）；非 Windows 无此概念，返回 None。
#[cfg(windows)]
fn resolve_protection() -> Result<Option<PathBuf>> {
    Ok(Some(fsutil::protected_root()?))
}
#[cfg(not(windows))]
fn resolve_protection() -> Result<Option<PathBuf>> {
    Ok(None)
}
pub fn prepare(root: &Path, config: Config, context: TaskContext) -> Result<TaskResult> {
    prepare_at(root, config, context, &config::state_dir()?)
}
/// 三条任务入口（prepare / extract_run / count_archives）共用的安全闸门，顺序固定：
/// 配置校验 → S-04 原始路径链接边界 → 规范化 →（仅解压链路）X-07 隔离区名拒绝 →
/// H-06 根含 .git 拒绝 → H-06 祖先含 .git 拒绝。任一闸门失败都不创建任务库；
/// `check_quarantine` 只在解压链路为 true（目录整理不解压，C-01）。
/// 返回规范化后的根目录与 S-05 受保护目录（供扫描剪枝继续使用）。
fn preflight_root(
    root: &Path,
    config: &Config,
    protected: Option<&Path>,
    check_quarantine: bool,
) -> Result<(PathBuf, Option<PathBuf>)> {
    config.validate()?;
    // S-04：先在用户原始路径上检查链接边界（canonicalize 会解析掉 reparse 身份）；
    // 拒绝时不创建任务库、不扫描、不解压。
    fsutil::ensure_plain_entry(root)?;
    let (root, protected) = fsutil::normalize_root_with(root, protected)?;
    // X-07：所选根本身名为「解压失败」时不开始解压，提示先移出待重试的包。
    if check_quarantine {
        anyhow::ensure!(!root_is_quarantine(&root), ROOT_QUARANTINE_MESSAGE);
    }
    // H-06：选定根目录直接含 .git 时整次处理不执行，明确提示且不创建任务库。
    anyhow::ensure!(!fsutil::is_git_root(&root)?, ROOT_GIT_MESSAGE);
    // H-06：祖先直接含 .git 同样拒绝整次处理（不拆散项目子树）。
    fsutil::root_inside_git_project(&root)?;
    Ok((root, protected))
}
/// prepare 与 extract 共用的任务库创建：持全局锁 → 任务目录命名 → 建库 →
/// 初始化事务写入 root/config/status/summary/created/state_dir → 提交。
/// 返回的锁守卫必须由调用方持有到任务结束（drop 即释放互斥）。
fn create_task_db(
    state: &Path,
    root: &Path,
    config: Config,
    context: TaskContext,
    status: &str,
) -> Result<(fsutil::RootGuard, Job)> {
    let guard = fsutil::RootGuard::acquire(state)?;
    let directory = state.join("tasks").join(format!(
        "{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S"),
        uuid::Uuid::new_v4()
    ));
    let db = Database::create(&directory)?;
    let initialization = db.conn.unchecked_transaction()?;
    db.set("root", &fsutil::path_string(root)?)?;
    db.set("config", &config)?;
    db.set("status", &status)?;
    db.set("summary", &Summary::default())?;
    db.set("created", &chrono::Utc::now().to_rfc3339())?;
    // 记录本次的全局锁目录：apply 若按任务目录当前位置推导锁位置，任务目录被移动后
    // 会与这里的互斥失效（apply_with 会优先锁这个记录值）。
    db.set("state_dir", &fsutil::path_string(state)?)?;
    initialization.commit()?;
    let job = Job {
        root: root.to_path_buf(),
        config,
        context,
        db,
        summary: Summary::default(),
    };
    Ok((guard, job))
}
/// 任务失败收尾（prepare 与 extract 同口径）：用户主动取消不是失败——任务库状态
/// 必须能区分「已取消」和「失败」，否则事后检查会误判；三个容错写库失败均不掩盖原错误。
fn record_task_failure(job: &mut Job, error: &anyhow::Error) {
    let cancelled = job.context.control.is_cancelled();
    let _ = job.db.set("summary", &job.summary);
    let _ = job
        .db
        .set("status", &if cancelled { "cancelled" } else { "failed" });
    let _ = job.db.log(
        "任务",
        "",
        "",
        if cancelled { "已取消" } else { "失败" },
        &format!("{error:#}"),
        0,
    );
}
/// 独立状态目录使核心可在无 GUI 下测试。目录整理不解压（C-01），不再接受引擎注入；
/// 真实引擎用例走 extract_run_at（E-02：引擎只能按固定顺序获得，不向用户提供路径参数）。
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "organize_analyze", skip_all, err)
)]
pub fn prepare_at(
    root: &Path,
    config: Config,
    context: TaskContext,
    state: &Path,
) -> Result<TaskResult> {
    let protected = resolve_protection()?;
    prepare_at_with(root, config, context, state, protected.as_deref())
}
/// S-05 受保护目录参数化变体（测试注入合成受保护目录），其余行为与 [`prepare_at`] 一致。
fn prepare_at_with(
    root: &Path,
    config: Config,
    context: TaskContext,
    state: &Path,
    protected: Option<&Path>,
) -> Result<TaskResult> {
    let (root, protected) = preflight_root(root, &config, protected, false)?;
    let (_guard, mut job) = create_task_db(state, &root, config, context, "analyzing")?;
    let directory = job.db.directory.clone();
    let result = (|| {
        // C-01：分析阶段只读，不做任何清扫（含本工具崩溃残留的硬链接临时文件——
        // 它们被扫描永久剪枝，不影响计划正确性；清扫统一在 apply_with 执行前进行）。
        // C-01：目录整理的分析阶段只读——不解压（解压职责整体移交「递归解压」工具，X-01），
        // 扫描即终态；树里既有压缩包按普通文件参与后续去重/归类。
        scan(&mut job, false, state, protected.as_deref())?;
        hash_candidates(&mut job)?;
        job.context
            .status("生成去重、冲突、归类和清理计划（尚未执行这些操作）");
        job.db.conn.execute_batch("BEGIN IMMEDIATE")?;
        match planner::build(&mut job) {
            Ok(()) => job.db.conn.execute_batch("COMMIT")?,
            Err(error) => {
                let _ = job.db.conn.execute_batch("ROLLBACK");
                return Err(error);
            }
        }
        job.db.set("summary", &job.summary)?;
        job.db.set("status", &"ready")?;
        job.log(
            "计划",
            "",
            "",
            "待确认",
            "分析完成（只读，未改动任何文件）；去重、移动和清理等待确认",
            0,
        )?;
        #[cfg(feature = "perf-tracing")]
        crate::perf::analyze_done(
            job.summary.scanned,
            job.summary.scanned_bytes,
            job.summary.errors,
        );
        Ok(TaskResult {
            directory: directory.clone(),
            summary: job.summary.clone(),
        })
    })();
    if let Err(error) = &result {
        record_task_failure(&mut job, error);
    }
    result
}
/// 「递归解压」工具的一段式执行（X-02）：确认后连续运行到结束——扫描登记压缩包、
/// 就地解压（X-03）、成功原包按处置策略处理（X-05 默认永久删除）、失败原包移入
/// 「解压失败」子目录（X-06）。没有计划审核环节，也不生成整理计划。
pub fn extract_run(root: &Path, config: Config, context: TaskContext) -> Result<TaskResult> {
    extract_run_at(root, config, context, &config::state_dir()?, None)
}
/// 测试注入变体：独立状态目录 + 显式引擎路径（与 prepare_at 同口径，E-02 不向用户提供）。
pub fn extract_run_at(
    root: &Path,
    config: Config,
    context: TaskContext,
    state: &Path,
    engine_path: Option<&Path>,
) -> Result<TaskResult> {
    let protected = resolve_protection()?;
    extract_run_with(
        root,
        config,
        context,
        state,
        engine_path,
        protected.as_deref(),
    )
}
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "archive_extract", skip_all, err)
)]
fn extract_run_with(
    root: &Path,
    config: Config,
    context: TaskContext,
    state: &Path,
    engine_path: Option<&Path>,
    protected: Option<&Path>,
) -> Result<TaskResult> {
    let (root, protected) = preflight_root(root, &config, protected, true)?;
    let (_guard, mut job) = create_task_db(state, &root, config, context, "executing")?;
    let directory = job.db.directory.clone();
    let result = (|| {
        scan(&mut job, true, state, protected.as_deref())?;
        let count: i64 = job.db.conn.query_row(
            "SELECT COUNT(*) FROM archives WHERE state='pending'",
            [],
            |r| r.get(0),
        )?;
        if count > 0 {
            let engine = match engine_path {
                Some(path) => SevenZip::with_executable(path)?,
                None => SevenZip::from_bundle()?,
            };
            archive::extract_queued(&mut job, &engine)?;
        } else {
            job.log(
                "解压",
                "",
                "",
                "提示",
                "所选目录（按当前扫描范围）没有发现压缩包；未做任何改动",
                0,
            )?;
        }
        job.db.set("summary", &job.summary)?;
        job.log(
            "任务",
            "",
            "",
            "完成",
            &format!(
                "解压结束：成功 {} 包（成功原包及分卷已永久删除）；未完全解开并移入「{}」 {} 包",
                job.summary.archives_ok,
                archive::QUARANTINE_DIR_NAME,
                job.summary.archives_failed
            ),
            0,
        )?;
        #[cfg(feature = "perf-tracing")]
        crate::perf::extract_done(
            job.summary.scanned,
            job.summary.archives_ok,
            job.summary.archives_failed,
        );
        Ok(TaskResult {
            directory: directory.clone(),
            summary: job.summary.clone(),
        })
    })();
    if let Err(error) = &result {
        record_task_failure(&mut job, error);
    } else {
        job.db.set("status", &"finished")?;
    }
    result
}
/// 确认框用的压缩包计数（X-02）：只读快速清点，与正式扫描同一套过滤口径
/// （递归/隐藏/系统/排除规则/跳过「解压失败」/Git 整树排除/状态与程序目录剪枝，X-07）。
/// 失败即报错，不回退猜测值。
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "count_archives", skip_all)
)]
pub fn count_archives(root: &Path, config: &Config) -> Result<u64> {
    let protected = resolve_protection()?;
    count_archives_with(root, config, protected.as_deref())
}
/// S-05 受保护目录参数化变体（测试注入合成受保护目录），其余行为与 [`count_archives`] 一致。
fn count_archives_with(root: &Path, config: &Config, protected: Option<&Path>) -> Result<u64> {
    let (root, protected_prune) = preflight_root(root, config, protected, true)?;
    let excluded = rules::build_exclusions(&config.exclusions)?;
    // 与 scan 同源的特殊目录剪枝口径。状态目录此时可能尚不存在（首次运行先弹
    // 确认再建目录）：canonicalize 失败视为无重叠，不阻止清点。
    let state = fs::canonicalize(crate::config::state_dir()?).ok();
    let executable_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().and_then(|d| fs::canonicalize(d).ok()));
    let (state_prefix, exe_prefix, root_under_special) =
        special_prefixes(&root, state.as_deref(), executable_dir.as_deref());
    if root_under_special {
        // 选定根位于状态目录/程序目录内：扫描将整树剪枝，清点必须同为 0。
        return Ok(0);
    }
    // S-05：根包含 Windows 系统目录时该子树整树剪枝，清点与扫描同一口径。
    let protected_prefix = protected_prune
        .as_ref()
        .and_then(|dir| fsutil::relative_string(&root, dir).ok());
    // 剪枝口径与扫描共用同一个实现（避免两处各自漂移成不同范围）。
    let scope_filter = ScopeFilter {
        excluded: &excluded,
        include_hidden: config.include_hidden,
        include_system: config.include_system,
        quarantine: Some(archive::QUARANTINE_DIR_NAME),
        state_prefix,
        exe_prefix,
        protected_prefix,
        root_under_special,
    };
    // X-10：缺主包的老式族尾卷组按「残缺但可归组的卷集计一包」参与清点。
    // 归组依赖同目录兄弟关系，先按目录收集文件名再统一计数，与扫描的兄弟判定同源。
    let mut names_by_dir: HashMap<PathBuf, Vec<String>> = HashMap::new();
    for entry in walkdir::WalkDir::new(&root)
        .follow_links(false)
        .min_depth(1)
        .max_depth(if config.recursive { usize::MAX } else { 1 })
        .into_iter()
        .filter_entry(|entry| {
            let Ok(meta) = fs::symlink_metadata(entry.path()) else {
                // 与正式扫描一致：元数据不可读的条目按跳过处理，不计入清点。
                return false;
            };
            // H-06：目录直接含 .git（目录或文件）时整树排除，识别后不遍历内部。
            // 边界判定失败时同样剪枝：内容未知的目录不得计入清点。
            if entry.file_type().is_dir() && fsutil::is_git_root(entry.path()).unwrap_or(true) {
                return false;
            }
            let Ok(rel) = fsutil::relative_string(&root, entry.path()) else {
                return false;
            };
            // 选定根目录自身不参与隐藏/系统/名称等判定——扫描同口径：筛选只作用于
            // 根目录的子项，隐藏的选定根目录不会把整棵树剪掉（否则确认框报 0，
            // 正式扫描却能找到包）。
            if rel.is_empty() {
                return true;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            !scope_filter.prunes(&rel, &name, &meta)
        })
    {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str() else {
            continue;
        };
        let parent = entry
            .path()
            .parent()
            .map_or_else(|| root.clone(), Path::to_path_buf);
        names_by_dir
            .entry(parent)
            .or_default()
            .push(name.to_lowercase());
    }
    let mut count = 0u64;
    for names in names_by_dir.values() {
        count += names
            .iter()
            .filter(|name| rules::archive_name(name))
            .count() as u64;
        count += rules::count_tail_only_old_style_groups(names);
    }
    Ok(count)
}
/// IO 阶段（目录枚举、句柄查询、删除）的线程池：这些都是元数据级小操作、
/// 瓶颈在系统调用延迟而非 CPU；按磁盘延迟预算取小池（与 hash_workers 同思路，
/// 不按 CPU 核数打满磁盘）。
fn io_pool() -> Result<rayon::ThreadPool> {
    let threads = std::thread::available_parallelism().map_or(4, usize::from);
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads.min(8))
        .thread_name(|i| format!("jch-io-{i}"))
        .build()
        .map_err(anyhow::Error::new)
        .context("无法创建 IO 线程池")
}
/// 并行扫描收集到的一条子项：目录或文件。同一父目录的子项由单线程按枚举序追加，
/// 汇总后按父目录分组即可还原旧 walkdir 的深度先序（目录先于其子树、
/// 同目录内保持文件系统枚举顺序），插入顺序与旧实现一致。
struct ScanChild {
    parent: String,
    name: String,
    kind: ScanKind,
}
enum ScanKind {
    Dir { lower: String },
    File { normal: String, snapshot: Snapshot },
}
/// 扫描期需要主线程补记的错误日志（工作线程不能触碰任务库连接）。
struct ScanNote {
    path: String,
    message: String,
}
#[derive(Default)]
struct ScanSink {
    children: Vec<ScanChild>,
    notes: Vec<ScanNote>,
    /// 盘上可见但未入盘点的内容所在的目录（被过滤条目、枚举/读取失败）：
    /// 空目录规划据此（planner 内向上传播）拒绝把该目录及其祖先当作空目录，
    /// 替代旧实现对每个候选目录重新走盘核对（has_unscanned_content）。
    taint: HashSet<String>,
    /// 整树排除的 Git 目录（目录直接含 .git 的 rel）：只用于界面提示与 git_roots 表。
    git_skips: Vec<String>,
    /// S-01/F02：位于本次处理范围内、因保留名剪枝的 .jchtools-link-* 普通文件
    /// （疑似上次执行崩溃残留的硬链接临时文件）。只供目录整理流程登记进任务库、
    /// 执行开始时按既有谓词清理；扫描本身仍然只读。
    link_residues: Vec<String>,
}
fn lock_sink(sink: &Mutex<ScanSink>) -> std::sync::MutexGuard<'_, ScanSink> {
    // 锁中毒只可能因持锁线程 panic；本模块持锁期间不 panic，恢复数据是安全回退。
    sink.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
/// 处理范围剪枝口径：扫描、确认框清点（X-02）与整理收尾实空清理共用同一套判定，
/// 三处不会各自漂移出不同范围。Git 整树排除（H-06）由调用方在枚举目录前单独判定。
struct ScopeFilter<'a> {
    excluded: &'a globset::GlobSet,
    include_hidden: bool,
    include_system: bool,
    /// 「解压失败」暂存区：扫描与清点整树跳过（C-09/X-07）；收尾实空清理不跳过
    /// （该目录实际为空时仍按 H-05 清理，但其内容一律不碰）。
    quarantine: Option<&'a str>,
    /// 状态目录/程序目录位于选定根内的相对路径前缀（剪枝其子树）。
    state_prefix: Option<String>,
    exe_prefix: Option<String>,
    /// S-05：根包含 Windows 系统目录时该子树的相对路径前缀（整树剪枝并提示）。
    protected_prefix: Option<String>,
    /// 选定根本身位于状态目录或程序目录内：整棵树按旧口径全部剪枝。
    root_under_special: bool,
}
impl ScopeFilter<'_> {
    fn prunes(&self, child_rel: &str, name: &str, metadata: &fs::Metadata) -> bool {
        // .jchtools-link-* 是本工具崩溃残留的保留名：一律剪枝（不入盘点）；
        // 「是否同时还在本次处理范围内」由 walk_dir 结合 prunes_other 判定后登记。
        if name.starts_with(".jchtools-link-") {
            return true;
        }
        self.prunes_other(child_rel, name, metadata)
    }
    /// 除 .jchtools-link-* 名称规则外的全部剪枝判定。
    /// 供 walk_dir 判断一个 .jchtools-link-* 条目是否「仅在保留名口径下被剪枝」
    /// （= 位于本次处理范围内，可登记为疑似残留供执行前清理）。
    /// `name` 仅非 Windows 的点开头隐藏判定使用（P-07：产品仅在 Windows 构建）。
    #[cfg_attr(windows, allow(unused_variables))]
    fn prunes_other(&self, child_rel: &str, name: &str, metadata: &fs::Metadata) -> bool {
        if self.root_under_special
            || fsutil::is_link(metadata)
            || child_rel == ".jchtools-work"
            || child_rel.starts_with(".jchtools-work/")
        {
            return true;
        }
        for prefix in [
            self.state_prefix.as_deref(),
            self.exe_prefix.as_deref(),
            self.protected_prefix.as_deref(),
        ] {
            if prefix.is_some_and(|p| child_rel == p || child_rel.starts_with(&format!("{p}/"))) {
                return true;
            }
        }
        if let Some(quarantine) = self.quarantine {
            // C-09/X-07：「解压失败」容器按任意层级的路径组件名整树剪枝（不区分大小写）——
            // 子目录单独解压会按 X-06 在子目录里留下隔离容器，整理父目录时同样保留，
            // 其内容不参与去重、归类、清理，也不计入确认框清点。
            // 容器名本身没有大小写变体，ASCII 折叠与 archive.rs 的组件名判定同口径。
            if child_rel
                .split('/')
                .any(|component| component.eq_ignore_ascii_case(quarantine))
            {
                return true;
            }
        }
        if self.excluded.is_match(child_rel) || self.excluded.is_match(format!("{child_rel}/")) {
            return true;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if !self.include_hidden && metadata.file_attributes() & 2 != 0 {
                return true;
            }
            if !self.include_system && metadata.file_attributes() & 4 != 0 {
                return true;
            }
        }
        #[cfg(not(windows))]
        if !self.include_hidden && name.starts_with('.') {
            return true;
        }
        false
    }
}
struct WalkCtx<'a> {
    control: Arc<Control>,
    scope: &'a ScopeFilter<'a>,
    recursive: bool,
    sink: &'a Mutex<ScanSink>,
}
/// 单个目录的枚举与登记（在 IO 池线程上运行）。过滤口径与旧 walkdir filter_entry
/// 一致；子目录下钻时首个内联、其余 spawn。内联深度设上限：极深树上避免
/// 任务在偷取执行时栈随树深增长；超限的子目录全部走 spawn（在全新栈帧上执行）。
fn walk_dir<'a>(
    scope: &rayon::Scope<'a>,
    ctx: &'a WalkCtx<'a>,
    dir: &Path,
    rel: String,
    inline: u32,
) {
    if ctx.control.checkpoint().is_err() {
        // 取消：主线程在汇合后统一以取消错误收尾，这里不再记账。
        return;
    }
    // H-06：目录直接含 .git（目录或文件）即整树排除——在枚举子项之前判定，
    // 识别后不继续遍历内部，也不登记其中任何内容；该目录与其全部祖先记污点，
    // 避免空目录规划顺着祖先间接改动 Git 树。
    let git_boundary = fsutil::is_git_root(dir);
    if matches!(git_boundary, Ok(true)) {
        let mut sink = lock_sink(ctx.sink);
        sink.git_skips.push(rel);
        // Git 树自身不入盘点（不进 directories/files），其祖先的空目录保护由
        // planner 按 git_roots.staying 在空目录规划时计算（C-14 移走后允许变空）。
        return;
    }
    if let Err(error) = git_boundary {
        // 边界无法判定：不得继续遍历该目录（内容未知），如实记录并整树跳过。
        let mut sink = lock_sink(ctx.sink);
        sink.notes.push(ScanNote {
            path: dir.display().to_string(),
            message: format!("无法判定是否含 .git，已整树跳过：{error:#}"),
        });
        sink.taint.insert(rel.clone());
        return;
    }
    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        Err(error) => {
            let mut sink = lock_sink(ctx.sink);
            sink.notes.push(ScanNote {
                path: dir.display().to_string(),
                message: format!("{error:#}"),
            });
            // 本目录未能盘点：其内容未知，自身记污点。
            sink.taint.insert(rel);
            return;
        }
    };
    let mut children: Vec<ScanChild> = Vec::new();
    let mut subdirs: Vec<(PathBuf, String)> = Vec::new();
    let mut parent_tainted = false;
    for entry in read {
        if ctx.control.checkpoint().is_err() {
            return;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                let mut sink = lock_sink(ctx.sink);
                sink.notes.push(ScanNote {
                    path: dir.display().to_string(),
                    message: format!("{error:#}"),
                });
                parent_tainted = true;
                continue;
            }
        };
        let Ok(name) = entry.file_name().into_string() else {
            let mut sink = lock_sink(ctx.sink);
            sink.notes.push(ScanNote {
                path: entry.path().display().to_string(),
                message: "文件名不能无损表示为 UTF-8，已跳过".into(),
            });
            parent_tainted = true;
            continue;
        };
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                let mut sink = lock_sink(ctx.sink);
                sink.notes.push(ScanNote {
                    path: entry.path().display().to_string(),
                    message: format!("{error:#}"),
                });
                parent_tainted = true;
                continue;
            }
        };
        let child_rel = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        if ctx.scope.prunes(&child_rel, &name, &metadata) {
            // S-01/F02：.jchtools-link-* 命名剪枝、且未命中任何其他范围规则的普通文件
            // 是「位于本次处理范围内」的疑似崩溃残留：登记供执行前清理与界面明示，
            // 删除范围不再由执行期的独立全盘扫描决定。
            if metadata.is_file()
                && name.starts_with(".jchtools-link-")
                && !ctx.scope.prunes_other(&child_rel, &name, &metadata)
            {
                lock_sink(ctx.sink).link_residues.push(child_rel);
            }
            // 被剪枝的条目盘上仍存在：其父目录不得按空目录处理。
            parent_tainted = true;
            continue;
        }
        if metadata.is_dir() {
            // H-06 不受递归开关影响：递归关闭时不再下钻，子目录自己的边界判定没有
            // 机会执行——直接含 .git 的目录同样不得登记（登记会让空目录规划把它当作
            // 空目录，等于间接处理该树），这里补一次判定。
            if !ctx.recursive {
                let git_boundary = fsutil::is_git_root(&entry.path());
                if matches!(git_boundary, Ok(true)) {
                    let mut sink = lock_sink(ctx.sink);
                    sink.git_skips.push(child_rel);
                    continue;
                }
                if let Err(error) = git_boundary {
                    let mut sink = lock_sink(ctx.sink);
                    sink.notes.push(ScanNote {
                        path: entry.path().display().to_string(),
                        message: format!("无法判定是否含 .git，已整树跳过：{error:#}"),
                    });
                    sink.taint.insert(child_rel);
                    continue;
                }
            }
            children.push(ScanChild {
                parent: rel.clone(),
                name,
                kind: ScanKind::Dir {
                    lower: String::new(),
                },
            });
            if ctx.recursive {
                subdirs.push((entry.path(), child_rel));
            }
        } else if metadata.file_type().is_file() {
            match fsutil::snapshot_with(&entry.path(), &metadata) {
                Ok(snapshot) => {
                    let normal = rules::normal_key(&name);
                    children.push(ScanChild {
                        parent: rel.clone(),
                        name,
                        kind: ScanKind::File { normal, snapshot },
                    });
                    ctx.control.scanned.fetch_add(1, Ordering::Relaxed);
                }
                Err(error) => {
                    let mut sink = lock_sink(ctx.sink);
                    sink.notes.push(ScanNote {
                        path: entry.path().display().to_string(),
                        message: format!("{error:#}"),
                    });
                    parent_tainted = true;
                }
            }
        } else {
            // 既非目录也非普通文件（旧实现静默不入库 → 盘上有、库里无，等价于污点）。
            parent_tainted = true;
        }
    }
    {
        let mut sink = lock_sink(ctx.sink);
        // 目录名小写折叠在这里补齐（供 directories.name 的 Windows 折叠匹配）；
        // 文件名的小写折叠与 UTF-16 计数由 db.insert_file 统一完成。
        for child in &mut children {
            if let ScanKind::Dir { lower } = &mut child.kind {
                *lower = child.name.to_lowercase();
            }
        }
        sink.children.append(&mut children);
        if parent_tainted {
            sink.taint.insert(rel.clone());
        }
    }
    let mut iter = subdirs.into_iter();
    if let Some((first_path, first_rel)) = iter.next() {
        for (path, child_rel) in iter {
            let ctx_ref = ctx;
            scope.spawn(move |scope| walk_dir(scope, ctx_ref, &path, child_rel, 0));
        }
        if inline < 32 {
            walk_dir(scope, ctx, &first_path, first_rel, inline + 1);
        } else {
            scope.spawn(move |scope| walk_dir(scope, ctx, &first_path, first_rel, 0));
        }
    }
}
/// 深度先序回放：按父目录分组后的子项以枚举序递归发射，与旧 walkdir 的产出顺序一致。
fn scan_emit(
    job: &mut Job,
    by_parent: &HashMap<String, Vec<ScanChild>>,
    enqueue: bool,
    parent: &str,
    depth: i64,
    count: &mut u64,
) -> Result<()> {
    let Some(children) = by_parent.get(parent) else {
        return Ok(());
    };
    for child in children {
        *count += 1;
        if (*count).is_multiple_of(2048) {
            job.db.conn.execute_batch("COMMIT; BEGIN IMMEDIATE;")?;
        }
        job.context.control.checkpoint()?;
        let rel = if parent.is_empty() {
            child.name.clone()
        } else {
            format!("{parent}/{}", child.name)
        };
        match &child.kind {
            ScanKind::Dir { lower } => {
                job.db.insert_dir(&rel, lower, depth)?;
                scan_emit(job, by_parent, enqueue, &rel, depth + 1, count)?;
            }
            ScanKind::File { normal, snapshot } => {
                job.db.insert_file(&rel, &child.name, normal, snapshot)?;
                job.summary.scanned += 1;
                job.summary.scanned_bytes = job.summary.scanned_bytes.saturating_add(snapshot.size);
                if enqueue && rules::archive_name(&child.name) {
                    // 入队失败按旧口径计错误并跳过该包，不中断整个扫描（文件行已入库）。
                    if let Err(error) = archive::enqueue(job, &job.root.join(&rel), 0) {
                        job.summary.errors += 1;
                        job.log("扫描", &rel, "", "跳过", &format!("{error:#}"), 0)?;
                    }
                }
            }
        }
    }
    if enqueue {
        enqueue_old_style_tail_groups(job, parent, children)?;
    }
    Ok(())
}
/// X-10：老式 zip/rar 族只发现尾卷、没有主包时仍归组——按「残缺但可归组的卷集
/// 计一包」把该组首个尾卷作为代表入队，解压阶段按缺主包整组失败并隔离（X-06）。
/// 主包（主干.zip/主干.rar）在场的尾卷属于其卷集，不在此入队，与
/// rules::archive_name 的入口口径一致；同主干同族只入队一次。
fn enqueue_old_style_tail_groups(
    job: &mut Job,
    parent: &str,
    children: &[ScanChild],
) -> Result<()> {
    let siblings: HashSet<String> = children
        .iter()
        .filter(|child| matches!(child.kind, ScanKind::File { .. }))
        .map(|child| child.name.to_lowercase())
        .collect();
    let mut seen: HashSet<(String, &'static str)> = HashSet::new();
    for child in children {
        let ScanKind::File { .. } = &child.kind else {
            continue;
        };
        let lower = child.name.to_lowercase();
        let Some(tail) = rules::old_style_tail(&lower) else {
            continue;
        };
        if siblings.contains(format!("{}.{}", tail.stem, tail.main_ext).as_str())
            || !seen.insert((tail.stem.to_string(), tail.main_ext))
        {
            continue;
        }
        let rel = if parent.is_empty() {
            child.name.clone()
        } else {
            format!("{parent}/{}", child.name)
        };
        // 与 archive_name 入队同口径：入队失败计错误并跳过，不中断整个扫描。
        if let Err(error) = archive::enqueue(job, &job.root.join(&rel), 0) {
            job.summary.errors += 1;
            job.log("扫描", &rel, "", "跳过", &format!("{error:#}"), 0)?;
        }
    }
    Ok(())
}
/// 状态目录/程序目录的剪枝口径换算：把「选定根内的特殊目录」折算成相对前缀
/// （条目都以相对路径比较，免去逐条目拼绝对路径），并给出「选定根本身位于特殊
/// 目录内（含重合）」标志——此时整棵树按旧口径全部剪枝。scan 与 count_archives
/// 共用本函数，保证确认框清点（X-02）与正式扫描是同一套过滤口径。
fn special_prefixes(
    root: &Path,
    state: Option<&Path>,
    executable_dir: Option<&Path>,
) -> (Option<String>, Option<String>, bool) {
    let prefix_under_root = |special: &Path| -> Option<String> {
        if !special.starts_with(root) {
            return None;
        }
        match fsutil::relative_string(root, special) {
            Ok(rel) if rel.is_empty() => None, // 根自身即特殊目录：走整树剪枝
            Ok(rel) => Some(rel),
            Err(_) => None,
        }
    };
    (
        state.and_then(prefix_under_root),
        executable_dir.and_then(prefix_under_root),
        state.is_some_and(|dir| root.starts_with(dir))
            || executable_dir.is_some_and(|dir| root.starts_with(dir)),
    )
}
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "scan", skip_all)
)]
/// 预览格式化：取前 3 项以「、」连接，超出 3 项时追加「 等」（扫描日志共用）。
fn preview3(items: &[String]) -> String {
    let shown = items.iter().take(3).cloned().collect::<Vec<_>>().join("、");
    let more = if items.len() > 3 { " 等" } else { "" };
    format!("{shown}{more}")
}
fn scan(job: &mut Job, enqueue: bool, state: &Path, protected: Option<&Path>) -> Result<()> {
    job.context.status(if enqueue {
        "扫描所选目录，登记待解压的压缩包"
    } else {
        "扫描所选目录（只读分析）"
    });
    job.db
        .conn
        .execute_batch("DELETE FROM files; DELETE FROM directories;")?;
    job.summary.scanned = 0;
    job.summary.scanned_bytes = 0;
    job.context.control.scanned.store(0, Ordering::Relaxed);
    let root = job.root.clone();
    let config = job.config.clone();
    // S-05：根包含 Windows 系统目录时整树剪枝该子树（相对前缀与状态/程序目录同口径）。
    let protected_prefix = protected.and_then(|dir| fsutil::relative_string(&root, dir).ok());
    let excluded = rules::build_exclusions(&config.exclusions)?;
    let state = fs::canonicalize(state)?;
    let executable_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().and_then(|d| fs::canonicalize(d).ok()));
    if let (Some(directory), Ok(target)) = (&executable_dir, fs::canonicalize(&root)) {
        if target.starts_with(directory) {
            job.log("扫描","","","提示",&format!("所选目录位于程序自身目录（{}）内；为避免误处理程序文件，扫描会跳过这个目录里的条目",directory.display()),0)?;
        }
    }
    // X-07 / C-09：「解压失败」是「递归解压」的失败暂存区，两个工具默认都不碰它；
    // 目录存在时明确提示（C-09），没有则不打扰。
    let quarantine = job.root.join(archive::QUARANTINE_DIR_NAME);
    if quarantine.is_dir() {
        job.log(
            "扫描",
            "",
            "",
            "提示",
            &format!(
                "已跳过「{}」子目录：失败暂存区不参与本次处理（补救后把包移出即可重新处理）",
                archive::QUARANTINE_DIR_NAME
            ),
            0,
        )?;
    }
    // S-04：任何缩小处理范围的选择都必须在日志里明确提示，避免静默漏处理。
    let mut reduced: Vec<String> = Vec::new();
    if !config.recursive {
        reduced.push("递归已关闭（只处理所选目录的第一层）".into());
    }
    if !config.include_hidden {
        reduced.push("未包含隐藏属性资料".into());
    }
    if !config.include_system {
        reduced.push("未包含系统属性资料".into());
    }
    if protected_prefix.is_some() {
        // S-05 第二句：选择包含系统目录的更高层根时整树排除该系统目录并提示。
        reduced.push(
            "已排除 Windows 系统目录（系统目录及其全部内容不参与本次处理，也不会被改动）".into(),
        );
    }
    // 规则的完整文本可能很长（默认列表就跨多行），这里只报条数，规则本身在界面上可查。
    let exclusion_rules = config
        .exclusions
        .split(';')
        .filter(|part| !part.trim().is_empty())
        .count();
    if exclusion_rules > 0 {
        reduced.push(format!("排除规则生效（{exclusion_rules} 条）"));
    }
    if !reduced.is_empty() {
        job.log(
            "扫描",
            "",
            "",
            "提示",
            &format!(
                "范围提示：{}；这些资料不参与本次处理，也不会被改动",
                reduced.join("；")
            ),
            0,
        )?;
    }
    // 状态目录/程序目录的剪枝口径与 count_archives、收尾实空清理共用；选定根位于
    // 它们内部（含重合）时，整棵树按旧口径全部剪枝。
    let (state_prefix, exe_prefix, root_under_special) =
        special_prefixes(&root, Some(&state), executable_dir.as_deref());
    let scope_filter = ScopeFilter {
        excluded: &excluded,
        include_hidden: config.include_hidden,
        include_system: config.include_system,
        quarantine: Some(archive::QUARANTINE_DIR_NAME),
        state_prefix,
        exe_prefix,
        protected_prefix,
        root_under_special,
    };
    let sink = Mutex::new(ScanSink::default());
    let ctx = WalkCtx {
        control: job.context.control.clone(),
        scope: &scope_filter,
        recursive: config.recursive,
        sink: &sink,
    };
    let pool = io_pool()?;
    pool.install(|| {
        rayon::scope(|scope| {
            scope.spawn(|scope| walk_dir(scope, &ctx, &root, String::new(), 0));
        });
    });
    // 用户取消：不做入库与后续阶段（旧实现同样以取消错误中止扫描）。
    job.context.control.checkpoint()?;
    let mut sink = sink
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for note in &sink.notes {
        job.summary.errors += 1;
        job.log("扫描", &note.path, "", "跳过", &note.message, 0)?;
    }
    let mut git_roots = std::mem::take(&mut sink.git_skips);
    if !git_roots.is_empty() {
        // H-06：Git 整树保护必须明确提示处置结果（界面蓝条 + 日志），不静默跳过。
        // 解压流程里 Git 项目整树排除；目录整理流程里按 C-14 固定行为整体移入「Git项目集合」，
        // 两者都不读取内容、不进入树内处理，但「处置结果」必须各自说清，不能都说成「跳过」。
        git_roots.sort();
        git_roots.dedup();
        let shown = preview3(&git_roots);
        let (log_message, notice) = if enqueue {
            (
                format!(
                    "已跳过 {} 个 Git 目录及其全部内容（含 .git 的目录整树排除：不读取、不归类、不改名、不删除，不受隐藏/递归/清理开关影响）：{shown}",
                    git_roots.len()
                ),
                format!(
                    "已跳过 {} 个 Git 目录树（含全部后代，未读取内容）",
                    git_roots.len()
                ),
            )
        } else {
            (
                format!(
                    "已识别 {} 个 Git 项目（含 .git 的目录整树保护：不读取内容、不进入树内改名/去重/清理；整树移入本次所选根下的「Git项目集合」，已在集合内的不再移动）：{shown}",
                    git_roots.len()
                ),
                format!(
                    "已识别 {} 个 Git 项目（整树移入「Git项目集合」，未读取内容）",
                    git_roots.len()
                ),
            )
        };
        job.log("扫描", "", "", "提示", &log_message, 0)?;
        job.context.emit(Event::Notice(notice));
    }
    // S-01/F02：范围内疑似崩溃残留（.jchtools-link-*，见 walk_dir 登记）随任务库
    // 记录，供执行开始时按既有谓词清理；分析日志必须明示，不得静默处置。
    // 残留清理只属于目录整理的执行段（apply），解压流程不清理、不登记。
    if !enqueue {
        let mut residues = std::mem::take(&mut sink.link_residues);
        residues.sort();
        residues.dedup();
        if !residues.is_empty() {
            job.db.set("link_residues", &residues)?;
            let shown = preview3(&residues);
            job.log(
                "扫描",
                "",
                "",
                "提示",
                &format!(
                    "发现 {} 个疑似上次执行崩溃残留的硬链接临时文件（.jchtools-link-*，均在本次处理范围内）：{shown}；执行开始时将清理其中「修改超过 24 小时且仍是硬链接（链接数 ≥ 2，内容另有保留文件持有）」的项",
                    residues.len()
                ),
                0,
            )?;
            job.context.emit(Event::Notice(format!(
                "发现 {} 个疑似崩溃残留的硬链接临时文件，执行开始时将清理",
                residues.len()
            )));
        }
    }
    // 汇总入库：按父目录分组还原深度先序；污点表供 planner 的空目录规划排除，
    // git_roots 供 planner 拒绝把内容归入 Git 工作树（H-06：不归类）。
    let mut by_parent: HashMap<String, Vec<ScanChild>> = HashMap::new();
    for child in sink.children {
        by_parent
            .entry(child.parent.clone())
            .or_default()
            .push(child);
    }
    let mut taint: Vec<String> = sink.taint.into_iter().collect();
    taint.sort();
    job.db.conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        job.db.conn.execute_batch(
            "DROP TABLE IF EXISTS scan_taint; CREATE TEMP TABLE scan_taint(rel TEXT PRIMARY KEY);
             DROP TABLE IF EXISTS git_roots; CREATE TEMP TABLE git_roots(rel TEXT PRIMARY KEY)",
        )?;
        for rel in &taint {
            job.db.remember_taint(rel)?;
        }
        {
            let mut statement = job
                .db
                .conn
                .prepare_cached("INSERT OR IGNORE INTO git_roots(rel) VALUES(?1)")?;
            for rel in &git_roots {
                statement.execute(params![rel])?;
            }
        }
        let mut count = 0u64;
        scan_emit(job, &by_parent, enqueue, "", 1, &mut count)?;
        Ok::<_, anyhow::Error>(())
    })();
    match result {
        Ok(()) => job.db.conn.execute_batch("COMMIT")?,
        Err(error) => {
            let _ = job.db.conn.execute_batch("ROLLBACK");
            return Err(error);
        }
    }
    Ok(())
}
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "hash", skip_all)
)]
fn hash_candidates(job: &mut Job) -> Result<()> {
    if !job.config.dedup_same_name && !job.config.dedup_copy_names && !job.config.dedup_other_names
    {
        return Ok(());
    }
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(job.config.hash_workers)
        .thread_name(|i| format!("organizer-hash-{i}"))
        .build()?;
    job.context.status("计算候选文件的完整 Hash");
    job.db.conn.execute_batch("DROP TABLE IF EXISTS hash_candidates; CREATE TEMP TABLE hash_candidates(id INTEGER PRIMARY KEY);")?;
    // C-12：候选范围只依据扫描期已获得的信息（大小与名称关系），完整哈希在一次遍历内
    // 完成，不设预哈希/完整哈希两阶段。内容相同必然同尺寸，所以「同尺寸不止一个」
    // 永远是安全下界；默认（不同名去重关闭）还可再收窄——去重配对只可能发生在
    // 「名称完全相同」（planner 比较 files.name）或「归一化名称完全相同」
    // （planner 比较 files.normal）两组之间，取两种分组并集仍是安全下界。
    if job.config.dedup_other_names {
        job.db.conn.execute("INSERT OR IGNORE INTO hash_candidates SELECT id FROM files WHERE active=1 AND size IN (SELECT size FROM files WHERE active=1 GROUP BY size HAVING COUNT(*)>1)",[])?;
    } else {
        for key in ["name", "normal"] {
            job.db.conn.execute(&format!("INSERT OR IGNORE INTO hash_candidates SELECT id FROM files WHERE active=1 AND (size,{key}) IN (SELECT size,{key} FROM files WHERE active=1 GROUP BY size,{key} HAVING COUNT(*)>1)"),[])?;
        }
    }
    // C-13：跨运行哈希缓存放在状态目录根（每次 prepare 都新建任务库，缓存必须
    // 跨任务存活）。打开失败只降级为「无缓存、全部重算」，不得阻断分析。
    let mut cache = match job.db.get::<String>("state_dir") {
        Ok(dir) => match crate::hash_cache::HashCache::open(Path::new(&dir)) {
            Ok(cache) => Some(cache),
            Err(error) => {
                job.log(
                    "Hash",
                    "",
                    "",
                    "提示",
                    &format!("哈希缓存不可用，本次全部重新计算（{error:#}）"),
                    0,
                )?;
                None
            }
        },
        Err(_) => None,
    };
    let mut reused_total: u64 = 0;
    let mut cursor = 0;
    // 分页 SQL 整个哈希阶段逐字不变：循环外构造一次，配合 db::files 的 prepare_cached
    // 让每页都命中语句缓存（不再逐页 format! 与解析）。
    let sql = format!(
        "SELECT {} FROM hash_candidates AS c CROSS JOIN files AS f ON f.id=c.id \
         WHERE c.id>?1 AND f.active=1 ORDER BY c.id LIMIT ?2",
        crate::db::file_columns_qualified("f")
    );
    loop {
        job.context.control.checkpoint()?;
        // 候选表按主键游标推进，并用 CROSS JOIN 固定 hash_candidates 为外层扫描表：
        // 旧的 `id IN (SELECT id FROM hash_candidates)` 会让每次分页都重扫整个候选集合
        // （实测每次调用成本随游标位置线性增长，累计平方级）。取数批量与哈希线程数解耦，
        // 避免「调线程数」同时改变两个量。
        let batch = job.db.files(
            &sql,
            params![cursor, crate::convert::usize_as_i64(HASH_BATCH)],
        )?;
        let Some(last) = batch.last() else {
            break;
        };
        cursor = last.id;
        let root = &job.root;
        let control = &job.context.control;
        // C-13：先查跨运行缓存，命中的候选不再读取文件内容。缓存故障一次性
        // 降级为「其后全部重算」——缓存只是加速，正确性不依赖它。
        // resume 是首个未消化的候选下标：缓存不可用/读取失败时从它起全部转入重算。
        let mut reused: Vec<(i64, String)> = Vec::new();
        let mut compute: Vec<&_> = Vec::new();
        let mut resume = batch.len();
        for (index, file) in batch.iter().enumerate() {
            let Some(cache_conn) = cache.as_ref() else {
                resume = index;
                break;
            };
            let Ok(size) = i64::try_from(file.snapshot.size) else {
                compute.push(file);
                continue;
            };
            if crate::hash_cache::identity_is_degenerate(&file.snapshot.identity) {
                compute.push(file);
                continue;
            }
            match cache_conn.lookup(&file.snapshot.identity, size, file.snapshot.modified_ns) {
                Ok(Some(hash)) => reused.push((file.id, hash)),
                Ok(None) => compute.push(file),
                Err(error) => {
                    cache = None;
                    job.log(
                        "Hash",
                        "",
                        "",
                        "提示",
                        &format!("哈希缓存读取失败，其后全部重新计算（{error:#}）"),
                        0,
                    )?;
                    resume = index;
                    break;
                }
            }
        }
        compute.extend(batch[resume..].iter());
        reused_total = reused_total.saturating_add(u64::try_from(reused.len()).unwrap_or(0));
        let results: Vec<_> = pool.install(|| {
            compute
                .par_iter()
                .map(|file| {
                    let result = (|| {
                        let path = fsutil::safe_join(root, &file.rel)?;
                        hashing::full_hash(&path, control)
                    })();
                    (file.id, file.rel.clone(), result)
                })
                .collect()
        });
        job.db.conn.execute_batch("BEGIN IMMEDIATE")?;
        let update = (|| {
            // 用户取消时并行哈希的每个成员都会返回取消错误；先在这里拦截，
            // 否则每个候选都被计成 error 并逐条写日志，取消任务的错误数虚高。
            job.context.control.checkpoint()?;
            // C-13：缓存命中的哈希与刚算出的走完全相同的落库路径，planner 无感知。
            for (id, hash) in &reused {
                job.db.set_file_hash(*id, hash)?;
            }
            for (id, rel, result) in &results {
                match result {
                    Ok(hash) => {
                        job.db.set_file_hash(*id, hash)?;
                    }
                    Err(error) => {
                        job.summary.errors += 1;
                        job.db.deactivate_file_id(*id)?;
                        job.log("Hash", rel, "", "跳过", &format!("{error:#}"), 0)?;
                    }
                }
            }
            Ok::<_, anyhow::Error>(())
        })();
        match update {
            Ok(()) => job.db.conn.execute_batch("COMMIT")?,
            Err(error) => {
                let _ = job.db.conn.execute_batch("ROLLBACK");
                return Err(error);
            }
        }
        // C-13：把本次新算出的哈希写入跨运行缓存；写失败同样只降级、不失败。
        if let Some(cache_conn) = cache.as_ref() {
            let by_id: HashMap<i64, &_> = batch.iter().map(|file| (file.id, file)).collect();
            let entries: Vec<(String, i64, i64, String)> = results
                .iter()
                .filter_map(|(id, _, result)| {
                    let hash = result.as_ref().ok()?;
                    let file = by_id.get(id)?;
                    let size = i64::try_from(file.snapshot.size).ok()?;
                    Some((
                        file.snapshot.identity.clone(),
                        size,
                        file.snapshot.modified_ns,
                        hash.clone(),
                    ))
                })
                .collect();
            if let Err(error) = cache_conn.store(&entries) {
                cache = None;
                job.log(
                    "Hash",
                    "",
                    "",
                    "提示",
                    &format!("哈希缓存写入失败，其后不再复用（{error:#}）"),
                    0,
                )?;
            }
        }
    }
    if reused_total > 0 {
        job.log(
            "Hash",
            "",
            "",
            "提示",
            &format!(
                "复用上次运行的哈希 {reused_total} 个（文件标识/大小/修改时间未变，未重读内容）"
            ),
            0,
        )?;
    }
    Ok(())
}
/// apply 的锁目录决策：优先 prepare 记录的全局锁目录；记录缺失或失效时，仅当任务
/// 目录仍处于「…/tasks/<任务>」布局才退回上一级推导（搬到别的机器后放回新机器的
/// tasks/ 布局仍可执行），其余位置一律拒绝——不得在陌生位置创建锁文件。
/// 布局判定成立时 derived 必然已存在（它包含 tasks/ 这一级），无需再查 is_dir。
fn lock_dir_for(directory: &Path, recorded: Option<&Path>) -> Result<PathBuf> {
    if let Some(recorded) = recorded.filter(|p| p.is_dir()) {
        return Ok(recorded.to_path_buf());
    }
    let derived = directory
        .parent()
        .and_then(Path::parent)
        .context("任务目录结构无效")?;
    // 盘根（X:\）与 UNC 共享根（\\server\share\）不是状态目录：在那里落锁会留下
    // 陌生位置的锁文件。判定：derived 不含任何 Normal 组件（盘根/共享根只有
    // Prefix+RootDir；UNC 前缀不可拆分，不能用祖先层数统计，否则会误拒共享盘上
    // 正常深度的状态目录）。
    let root_level = !derived
        .components()
        .any(|c| matches!(c, std::path::Component::Normal(_)));
    anyhow::ensure!(
        !root_level&&directory.parent().and_then(Path::file_name).is_some_and(|name|name=="tasks"),
        "任务库缺少有效的全局锁目录记录且任务目录已离开原位置；为避免互斥失效，请重新「解压与分析」后再执行");
    Ok(derived.to_path_buf())
}
/// 执行期实空复查：目录消失、类型改变或已非空时返回 false，不得按空目录清理
/// （计划与执行之间的盘面变化以盘面实况为准）。执行段与串行路径共用同一谓词。
fn dir_recheck_empty(path: &Path) -> Result<bool> {
    Ok(path.try_exists()? && path.is_dir() && fs::read_dir(path)?.next().is_none())
}
/// C-01：取消勾选后仅用既有分析资料重算受影响的计划。被取消且未执行的
/// Move/Delete 项原位置视为占用，被取消的空目录清理行自身同样占位（子目录
/// 不删，父目录不再变空）；selected=1 的空目录清理行若其目录（含子树）内
/// 存在占位项，依赖失效、转为未勾选——禁止删除依赖于已取消保留者移动的
/// 其他项。用户已取消的行不翻回，不新增用户勾选，不读取文件内容。
/// 返回本次重算取消的行数；执行期的实空复查与最终空目录清理仍独立兜底。
pub fn recompute_plan(directory: &Path) -> Result<usize> {
    // open_existing：任务库文件消失时不静默创建空库（apply 同口径）。
    let db = Database::open_existing(directory)?;
    let status: String = db.get("status")?;
    anyhow::ensure!(
        status == "ready",
        "任务不是待确认状态（{status}），不重算受影响计划"
    );
    let move_kind = serde_json::to_string(&ActionKind::Move)?;
    let delete_kind = serde_json::to_string(&ActionKind::Delete)?;
    let empty_kind = serde_json::to_string(&ActionKind::EmptyDirectory)?;
    // 占位项 = 取消且未执行的 Move/Delete 源 + 取消且未执行的空目录行自身。
    // 消费方只做等值与前缀匹配、与行序无关，三种 kind 合并为一条查询。
    let mut occupied: Vec<String> = {
        let mut statement = db.conn.prepare(
            "SELECT source FROM actions WHERE kind IN (?1, ?2, ?3) AND selected=0 AND state='pending'",
        )?;
        let rows = statement.query_map(params![move_kind, delete_kind, empty_kind], |row| {
            row.get::<_, String>(0)
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    // 取消一个 Move 后，其源仍留在盘上，可能阻挡另一条原本选中的 Move 目标。
    // 以固定点逐轮取消，保证用户取消一条链中的任意一项都不会留下必然撞名的
    // 执行计划；不新增勾选，也不读取文件内容。
    let mut cancelled = 0usize;
    let mut pending_moves: Vec<(i64, String, String)> = {
        let mut statement = db.conn.prepare(
            "SELECT id, source, target FROM actions WHERE kind=?1 AND selected=1 AND state='pending' AND target IS NOT NULL",
        )?;
        let rows = statement.query_map(params![move_kind], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    while let Some(index) = pending_moves.iter().position(|(_, _, target)| {
        let target = fsutil::fold_rel(target);
        occupied
            .iter()
            .any(|source| fsutil::fold_rel(source) == target)
    }) {
        let (id, source, _) = pending_moves.remove(index);
        db.set_selected(id, false)?;
        occupied.push(source);
        cancelled += 1;
    }
    // 与 planner 的前缀区间判定同口径：Windows 折叠大小写后排序去重，二分定位。
    occupied = occupied
        .into_iter()
        .map(|rel| fsutil::fold_rel(&rel))
        .collect();
    occupied.sort_unstable();
    occupied.dedup();
    let rows: Vec<(i64, String)> = {
        let mut statement = db.conn.prepare(
            "SELECT id, source FROM actions WHERE kind=?1 AND selected=1 AND state='pending'",
        )?;
        let rows = statement.query_map(params![empty_kind], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, rel) in rows {
        let folded = fsutil::fold_rel(&rel);
        let probe = format!("{folded}/");
        let index = occupied.partition_point(|item| item.as_str() < probe.as_str());
        let blocked = occupied
            .get(index)
            .is_some_and(|item| item.starts_with(probe.as_str()));
        if blocked {
            db.set_selected(id, false)?;
            cancelled += 1;
        }
    }
    Ok(cancelled)
}
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "organize_apply", skip_all, err)
)]
pub fn apply(directory: &Path, context: TaskContext) -> Result<TaskResult> {
    // Database::open 会在文件缺失时创建一个空库；先确认这是扫描生成的任务目录，
    // 避免把任意目录（甚至写错路径）悄悄变成一个必然失败的空任务。
    // 该检查必须先于 RootGuard：否则写错路径会先在错误位置创建目录并落下锁文件。
    anyhow::ensure!(
        directory.join("task.sqlite3").is_file(),
        "目录里没有 task.sqlite3；请选择「开始解压与分析」生成的任务目录"
    );
    let db = Database::open(directory)?;
    // 锁位置：prepare 已把当时的全局锁目录记进任务库（决策规则见 lock_dir_for）。
    let recorded: Option<PathBuf> = db.get::<String>("state_dir").ok().map(PathBuf::from);
    let _guard = fsutil::RootGuard::acquire(&lock_dir_for(directory, recorded.as_deref())?)?;
    let status: String = db.get("status")?;
    if status != "ready" {
        bail!("任务不是待确认状态（{status}）；请重新扫描，不会盲目重放旧计划");
    }
    let root_text: String = db.get("root")?;
    let protection = resolve_protection()?;
    let (root, protected) =
        fsutil::normalize_root_with(Path::new(&root_text), protection.as_deref())?;
    // prepare 记录的是当时的规范化路径；apply 重新解析 canonicalize（会解析 junction）。
    // 两者不一致说明目录被移动或被替换成指向别处的链接，继续执行会把整理动作落到另一棵树上。
    anyhow::ensure!(
        fsutil::path_string(&root)? == root_text,
        "目录位置已改变或被链接替换（{root_text}）；请重新扫描生成新计划后再执行"
    );
    let config = db.config()?;
    config.validate()?;
    let summary = db.summary()?;
    let mut job = Job {
        root,
        config,
        context,
        db,
        summary,
    };
    job.db.set("status", &"executing")?;
    let outcome = (|| {
        // S-01/F02：执行开始时清理上次执行崩溃残留的硬链接临时文件。名单来自分析
        // 阶段登记的「本次范围内疑似残留」（不越出本次范围设置），执行时逐项复核
        // 既有谓词；已请求停止时不清理——用户尚未授权本次执行的任何删除。
        job.context.control.checkpoint()?;
        let removed_link_temps = clean_orphan_link_temps(&mut job)?;
        if removed_link_temps > 0 {
            job.log(
                "任务",
                "",
                "",
                "提示",
                &format!(
                    "已清理 {removed_link_temps} 个上次执行崩溃残留的硬链接临时文件（.jchtools-link-*，均为分析阶段在本次范围内登记并经复核的项）"
                ),
                0,
            )?;
        }
        // 性能打点（perf-tracing，默认不编译）：计划动作的实际执行区间；与前面的
        // 崩溃残留清理、后面的收尾写库分开计时。
        crate::perf::perf_span!("execute_actions");
        let pool = io_pool()?;
        let mut moves_executed = false;
        let mut cursor = 0;
        loop {
            let actions = job.db.actions_page(cursor, APPLY_BATCH)?;
            if actions.is_empty() {
                break;
            }
            // 一批一个事务（见 APPLY_BATCH）。事务里写库只影响「任务库记录的可见时机」，
            // 文件改动本身不受事务保护，因此：
            // - 出错/取消时提交已完成的部分（文件已经删了/移了，丢掉记录只会让计划行
            //   状态与磁盘不一致，C-11）；SQLite 语句级失败不会中断事务，提交是安全的；
            // - 崩溃/强杀时最多丢失当前一批（<64 项）的状态与审计行，此时任务不可能从
            //   「执行中」恢复（C-10 无断点恢复），不存在重复删除的可能。
            job.db.conn.execute_batch("BEGIN IMMEDIATE")?;
            let batch = (|| -> Result<()> {
                // 分段执行：文件删除彼此独立、空目录删除按深度分层（计划按深度降序生成，
                // 同层目录不互为父子，更深层已在此前的段/页删除）——这两类连续动作在
                // IO 池上并行；移动/硬链接/未勾选保持逐项串行，语义与旧实现一致。
                let mut index = 0;
                while index < actions.len() {
                    // 移动动作不能只按计划行号执行：当一个目标正是另一个移动的
                    // 源时，必须先释放源；交换环则由 execute_planned_moves 临时
                    // 暂存一个源后完成。该预处理只执行一次，之后本页及后续页的
                    // Move 行已经标记 done/failed，跳过即可。
                    if actions[index].kind == ActionKind::Move {
                        if !moves_executed {
                            execute_planned_moves(&mut job)?;
                            moves_executed = true;
                        }
                        index += 1;
                        continue;
                    }
                    if actions[index].state != "pending" {
                        index += 1;
                        continue;
                    }
                    let Some(kind) = parallel_run_kind(&actions[index]) else {
                        execute_sequential(&mut job, &actions[index])?;
                        index += 1;
                        continue;
                    };
                    let mut end = index + 1;
                    while end < actions.len() && parallel_run_kind(&actions[end]) == Some(kind) {
                        end += 1;
                    }
                    execute_run(&mut job, &pool, &actions[index..end], kind)?;
                    index = end;
                }
                Ok(())
            })();
            match batch {
                Ok(()) => job.db.conn.execute_batch("COMMIT")?,
                Err(error) => {
                    // 尽力提交：SQLite 已因致命错误自行回滚时 COMMIT 会失败，此时无需处理。
                    let _ = job.db.conn.execute_batch("COMMIT");
                    return Err(error);
                }
            }
            cursor = actions[actions.len() - 1].id;
        }
        // H-05/C-07：计划动作执行完总是做最终实空清理（不可关闭、不受文件清理/删除
        // 方式选择影响）：既覆盖本次新产生的空目录与空目录链，也覆盖从未入库的目录
        // （例如实际为空的「解压失败」暂存区、归类新建但没落文件的目录）。
        final_empty_cleanup(&mut job, protected.as_deref())?;
        Ok::<_, anyhow::Error>(())
    })();
    job.db.set("summary", &job.summary)?;
    job.db.set(
        "status",
        &if outcome.is_ok() {
            "finished"
        } else if job.context.control.is_cancelled() {
            "cancelled"
        } else {
            "failed"
        },
    )?;
    outcome?;
    #[cfg(feature = "perf-tracing")]
    crate::perf::apply_done(
        job.summary.deleted,
        job.summary.moved,
        job.summary.skipped,
        job.summary.errors,
    );
    Ok(TaskResult {
        directory: directory.to_path_buf(),
        summary: job.summary,
    })
}
/// 整理收尾的实空清理（H-05/C-07）：自底向上删除实际为空的目录及空目录链。
/// 「总是执行」：没有配置开关可关闭，也不受文件清理/删除方式选择影响——空目录清理
/// 不属于 C-08 的六类清理项，H-05 不提供关闭这一步的选项。只尊重：选定根目录、
/// Git 整树排除（H-06）、递归范围、用户排除与隐藏/系统开关、状态目录/程序目录/
/// 工具工作目录、Windows 系统目录子树（S-05），以及取消；一律永久删除（S-02）。
/// 与扫描共用同一套范围口径（[`ScopeFilter`]），所以「解压失败」暂存区内实际为空的
/// 目录也按 H-05 清理（C-09），但其内容一律不碰——判定只看目录项是否为空。
fn final_empty_cleanup(job: &mut Job, protected: Option<&Path>) -> Result<()> {
    job.context.control.checkpoint()?;
    let errors_before_cleanup = job.summary.errors;
    let excluded = rules::build_exclusions(&job.config.exclusions)?;
    let executable_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().and_then(|d| fs::canonicalize(d).ok()));
    let state = job
        .db
        .get::<String>("state_dir")
        .ok()
        .map(PathBuf::from)
        .and_then(|directory| fs::canonicalize(directory).ok());
    let (state_prefix, exe_prefix, root_under_special) =
        special_prefixes(&job.root, state.as_deref(), executable_dir.as_deref());
    if root_under_special {
        // 选定根位于状态目录/程序目录内：扫描整树剪枝，这里同样不处理任何条目。
        return Ok(());
    }
    // S-05：根包含 Windows 系统目录时该子树整树剪枝，清理与扫描同一口径。
    let protected_prefix = protected.and_then(|dir| fsutil::relative_string(&job.root, dir).ok());
    let scope_filter = ScopeFilter {
        excluded: &excluded,
        include_hidden: job.config.include_hidden,
        include_system: job.config.include_system,
        // C-09：暂存区内容不参与处理，但实际为空的目录仍按 H-05 清理。
        quarantine: None,
        state_prefix,
        exe_prefix,
        protected_prefix,
        root_under_special,
    };
    let recursive = job.config.recursive;
    let mut stack: Vec<SweepFrame> = Vec::new();
    // 选定根目录本身绝不删除（H-05）：它的条目决定深度 1 的候选。
    let (children, keep, notes) = sweep_entries(&scope_filter, true, &job.root, "");
    for note in notes {
        job.summary.errors += 1;
        job.log("清理", "", "", "跳过", &note, 0)?;
    }
    stack.push(SweepFrame {
        path: job.root.clone(),
        rel: String::new(),
        children,
        next: 0,
        keep,
    });
    while !stack.is_empty() {
        // 后序遍历：先把全部子目录处理完，再决定当前目录是否实际为空。
        let index = stack.len() - 1;
        if stack[index].next < stack[index].children.len() {
            let (path, rel) = stack[index].children[stack[index].next].clone();
            stack[index].next += 1;
            let (children, keep, notes) = sweep_entries(&scope_filter, recursive, &path, &rel);
            for note in notes {
                job.summary.errors += 1;
                job.log("清理", &rel, "", "跳过", &note, 0)?;
            }
            stack.push(SweepFrame {
                path,
                rel,
                children,
                next: 0,
                keep,
            });
            continue;
        }
        let Some(frame) = stack.pop() else {
            break;
        };
        if frame.rel.is_empty() {
            // 选定根目录本身绝不删除（H-05）。
            continue;
        }
        job.context.control.checkpoint()?;
        if !sweep_remove(job, &frame)? {
            // 目录没能删除（非空/失败/超出范围）：父目录因此不算实际为空。
            if let Some(parent) = stack.last_mut() {
                parent.keep = true;
            }
        }
    }
    anyhow::ensure!(
        job.summary.errors == errors_before_cleanup,
        "空目录清理未完成：有目录无法读取或删除，详情见进度与日志"
    );
    Ok(())
}
/// 删除一个收尾清理候选目录，返回是否真的删掉了（没删掉时父目录不算实际为空）。
/// 一律永久删除（S-02）；这里不看用户勾选状态——H-05 的清理是强制步骤。
fn sweep_remove(job: &mut Job, frame: &SweepFrame) -> Result<bool> {
    if frame.keep {
        return Ok(false);
    }
    // 执行期复查（与计划动作同口径）：platform::remove 还会再确认一次目录为空。
    match platform::remove(&frame.path, DeleteMode::Permanent, &job.context.control) {
        Ok(DeleteResult::Permanent) => {
            job.summary.deleted += 1;
            job.log(
                "清理",
                &frame.rel,
                "",
                "已永久删除",
                "整理收尾：目录实际为空（H-05）",
                0,
            )?;
            Ok(true)
        }
        Ok(DeleteResult::Kept) => {
            job.summary.skipped += 1;
            job.log("清理", &frame.rel, "", "保留", "删除方式为保留", 0)?;
            Ok(false)
        }
        Err(error) => {
            // 用户主动取消不是失败（C-10）：直接以取消错误收尾，不记为错误。
            if job.context.control.is_cancelled() {
                job.context.control.check_cancelled()?;
            }
            job.summary.errors += 1;
            job.log("清理", &frame.rel, "", "失败", &format!("{error:#}"), 0)?;
            Ok(false)
        }
    }
}
/// 收尾清理的一个待处理目录：读条目、处理完全部子目录后再决定它是否实际为空。
struct SweepFrame {
    path: PathBuf,
    rel: String,
    /// 需要下钻处理的子目录（按目录项枚举序）。
    children: Vec<(PathBuf, String)>,
    next: usize,
    /// 该目录里存在不会随本次清理消失的条目（普通文件、链接、被范围剪枝的条目、
    /// Git 目录、不可读条目，或递归范围之外的子目录）：存在这样的条目即「不是空目录」。
    keep: bool,
}
/// 读取收尾清理候选目录的条目：返回（需下钻的子目录、是否确定不会为空、记录的跳过原因）。
/// `collect` 为假（递归已关闭，且不是选定根目录的第一层）时不下钻更深层，也不清理它们；
/// 深层条目仍会使本目录不算空。
fn sweep_entries(
    scope_filter: &ScopeFilter<'_>,
    collect: bool,
    dir: &Path,
    rel: &str,
) -> (Vec<(PathBuf, String)>, bool, Vec<String>) {
    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        Err(error) => return (Vec::new(), true, vec![format!("{error:#}")]),
    };
    let mut children = Vec::new();
    let mut keep = false;
    let mut notes = Vec::new();
    for entry in read {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                // 枚举中断：该目录未必为空，按保留处理。
                keep = true;
                notes.push(format!("{error:#}"));
                continue;
            }
        };
        let Ok(name) = entry.file_name().into_string() else {
            keep = true;
            continue;
        };
        let child_rel = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let Ok(metadata) = entry.metadata() else {
            keep = true;
            continue;
        };
        if scope_filter.prunes(&child_rel, &name, &metadata) {
            // 超出本次处理范围的条目仍在盘上：本目录不是实际为空，也不会被删除。
            keep = true;
            continue;
        }
        if !metadata.is_dir() {
            keep = true;
            continue;
        }
        if !collect {
            // 递归范围之外：不清理也不下钻（清理范围与本次扫描一致）。
            keep = true;
            continue;
        }
        match fsutil::is_git_root(&entry.path()) {
            // H-06：目录直接含 .git 时整树排除，不遍历内部，也不清理它。
            Ok(true) => keep = true,
            Ok(false) => children.push((entry.path(), child_rel)),
            Err(error) => {
                keep = true;
                notes.push(format!("{}：{error:#}", entry.path().display()));
            }
        }
    }
    (children, keep, notes)
}
/// 可并行执行的动作段类别：文件删除彼此独立；空目录删除按深度分层
/// （计划按深度降序生成，同层目录不互为父子，深层已在更早的段/页删除）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParallelKind {
    Delete,
    EmptyDir(usize),
}
fn parallel_run_kind(action: &Action) -> Option<ParallelKind> {
    if !action.selected {
        return None;
    }
    match action.kind {
        ActionKind::Delete => Some(ParallelKind::Delete),
        ActionKind::EmptyDirectory => Some(ParallelKind::EmptyDir(
            action.source.bytes().filter(|byte| *byte == b'/').count(),
        )),
        ActionKind::Move => None,
    }
}
/// 单个动作的串行执行（移动/硬链接/未勾选）：语义与旧逐项循环一致。
/// 动作失败结算：用户主动取消不算失败（不计 errors、不标 failed，与 prepare
/// 阶段取消口径一致）；其余错误计 errors、落 failed、写统一格式的失败日志。
/// 串行路径与并行段结算共用同一口径，防止两处取消/失败判定漂移。
fn record_action_failure(job: &mut Job, action: &Action, error: &anyhow::Error) -> Result<()> {
    if job.context.control.is_cancelled() {
        job.context.control.check_cancelled()?;
    }
    job.summary.errors += 1;
    job.db.mark_action(action.id, "failed")?;
    // 失败日志统一 phase「执行」、带目标，便于按口径筛选。
    job.log(
        "执行",
        &action.source,
        action.target.as_deref().unwrap_or(""),
        "失败",
        &format!("{error:#}"),
        0,
    )?;
    job.context.control.check_cancelled()?;
    Ok(())
}
fn execute_sequential(job: &mut Job, action: &Action) -> Result<()> {
    job.context.control.checkpoint()?;
    if !action.selected {
        job.summary.skipped += 1;
        job.db.mark_action(action.id, "unselected")?;
        // 未勾选不计入 completed：GUI 分母 count_selected_pending 只含 selected+pending，
        // 分子若含 unselected 会出现 done>planned、提前 100% 的口径分裂。
        return Ok(());
    }
    job.context
        .status(format!("执行 {:?}：{}", action.kind, action.source));
    match execute_action(job, action) {
        Ok(true) => {
            job.db.mark_action(action.id, "done")?;
        }
        Ok(false) => {
            job.summary.skipped += 1;
            job.db.mark_action(action.id, "skipped")?;
        }
        Err(error) => record_action_failure(job, action, &error)?,
    }
    job.context
        .control
        .completed
        .fetch_add(1, Ordering::Relaxed);
    Ok(())
}
/// 并行执行一段相互独立的删除类动作：先整段记审计（意图先于任何变更落账），
/// 再在 IO 池上并行做文件系统变更，最后按段内顺序串行结算（日志、计数、任务库状态）。
/// 路径只用字符串级校验（safe_relative）：P-08 假定处理期间文件不被其他程序改动，
/// 且 platform::remove 自身复查链接并拒绝非空目录；写入类动作仍走 safe_join 全校验。
fn execute_run(
    job: &mut Job,
    pool: &rayon::ThreadPool,
    run: &[Action],
    kind: ParallelKind,
) -> Result<()> {
    job.context.control.checkpoint()?;
    job.context.status(if run.len() > 1 {
        format!(
            "执行 {:?}：{} 等 {} 项",
            run[0].kind,
            run[0].source,
            run.len()
        )
    } else {
        format!("执行 {:?}：{}", run[0].kind, run[0].source)
    });
    let size = |action: &Action| action.expected.as_ref().map_or(0, |snapshot| snapshot.size);
    for action in run {
        job.log(
            "删除",
            &action.source,
            "",
            "准备",
            &action.reason,
            size(action),
        )?;
    }
    let mut targets = Vec::with_capacity(run.len());
    for action in run {
        let path = job.root.join(fsutil::safe_relative(&action.source)?);
        targets.push((path, action.mode));
    }
    let control = job.context.control.clone();
    let results: Vec<_> = pool.install(|| {
        targets
            .par_iter()
            .map(|(path, mode)| match kind {
                ParallelKind::Delete => platform::remove(path, *mode, &control).map(Some),
                ParallelKind::EmptyDir(_) => {
                    // 执行期实空复查（同旧 EmptyDirectory 分支）；同段目录互不嵌套，可并行。
                    if !dir_recheck_empty(path)? {
                        return Ok(None);
                    }
                    platform::remove(path, *mode, &control).map(Some)
                }
            })
            .collect()
    });
    for (action, result) in run.iter().zip(results) {
        match result {
            Err(error) => record_action_failure(job, action, &error)?,
            // 空目录实空复查未过（已不存在/非目录/非空）：按旧口径计跳过、不写结果日志。
            Ok(None) => {
                job.summary.skipped += 1;
                job.db.mark_action(action.id, "skipped")?;
            }
            Ok(Some(DeleteResult::Kept)) => {
                job.summary.skipped += 1;
                job.log(
                    "删除",
                    &action.source,
                    "",
                    "保留",
                    &action.reason,
                    size(action),
                )?;
                job.db.mark_action(action.id, "skipped")?;
            }
            Ok(Some(DeleteResult::Permanent)) => {
                job.summary.deleted += 1;
                // 硬链接源（links>1）的内容仍由其他链接持有，不计入 permanent_bytes（S-06）；
                // 空目录（expected=None）逻辑大小为 0。
                if action
                    .expected
                    .as_ref()
                    .is_none_or(|snapshot| snapshot.links <= 1)
                {
                    job.summary.permanent_bytes =
                        job.summary.permanent_bytes.saturating_add(size(action));
                }
                job.db.deactivate_file_rel(&action.source)?;
                job.log(
                    "删除",
                    &action.source,
                    "",
                    "已永久删除",
                    &action.reason,
                    size(action),
                )?;
                job.db.mark_action(action.id, "done")?;
            }
        }
        job.context
            .control
            .completed
            .fetch_add(1, Ordering::Relaxed);
    }
    Ok(())
}
/// 执行所有归类/项目移动，并处理「目标是另一项源」的依赖。
/// `a -> b` 与 `b -> c` 要先释放依赖源；交换环则先把一个源临时暂存。
fn execute_planned_moves(job: &mut Job) -> Result<()> {
    let mut actions = Vec::new();
    let mut cursor = 0;
    loop {
        let page = job.db.actions_page_filtered(cursor, 256, Some("move"))?;
        if page.is_empty() {
            break;
        }
        cursor = page.last().map_or(cursor, |action| action.id);
        actions.extend(page.into_iter().filter(|action| action.state == "pending"));
    }
    if actions.is_empty() {
        return Ok(());
    }
    // 环形移动暂存在本次任务的状态目录：扫描已排除状态目录，用户根下同名
    // 普通目录仍会参与整理。OWNER 标记限制清理范围，失败时保留唯一副本。
    let owner = uuid::Uuid::new_v4().to_string();
    let work = fsutil::safe_join(&job.db.directory, "move-stage")?;
    fs::create_dir_all(&work)?;
    let stage = work.join(&owner);
    fs::create_dir(&stage)?;
    fs::write(stage.join("OWNER"), &owner)?;
    let stage_guard = MoveStageGuard {
        directory: stage.clone(),
        owner,
    };
    let mut staged: HashMap<i64, PathBuf> = HashMap::new();
    while !actions.is_empty() {
        if let Some(index) = actions.iter().position(|action| !action.selected) {
            let action = actions.remove(index);
            execute_sequential(job, &action)?;
            continue;
        }
        let available = actions.iter().enumerate().find(|(index, action)| {
            let Some(target) = action.target.as_deref() else {
                return true;
            };
            let target = fsutil::fold_rel(target);
            !actions.iter().enumerate().any(|(other, candidate)| {
                other != *index
                    && !staged.contains_key(&candidate.id)
                    && fsutil::fold_rel(&candidate.source) == target
            })
        });
        if let Some((index, _)) = available {
            let action = actions.remove(index);
            let staged_source = staged.remove(&action.id);
            let had_stage = staged_source.is_some();
            let source = match staged_source {
                Some(path) => path,
                None => fsutil::safe_join(&job.root, &action.source)?,
            };
            execute_move_sequential(
                job,
                &action,
                &source,
                !had_stage && staged.is_empty(),
                had_stage,
            )?;
            continue;
        }
        // 所有目标都被另一项源占用，形成环；暂存第一项源后，后续动作连续收敛。
        job.context.control.checkpoint()?;
        let action = &actions[0];
        let source = fsutil::safe_join(&job.root, &action.source)?;
        let temporary = stage.join(action.id.to_string());
        if let Err(error) = fsutil::move_file_preserving_times(&source, &temporary) {
            let action = actions.remove(0);
            record_move_failure(job, &action, &error)?;
            job.context
                .control
                .completed
                .fetch_add(1, Ordering::Relaxed);
        } else {
            staged.insert(action.id, temporary);
        }
    }
    drop(stage_guard);
    Ok(())
}

struct MoveStageGuard {
    directory: PathBuf,
    owner: String,
}

impl Drop for MoveStageGuard {
    fn drop(&mut self) {
        if fs::read_to_string(self.directory.join("OWNER"))
            .ok()
            .as_deref()
            != Some(self.owner.as_str())
        {
            return;
        }
        // 暂存恢复失败时目录中可能仍有唯一副本。Drop 绝不能递归删除它；只有 OWNER
        // 是目录内唯一条目时，才逐项删除标记并删除空目录，保留失败路径供恢复。
        let Ok(entries) = fs::read_dir(&self.directory) else {
            return;
        };
        let mut has_payload = false;
        for entry in entries.flatten() {
            if entry.file_name() != "OWNER" {
                has_payload = true;
                break;
            }
        }
        if has_payload {
            return;
        }
        if fs::remove_file(self.directory.join("OWNER")).is_ok()
            && fs::remove_dir(&self.directory).is_ok()
        {
            if let Some(parent) = self.directory.parent() {
                let _ = fs::remove_dir(parent);
            }
        }
    }
}

fn execute_move_sequential(
    job: &mut Job,
    action: &Action,
    source: &Path,
    checkpoint: bool,
    restore_on_error: bool,
) -> Result<()> {
    if checkpoint {
        job.context.control.checkpoint()?;
    }
    job.context
        .status(format!("执行 {:?}：{}", action.kind, action.source));
    match execute_move_at(job, action, source) {
        Ok(true) => job.db.mark_action(action.id, "done")?,
        Ok(false) => {
            job.summary.skipped += 1;
            job.db.mark_action(action.id, "skipped")?;
        }
        Err(error) => {
            if restore_on_error {
                let original = fsutil::safe_join(&job.root, &action.source)?;
                if !original.exists() && source.exists() {
                    if let Err(restore_error) =
                        fsutil::move_file_preserving_times(source, &original)
                    {
                        job.log(
                            "移动",
                            &action.source,
                            "",
                            "恢复失败",
                            &format!(
                                "移动目标失败后，临时暂存无法恢复到原位置：{restore_error:#}；暂存路径：{}",
                                source.display()
                            ),
                            0,
                        )?;
                    }
                } else if original.exists() && source.exists() {
                    job.log(
                        "移动",
                        &action.source,
                        "",
                        "恢复失败",
                        &format!(
                            "原位置已被占用，无法放回暂存中的文件；请从暂存路径恢复：{}",
                            source.display()
                        ),
                        0,
                    )?;
                } else if !original.exists() {
                    job.log(
                        "移动",
                        &action.source,
                        "",
                        "恢复失败",
                        &format!(
                            "移动目标失败后原位置与临时暂存均不存在；暂存路径：{}",
                            source.display()
                        ),
                        0,
                    )?;
                }
            }
            record_move_failure(job, action, &error)?;
            job.context.control.check_cancelled()?;
        }
    }
    job.context
        .control
        .completed
        .fetch_add(1, Ordering::Relaxed);
    Ok(())
}

fn record_move_failure(job: &mut Job, action: &Action, error: &anyhow::Error) -> Result<()> {
    job.summary.errors += 1;
    job.db.mark_action(action.id, "failed")?;
    job.log(
        "执行",
        &action.source,
        action.target.as_deref().unwrap_or(""),
        "失败",
        &format!("{error:#}"),
        0,
    )?;
    Ok(())
}

fn execute_move_at(job: &mut Job, action: &Action, source: &Path) -> Result<bool> {
    let target = fsutil::safe_join(
        &job.root,
        action.target.as_deref().context("移动操作缺少目标")?,
    )?;
    fsutil::ensure_parent(&job.root, &target)?;
    job.log(
        "移动",
        &action.source,
        action.target.as_deref().unwrap_or(""),
        "准备",
        &action.reason,
        action.expected.as_ref().map_or(0, |snapshot| snapshot.size),
    )?;
    fsutil::move_file_preserving_times(source, &target)?;
    job.summary.moved += 1;
    job.log(
        "移动",
        &action.source,
        action.target.as_deref().unwrap_or(""),
        "成功",
        &action.reason,
        0,
    )?;
    Ok(true)
}

fn execute_action(job: &mut Job, action: &Action) -> Result<bool> {
    // P-08：处理期间假定文件不被其他程序改动，执行阶段不再做快照比对；
    // S-03/C-12：删除前 `MUST NOT` 重读文件内容做逐字节复核，去重判定完全依据
    // 分析期算出的整文件哈希，因此执行阶段不读取任何文件内容。
    let source = fsutil::safe_join(&job.root, &action.source)?;
    match action.kind {
        ActionKind::Delete => Ok(job.delete_path(
            &source,
            action.expected.as_ref(),
            action.mode,
            &action.reason,
            true,
        )? != DeleteResult::Kept),
        ActionKind::Move => execute_move_at(job, action, &source),
        ActionKind::EmptyDirectory => {
            if !dir_recheck_empty(&source)? {
                return Ok(false);
            }
            Ok(
                job.delete_path(&source, None, action.mode, &action.reason, true)?
                    != DeleteResult::Kept,
            )
        }
    }
}

#[cfg(test)]
mod lock_tests {
    use super::*;
    // 平台门禁原因：仅 Windows 门禁测试使用（UNC/盘符前缀拼接），非 Windows 无使用者。
    #[cfg(windows)]
    const BSLASH: char = std::path::MAIN_SEPARATOR; // Windows 下为反斜杠

    #[test]
    fn move_stage_guard_keeps_payload_when_cleanup_is_not_safe() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join(".jchtools-move-work").join("owner");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("OWNER"), "owner").unwrap();
        std::fs::write(directory.join("唯一暂存副本"), b"payload").unwrap();
        {
            let _guard = MoveStageGuard {
                directory: directory.clone(),
                owner: "owner".into(),
            };
        }
        assert!(directory.join("唯一暂存副本").is_file());
        assert!(directory.join("OWNER").is_file());
    }
    // apply 锁目录决策：recorded 有效优先；失效时仅 tasks/ 布局可回退推导。
    #[test]
    fn recorded_state_dir_takes_priority() {
        let temp = tempfile::tempdir().unwrap();
        let tasks = temp.path().join("tasks").join("t1");
        std::fs::create_dir_all(&tasks).unwrap();
        assert_eq!(
            lock_dir_for(&tasks, Some(temp.path())).unwrap(),
            *temp.path()
        );
    }
    #[test]
    fn invalid_record_with_tasks_layout_falls_back_to_parent() {
        let temp = tempfile::tempdir().unwrap();
        let tasks = temp.path().join("tasks").join("t1");
        std::fs::create_dir_all(&tasks).unwrap();
        let gone = temp.path().join("gone"); // 不存在 → recorded 过滤失效
        assert_eq!(lock_dir_for(&tasks, Some(&gone)).unwrap(), *temp.path());
        assert_eq!(lock_dir_for(&tasks, None).unwrap(), *temp.path());
    }
    // 平台门禁原因：断言依赖 Windows 路径语义（盘符前缀与 \server\share UNC 前缀的
    // components 解析），Unix 上分隔符不同、该解析不存在；OS 无关的三条决策测试保持全平台。
    #[cfg(windows)]
    #[test]
    fn tasks_layout_at_drive_root_is_rejected() {
        // 盘根/共享根不是状态目录：在那里落锁会留下陌生位置的锁文件（engine 不挂载
        // 任何盘，这里用纯路径验证决策逻辑，不触碰文件系统；UNC 前缀用 BSLASH 拼接，
        // 避免源码转义歧义）。
        assert!(
            lock_dir_for(Path::new(r"Q:\tasks\t1"), None).is_err(),
            "盘根 tasks 布局必须拒绝"
        );
        let unc = [BSLASH, BSLASH].iter().collect::<String>()
            + "server"
            + &BSLASH.to_string()
            + "share"
            + &BSLASH.to_string()
            + "tasks"
            + &BSLASH.to_string()
            + "t1";
        assert!(
            lock_dir_for(Path::new(&unc), None).is_err(),
            "UNC 共享根 tasks 布局必须拒绝"
        );
    }
    // 平台门禁原因：同上，断言 Windows 前缀路径的解析结果。
    #[cfg(windows)]
    #[test]
    fn tasks_layout_deep_on_unc_share_is_allowed() {
        // UNC 上正常深度的状态目录（如 \\server\share\JchTools\data）必须放行：
        // 前缀 \\server\share 不可拆分，不能用「祖先层数少」误判为根。
        let unc = [BSLASH, BSLASH].iter().collect::<String>()
            + "server"
            + &BSLASH.to_string()
            + "share"
            + &BSLASH.to_string()
            + "JchTools"
            + &BSLASH.to_string()
            + "data"
            + &BSLASH.to_string()
            + "tasks"
            + &BSLASH.to_string()
            + "t1";
        let expected = [BSLASH, BSLASH].iter().collect::<String>()
            + "server"
            + &BSLASH.to_string()
            + "share"
            + &BSLASH.to_string()
            + "JchTools"
            + &BSLASH.to_string()
            + "data";
        assert_eq!(
            lock_dir_for(Path::new(&unc), None).unwrap(),
            PathBuf::from(expected),
            "UNC 深目录 tasks 布局必须放行"
        );
        let win = Path::new(r"C:\state\tasks\t1");
        assert_eq!(lock_dir_for(win, None).unwrap(), PathBuf::from(r"C:\state"));
    }
    #[test]
    fn non_tasks_layout_without_record_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("t1");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(
            lock_dir_for(&dir, None).is_err(),
            "非 tasks 布局且无记录必须拒绝"
        );
    }
}

/// 扫描日志预览格式的特征测试：锁定 preview3 的「前 3 项、顿号连接、超出追加
/// 『 等』」口径（Git 整树提示与硬链接残留提示共用同一格式）。
#[cfg(test)]
mod preview_format_tests {
    use super::preview3;

    #[test]
    fn preview3_shows_three_items_and_marks_more_with_deng() {
        assert_eq!(preview3(&[]), "");
        assert_eq!(preview3(&["a".into()]), "a");
        assert_eq!(preview3(&["a".into(), "b".into(), "c".into()]), "a、b、c");
        assert_eq!(
            preview3(&["a".into(), "b".into(), "c".into(), "d".into()]),
            "a、b、c 等"
        );
    }
}

#[cfg(test)]
mod s05_scope_tests {
    use super::*;
    use crate::control::Context as TaskContext;
    use crate::db::Database;

    // 覆盖 S-05（受保护子树剪枝：命中相对前缀整树排除，兄弟目录不受影响）
    #[test]
    fn scope_filter_prunes_protected_subtree_only() {
        let excluded = rules::build_exclusions("").unwrap();
        let filter = ScopeFilter {
            excluded: &excluded,
            include_hidden: true,
            include_system: true,
            quarantine: Some(archive::QUARANTINE_DIR_NAME),
            state_prefix: None,
            exe_prefix: None,
            protected_prefix: Some("WinRoot".to_string()),
            root_under_special: false,
        };
        let temp = tempfile::tempdir().unwrap();
        let probe = temp.path().join("x.txt");
        fs::write(&probe, b"x").unwrap();
        let meta = fs::symlink_metadata(&probe).unwrap();
        assert!(
            filter.prunes("WinRoot", "WinRoot", &meta),
            "受保护子树根必须剪枝"
        );
        assert!(
            filter.prunes("WinRoot/System32/a.dll", "a.dll", &meta),
            "受保护子树后代必须剪枝"
        );
        assert!(
            !filter.prunes("WinRootBackup/a.txt", "a.txt", &meta),
            "不得按字符串前缀误伤兄弟目录"
        );
        assert!(!filter.prunes("other/a.txt", "a.txt", &meta));
    }

    /// S-05 合成受保护子树夹具：root/WinFake 内含「系统样」文件，root 内含普通文件。
    /// 不用真实系统目录做破坏性实验；返回（临时目录、根、受保护目录的规范化路径）。
    fn synthetic_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir_all(root.join("WinFake/System32")).unwrap();
        fs::write(root.join("WinFake/System32/a.dll"), b"sys").unwrap();
        fs::write(root.join("normal.txt"), b"n").unwrap();
        let protected = fs::canonicalize(root.join("WinFake")).unwrap();
        (temp, root, protected)
    }

    // 覆盖 S-05（端到端：合成受保护子树位于普通根内——分析扫描不进入、
    // 确认计数只含范围内的文件、范围提示写明已排除系统目录）
    #[test]
    fn prepare_excludes_synthetic_protected_subtree() {
        let (temp, root, protected) = synthetic_fixture();
        let state = temp.path().join("state");
        let result = prepare_at_with(
            &root,
            Config::default(),
            TaskContext::default(),
            &state,
            Some(&protected),
        )
        .unwrap();
        assert_eq!(result.summary.scanned, 1, "受保护子树内文件不入盘点");
        let db = Database::open_existing(&result.directory).unwrap();
        let text = db
            .conn
            .query_row(
                "SELECT group_concat(reason, ' | ') FROM events",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        assert!(
            text.contains("已排除 Windows 系统目录"),
            "范围提示必须写明已排除系统目录：{text}"
        );
    }

    // 覆盖 S-05（确认框清点与扫描同口径：受保护子树内的压缩包不计入）
    #[test]
    fn count_archives_excludes_synthetic_protected_subtree() {
        let (_temp, root, protected) = synthetic_fixture();
        fs::write(root.join("WinFake/inner.zip"), b"zip").unwrap();
        fs::write(root.join("y.zip"), b"zip").unwrap();
        let count = count_archives_with(&root, &Config::default(), Some(&protected)).unwrap();
        assert_eq!(count, 1, "受保护子树内的压缩包不计入清点");
        assert_eq!(
            count_archives_with(&root, &Config::default(), None).unwrap(),
            2,
            "对照组：未注入受保护目录时两包都计入"
        );
    }

    // 覆盖 S-05, H-05（收尾空目录清理不进入受保护子树；范围外普通空目录仍正常清理）
    #[test]
    fn final_cleanup_keeps_synthetic_protected_subtree() {
        let (temp, root, protected) = synthetic_fixture();
        fs::create_dir_all(root.join("WinFake/内空")).unwrap();
        fs::create_dir_all(root.join("普通空")).unwrap();
        let canonical_root = fs::canonicalize(&root).unwrap();
        let state = temp.path().join("state");
        let mut job = Job {
            root: canonical_root,
            config: Config::default(),
            context: TaskContext::default(),
            db: Database::create(&state).unwrap(),
            summary: crate::model::Summary::default(),
        };
        final_empty_cleanup(&mut job, Some(&protected)).unwrap();
        assert!(
            root.join("WinFake/System32/a.dll").is_file(),
            "受保护子树内容零改动"
        );
        assert!(
            root.join("WinFake/内空").is_dir(),
            "受保护子树内的空目录同样不得清理"
        );
        assert!(
            !root.join("普通空").exists(),
            "范围外的普通空目录仍按 H-05 清理"
        );
    }
}
