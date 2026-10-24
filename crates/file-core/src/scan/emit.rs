//! 扫描结果入库：scan_emit 按深度先序回放并行枚举产物写入任务库（含 S-01 固定
//! 容器占用保留），X-10 缺首卷/缺入口/老式尾卷组按代表卷入队。

use super::records::{ScanChild, ScanKind};
use crate::{
    fsutil, rules,
    task::{enqueue_archive, is_fixed_container_name, Job},
};
use anyhow::Result;
use std::collections::{HashMap, HashSet};

/// 深度先序回放：显式 DFS 栈保留同目录枚举序，目录退出时再归组尾卷。
/// 回放栈随目录深度增长在堆上，不让合法深链耗尽调用线程栈。
pub(super) fn scan_emit(
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
