use crate::{
    control::{Control, Event},
    fsutil,
    model::Snapshot,
    rules,
    task::{
        enqueue_archive, is_fixed_container_name, observe, root_is_quarantine, Job,
        QUARANTINE_DIR_NAME,
    },
};
use anyhow::{bail, Context as _, Result};
use rusqlite::params;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Arc, Mutex},
};

/// IO 阶段（目录枚举、句柄查询、删除）的线程池：这些都是元数据级小操作、
/// 瓶颈在系统调用延迟而非 CPU；按磁盘延迟预算取小池（与 hash_workers 同思路，
/// 不按 CPU 核数打满磁盘）。
pub fn io_pool() -> Result<rayon::ThreadPool> {
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
    /// 扫描实际发现的隔离目录：只记录路径，不遍历内容。
    quarantine_skips: Vec<String>,
    /// 已确认带完整所有权标记的解压工作区：只提示，不扫描内部。
    workspace_skips: Vec<String>,
    /// 必要 Git 边界检查失败时拒绝继续扫描/处理，不能把未知目录当作已排除。
    git_boundary_error: Option<String>,
}
pub fn ensure_git_boundary_checked(error: Option<&str>) -> Result<()> {
    if let Some(error) = error {
        bail!("无法完成必要的 Git 边界判定（H-06），拒绝继续处理：{error}");
    }
    Ok(())
}
fn lock_sink(sink: &Mutex<ScanSink>) -> std::sync::MutexGuard<'_, ScanSink> {
    // 锁中毒只可能因持锁线程 panic；本模块持锁期间不 panic，恢复数据是安全回退。
    sink.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
/// 处理范围剪枝口径：扫描、确认框清点（X-02）与整理收尾实空清理共用同一套判定，
/// 三处不会各自漂移出不同范围。Git 整树排除（H-06）由调用方在枚举目录前单独判定。
pub struct ScopeFilter<'a> {
    pub excluded: &'a rules::Exclusions,
    pub include_hidden: bool,
    pub include_system: bool,
    /// 「解压失败」暂存区：扫描与清点整树跳过（C-09/X-07）；收尾实空清理不跳过
    /// （该目录实际为空时仍按 H-05 清理，但其内容一律不碰）。
    pub quarantine: Option<&'a str>,
    /// 状态目录/程序目录位于选定根内的相对路径前缀（剪枝其子树）。
    pub state_prefix: Option<String>,
    pub exe_prefix: Option<String>,
    /// S-05：根包含 Windows 系统目录时该子树的相对路径前缀（整树剪枝并提示）。
    pub protected_prefix: Option<String>,
    /// 选定根本身位于状态目录或程序目录内：整棵树按旧口径全部剪枝。
    pub root_under_special: bool,
}
impl ScopeFilter<'_> {
    /// `name` 仅非 Windows 的点开头隐藏判定使用（P-07：产品仅在 Windows 构建）。
    // [quality-baseline approved 2026-10-03] 平台口径参数，经用户裁定保留
    #[cfg_attr(windows, allow(unused_variables))]
    pub fn prunes(&self, child_rel: &str, name: &str, metadata: &fs::Metadata) -> bool {
        if self.root_under_special || fsutil::is_link(metadata) {
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
        // 附录 C：匹配对象没有末尾分隔符，不虚构目录形态扩大排除范围。
        if self.excluded.is_match(child_rel) {
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
    // C-09：所选根本身是隔离容器时，同样不能扫描其文件内容。
    if rel.is_empty() && ctx.scope.quarantine.is_some() && root_is_quarantine(dir) {
        lock_sink(ctx.sink).quarantine_skips.push(rel);
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
        tracing::error!(event = "scan_item_failed", stage = "git_boundary",
            source = %jchtools_foundation::logging::safe_error(&rel), error_type = "filesystem_boundary",
            error_code = error.chain().find_map(|cause| cause.downcast_ref::<std::io::Error>()).and_then(std::io::Error::raw_os_error),
            "扫描无法判定Git边界，整树跳过并停止任务");
        // 边界无法判定：不得继续遍历该目录（内容未知），如实记录并整树跳过。
        let mut sink = lock_sink(ctx.sink);
        sink.notes.push(ScanNote {
            path: dir.display().to_string(),
            message: format!("无法判定是否含 .git，已整树跳过：{error:#}"),
        });
        sink.taint.insert(rel.clone());
        sink.git_boundary_error
            .get_or_insert_with(|| format!("{}：{error:#}", dir.display()));
        return;
    }
    // Git 边界优先；只有真实标记工作区才整树排除，同名用户目录正常扫描。
    match fsutil::is_extraction_workspace(dir) {
        Ok(true) => {
            let mut sink = lock_sink(ctx.sink);
            sink.taint.insert(rel.clone());
            sink.workspace_skips.push(rel);
            return;
        }
        Ok(false) => {}
        Err(error) => {
            tracing::warn!(event = "scan_item_failed", stage = "workspace_boundary",
                source = %jchtools_foundation::logging::safe_error(&rel), error_type = "filesystem_boundary",
                error_code = error.chain().find_map(|cause| cause.downcast_ref::<std::io::Error>()).and_then(std::io::Error::raw_os_error),
                "无法安全判定解压工作区，保留该目录");
            let mut sink = lock_sink(ctx.sink);
            sink.notes.push(ScanNote {
                path: dir.display().to_string(),
                message: format!("无法安全判定解压工作区，已保留并整树跳过：{error:#}"),
            });
            sink.taint.insert(rel);
            return;
        }
    }
    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        Err(error) => {
            tracing::warn!(event = "scan_item_failed", stage = "directory_read",
                source = %jchtools_foundation::logging::safe_error(&rel), error_type = "io", error_code = error.raw_os_error(),
                "扫描目录无法读取，保留该目录");
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
                tracing::warn!(event = "scan_item_failed", stage = "directory_entry",
                    source = %jchtools_foundation::logging::safe_error(&rel), error_type = "io", error_code = error.raw_os_error(),
                    "扫描目录项无法读取，保留父目录");
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
            tracing::warn!(event = "scan_item_failed", stage = "file_name",
                source = %jchtools_foundation::logging::safe_error(&entry.path().to_string_lossy()), error_type = "invalid_utf8",
                "扫描文件名不能无损表示，跳过该项");
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
                tracing::warn!(event = "scan_item_failed", stage = "metadata",
                    source = %jchtools_foundation::logging::safe_error(&entry.path().to_string_lossy()), error_type = "io", error_code = error.raw_os_error(),
                    "扫描文件属性读取失败，跳过该项");
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
            if metadata.is_dir()
                && ctx
                    .scope
                    .quarantine
                    .is_some_and(|quarantine| name.eq_ignore_ascii_case(quarantine))
            {
                lock_sink(ctx.sink).quarantine_skips.push(child_rel);
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
                    tracing::error!(event = "scan_item_failed", stage = "git_boundary",
                        source = %jchtools_foundation::logging::safe_error(&child_rel), error_type = "filesystem_boundary",
                        error_code = error.chain().find_map(|cause| cause.downcast_ref::<std::io::Error>()).and_then(std::io::Error::raw_os_error),
                        "扫描无法判定子目录Git边界，整树跳过并停止任务");
                    let mut sink = lock_sink(ctx.sink);
                    sink.notes.push(ScanNote {
                        path: entry.path().display().to_string(),
                        message: format!("无法判定是否含 .git，已整树跳过：{error:#}"),
                    });
                    sink.taint.insert(child_rel);
                    sink.git_boundary_error
                        .get_or_insert_with(|| format!("{}：{error:#}", entry.path().display()));
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
                    tracing::warn!(event = "scan_item_failed", stage = "snapshot",
                        source = %jchtools_foundation::logging::safe_error(&child_rel), error_type = "filesystem_boundary",
                        error_code = error.chain().find_map(|cause| cause.downcast_ref::<std::io::Error>()).and_then(std::io::Error::raw_os_error),
                        "扫描文件快照读取失败，跳过该项");
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
        // 文件名的 Windows ordinal 匹配键与 UTF-16 计数由 db.insert_file 统一完成。
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
            let span = tracing::Span::current();
            scope.spawn(move |scope| {
                span.in_scope(|| walk_dir(scope, ctx_ref, &path, child_rel, 0));
            });
        }
        if inline < 32 {
            walk_dir(scope, ctx, &first_path, first_rel, inline + 1);
        } else {
            let span = tracing::Span::current();
            scope.spawn(move |scope| {
                span.in_scope(|| walk_dir(scope, ctx, &first_path, first_rel, 0));
            });
        }
    }
}
/// 深度先序回放：显式 DFS 栈保留同目录枚举序，目录退出时再归组尾卷。
/// 回放栈随目录深度增长在堆上，不让合法深链耗尽调用线程栈。
fn scan_emit(
    job: &mut Job,
    by_parent: &HashMap<String, Vec<ScanChild>>,
    enqueue: bool,
    parent: &str,
    depth: i64,
    count: &mut u64,
) -> Result<()> {
    struct Frame<'a> {
        parent: &'a str,
        children: &'a [ScanChild],
        next: usize,
        depth: i64,
        archive_entries: Option<HashSet<String>>,
    }
    let Some((parent, children)) = by_parent.get_key_value(parent) else {
        return Ok(());
    };
    let mut stack = vec![Frame {
        parent,
        children,
        next: 0,
        depth,
        archive_entries: enqueue.then(HashSet::new),
    }];
    while let Some(frame) = stack.last_mut() {
        let Some(child) = frame.children.get(frame.next) else {
            // 与递归返回时一致：整棵子树回放结束后，才处理本目录的残缺卷族。
            if enqueue {
                enqueue_old_style_tail_groups(job, frame.parent, frame.children)?;
                enqueue_missing_entry_groups(job, frame.parent, frame.children)?;
            }
            stack.pop();
            continue;
        };
        frame.next += 1;
        *count += 1;
        if (*count).is_multiple_of(2048) {
            job.db.conn.execute_batch("COMMIT; BEGIN IMMEDIATE;")?;
        }
        job.context.control.checkpoint()?;
        let rel = if frame.parent.is_empty() {
            child.name.clone()
        } else {
            format!("{}/{}", frame.parent, child.name)
        };
        match &child.kind {
            ScanKind::Dir { lower } => {
                job.db.insert_dir(&rel, lower, frame.depth)?;
                if let Some((parent, children)) = by_parent.get_key_value(&rel) {
                    let depth = frame.depth + 1;
                    stack.push(Frame {
                        parent,
                        children,
                        next: 0,
                        depth,
                        // 同目录的歧义入口只选一个代表；每层独立维护卷族集合。
                        archive_entries: enqueue.then(HashSet::new),
                    });
                }
            }
            ScanKind::File { normal, snapshot } => {
                job.db.insert_file(&rel, &child.name, normal, snapshot)?;
                job.summary.scanned += 1;
                job.summary.scanned_bytes = job.summary.scanned_bytes.saturating_add(snapshot.size);
                // S-01：根层固定容器被普通文件占用时原位保留；在哈希、清理与
                // 去重前退出活动集合，不能通过其他规则间接删除或挪走占用项。
                if !enqueue && frame.parent.is_empty() && is_fixed_container_name(&child.name) {
                    job.db.deactivate_file_rel(&rel)?;
                    job.log(
                        "扫描",
                        &rel,
                        "",
                        "保留",
                        "固定容器被普通文件占用：不删除、不挪走占用项（S-01）",
                        snapshot.size,
                    )?;
                }
                if let Some(entry_groups) = frame.archive_entries.as_mut() {
                    if rules::archive_entry_key(&child.name)
                        .is_some_and(|key| entry_groups.insert(key))
                    {
                        // 同一卷族只入队一次；入队失败计错，但不重新处理另一个歧义入口。
                        if let Err(error) = enqueue_archive(job, &job.root.join(&rel), 0) {
                            job.summary.errors += 1;
                            job.log("扫描", &rel, "", "跳过", &format!("{error:#}"), 0)?;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}
/// X-10：part rar 缺首卷组与数字尾卷缺入口组的入队——按「残缺但可归组的卷集
/// 计一包」把每组首个卷作为代表入队，解压阶段按缺入口整组失败并隔离（X-06），
/// 不把余卷静默漏掉。代表选择与确认清点（[`rules::part_rar_missing_first_groups`] /
/// [`rules::numbered_volume_missing_entry_groups`]）同口径；同组只入队一次。
fn enqueue_missing_entry_groups(job: &mut Job, parent: &str, children: &[ScanChild]) -> Result<()> {
    let lowered: Vec<String> = children
        .iter()
        .filter(|child| matches!(child.kind, ScanKind::File { .. }))
        .map(|child| child.name.to_ascii_lowercase())
        .collect();
    // 代表名是小写口径；入队必须用磁盘上的原始文件名（可能含大写）。
    let mut representatives: HashSet<String> = rules::part_rar_missing_first_groups(&lowered)
        .into_iter()
        .collect();
    representatives.extend(rules::numbered_volume_missing_entry_groups(&lowered));
    if representatives.is_empty() {
        return Ok(());
    }
    for child in children {
        let ScanKind::File { .. } = &child.kind else {
            continue;
        };
        let lower = child.name.to_ascii_lowercase();
        if !representatives.remove(&lower) {
            continue;
        }
        let rel = if parent.is_empty() {
            child.name.clone()
        } else {
            format!("{parent}/{}", child.name)
        };
        // 与 archive_name 入队同口径：入队失败计错误并跳过，不中断整个扫描。
        if let Err(error) = enqueue_archive(job, &job.root.join(&rel), 0) {
            job.summary.errors += 1;
            job.log("扫描", &rel, "", "跳过", &format!("{error:#}"), 0)?;
        }
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
        .map(|child| fsutil::fold_rel(&child.name))
        .collect();
    let mut seen: HashSet<(String, &'static str)> = HashSet::new();
    for child in children {
        let ScanKind::File { .. } = &child.kind else {
            continue;
        };
        let lower = child.name.to_ascii_lowercase();
        let Some(tail) = rules::old_style_tail(&lower) else {
            continue;
        };
        // 与确认清点及实际卷集识别一致：语法只折叠 ASCII，主干按 Windows 序数比较。
        if siblings.contains(&fsutil::fold_rel(&format!(
            "{}.{}",
            tail.stem, tail.main_ext
        ))) || !seen.insert((fsutil::fold_rel(tail.stem), tail.main_ext))
        {
            continue;
        }
        let rel = if parent.is_empty() {
            child.name.clone()
        } else {
            format!("{parent}/{}", child.name)
        };
        // 与 archive_name 入队同口径：入队失败计错误并跳过，不中断整个扫描。
        if let Err(error) = enqueue_archive(job, &job.root.join(&rel), 0) {
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
pub fn special_prefixes(
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
/// 预览格式化：取前 3 项以「、」连接，超出 3 项时追加「 等」（扫描日志共用）。
fn preview3(items: &[String]) -> String {
    let shown = items.iter().take(3).cloned().collect::<Vec<_>>().join("、");
    let more = if items.len() > 3 { " 等" } else { "" };
    format!("{shown}{more}")
}
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "scan", skip_all)
)]
pub fn scan(job: &mut Job, enqueue: bool, state: &Path, protected: Option<&Path>) -> Result<()> {
    let control = job.context.control.clone();
    observe("scan", Some(&control), |stage| {
        *stage = "directory_scan";
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
                "已排除 Windows 系统目录（系统目录及其全部内容不参与本次处理，也不会被改动）"
                    .into(),
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
            quarantine: Some(QUARANTINE_DIR_NAME),
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
        let scan_span = tracing::Span::current();
        pool.install(|| {
            rayon::scope(|scope| {
                scope.spawn(|scope| {
                    scan_span.in_scope(|| walk_dir(scope, &ctx, &root, String::new(), 0));
                });
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
        ensure_git_boundary_checked(sink.git_boundary_error.as_deref())?;
        sink.quarantine_skips.sort();
        sink.quarantine_skips.dedup();
        for rel in &sink.quarantine_skips {
            let shown = if rel.is_empty() {
                job.root.display().to_string()
            } else {
                rel.clone()
            };
            job.log(
                "扫描",
                rel,
                "",
                "提示",
                &format!("已跳过隔离目录「{shown}」：内容不参与本次处理；需要重试时请先移出"),
                0,
            )?;
        }
        sink.workspace_skips.sort();
        sink.workspace_skips.dedup();
        for rel in &sink.workspace_skips {
            job.log(
                "扫描",
                rel,
                "",
                "提示",
                "已跳过带所有权标记的解压工作区；未读取其内容，不自动清理其他任务的资料",
                0,
            )?;
        }
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
                "已识别 {} 个 Git 项目（含 .git 的目录整树保护：不读取内容、不进入树内改名/去重/清理；可归类项目整树移入本次所选根下的「Git项目集合」，已就位或占用根层固定容器的项目原位保留）：{shown}",
                git_roots.len()
            ),
            format!(
                "已识别 {} 个 Git 项目（未读取内容；已就位或占用固定容器的项目保留）",
                git_roots.len()
            ),
        )
            };
            job.log("扫描", "", "", "提示", &log_message, 0)?;
            job.context.emit(Event::Notice(notice));
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
            if !enqueue {
                // 盘点用 git_roots 是连接内临时表；重算计划需在重新打开任务库后
                // 知道哪些 C-14 Move 未完成，故同时持久化这份边界清单。
                job.db.set("git_roots", &git_roots)?;
            }
            let mut count = 0u64;
            scan_emit(job, &by_parent, enqueue, "", 1, &mut count)?;
            Ok::<_, anyhow::Error>(())
        })();
        match result {
            Ok(()) => job.db.conn.execute_batch("COMMIT")?,
            Err(error) => {
                if job.db.conn.execute_batch("ROLLBACK").is_err() {
                    tracing::warn!(
                        event = "task_transaction_cleanup_failed",
                        stage = "scan_rollback",
                        error_type = "database",
                        "扫描失败后的事务回滚失败"
                    );
                }
                return Err(error);
            }
        }
        Ok(())
    })
}
/// 扫描日志预览格式的特征测试：锁定 preview3 的「前 3 项、顿号连接、超出追加
/// 『 等』」口径（Git 整树提示使用同一格式）。
#[rustfmt::skip]
#[warn(
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
#[rustfmt::skip]
#[warn(
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
#[cfg(test)]
mod s05_scope_tests {
    use super::*;
    // 覆盖 S-05（受保护子树剪枝：命中相对前缀整树排除，兄弟目录不受影响）
    #[test]
    fn scope_filter_prunes_protected_subtree_only() {
        let excluded = rules::build_exclusions("").unwrap();
        let filter = ScopeFilter {
            excluded: &excluded,
            include_hidden: true,
            include_system: true,
            quarantine: Some(QUARANTINE_DIR_NAME),
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
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    #[test]
    fn failed_git_boundary_check_aborts_processing() {
        let error = ensure_git_boundary_checked(Some("access denied")).unwrap_err();
        assert!(error.to_string().contains("H-06"));
        assert!(ensure_git_boundary_checked(None).is_ok());
    }

    // 覆盖 X-02 / X-10 / 附录 B：扫描入队与确认清点使用同一序数忽略大小写主干。
    // Windows 专属原因：Unicode 名称等价按产品平台的 Windows 序数语义判定。
    #[cfg(windows)]
    #[test]
    fn old_style_volume_queue_uses_ordinal_stems() {
        for names in [
            ["Ä.zip", "ä.z01"],
            ["ä.z01", "Ä.z02"],
            ["Ä.rar", "ä.r00"],
            ["ä.r00", "Ä.r01"],
        ] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("data");
            let state = temp.path().join("state");
            fs::create_dir(&root).unwrap();
            for name in names {
                fs::write(root.join(name), b"original volume").unwrap();
            }
            let db = crate::db::Database::create(&state.join("tasks/probe")).unwrap();
            let mut job = Job {
                root: root.clone(),
                config: crate::config::Config::default(),
                context: crate::control::Context::default(),
                db,
                summary: crate::model::Summary::default(),
            };
            scan(&mut job, true, &state, None).unwrap();
            let queued: i64 = job
                .db
                .conn
                .query_row("SELECT COUNT(*) FROM archives", [], |row| row.get(0))
                .unwrap();
            assert_eq!(queued, 1, "同一老式分卷族只能入队一次：{names:?}");
            assert_eq!(job.summary.scanned, 2);
            assert_eq!(job.summary.errors, 0);
            for name in names {
                assert_eq!(fs::read(root.join(name)).unwrap(), b"original volume");
            }
        }
    }
}
