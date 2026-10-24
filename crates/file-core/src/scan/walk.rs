//! 并行目录枚举：IO 线程池 io_pool、WalkCtx 与 walk_dir——rayon scope 递归下钻，
//! 内含 Git 边界（H-06）、隔离容器与解压工作区整树判定，跳过与污点记入 ScanSink。

use super::records::{lock_sink, ScanChild, ScanKind, ScanNote, ScanSink};
use super::scope::ScopeFilter;
use crate::{control::Control, fsutil, rules, task::root_is_quarantine};
use anyhow::{Context as _, Result};
use std::{
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

pub(super) struct WalkCtx<'a> {
    pub(super) control: Arc<Control>,
    pub(super) scope: &'a ScopeFilter<'a>,
    pub(super) recursive: bool,
    pub(super) sink: &'a Mutex<ScanSink>,
}
/// 单个目录的枚举与登记（在 IO 池线程上运行）。过滤口径与旧 walkdir filter_entry
/// 一致；子目录下钻时首个内联、其余 spawn。内联深度设上限：极深树上避免
/// 任务在偷取执行时栈随树深增长；超限的子目录全部走 spawn（在全新栈帧上执行）。
pub(super) fn walk_dir<'a>(
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
