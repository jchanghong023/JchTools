use crate::dedup_components::{find_duplicate_groups, DedupRules};
use anyhow::Result;
use jchtools_file_core::{
    config::DeleteMode,
    db::FILE_COLUMNS,
    model::{Action, ActionKind, FileRecord, Snapshot},
    rules,
    task::Job,
};
use std::collections::HashSet;
use std::path::Path;

pub(super) fn try_detect_extension(
    source: &Path,
    control: &jchtools_file_core::control::Control,
) -> Result<Option<&'static str>> {
    let detected = rules::detect_extension(source, control);
    control.check_cancelled()?;
    Ok(detected.ok().flatten())
}

pub(super) fn action(
    file: &FileRecord,
    kind: ActionKind,
    reason: &str,
    mode: DeleteMode,
) -> Action {
    Action {
        id: 0,
        kind,
        source: file.rel.clone(),
        target: None,
        reason: reason.into(),
        expected: Some(file.snapshot.clone()),
        keeper: None,
        hash: file.hash.clone(),
        mode,
        selected: true,
        state: "pending".into(),
    }
}

/// S-06：只合并可靠文件标识；未知/退化标识的不同项各计自己的逻辑大小。
pub(crate) fn count_logical_bytes(
    snapshot: Option<&Snapshot>,
    identities: &mut HashSet<String>,
) -> u64 {
    let Some(snapshot) = snapshot else {
        return 0;
    };
    let identity = &snapshot.identity;
    let reliable = if cfg!(windows) {
        let mut parts = identity.split(':');
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(volume), Some(high), Some(low), None) => {
                matches!(
                    (volume.parse::<u32>(), high.parse::<u32>(), low.parse::<u32>()),
                    (Ok(_), Ok(high), Ok(low)) if high != 0 || low != 0
                )
            }
            _ => false,
        }
    } else {
        identity.split_once(':').is_some_and(|(device, inode)| {
            device.parse::<u64>().is_ok() && inode.parse::<u64>().is_ok_and(|value| value != 0)
        })
    };
    if reliable && identities.contains(identity) {
        return 0;
    }
    if reliable {
        identities.insert(identity.clone());
    }
    snapshot.size
}

fn remove_candidate(
    job: &mut Job,
    file: &FileRecord,
    keeper: Option<&FileRecord>,
    reason: &str,
    mode: DeleteMode,
    identities: &mut HashSet<String>,
) -> Result<()> {
    // S-02/C-04：保留时不生成删除操作，副本仍按其余已启用规则参与归类和清理。
    if mode == DeleteMode::Keep {
        return Ok(());
    }
    let mut planned = action(file, ActionKind::Delete, reason, mode);
    if let Some(keeper) = keeper {
        planned.keeper = Some((keeper.rel.clone(), keeper.snapshot.clone()));
    }
    job.db.add_action(&planned)?;
    job.db.deactivate_file_id(file.id)?;
    job.summary.planned_delete += 1;
    job.summary.candidate_bytes = job
        .summary
        .candidate_bytes
        .saturating_add(count_logical_bytes(Some(&file.snapshot), identities));
    Ok(())
}

#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "plan_cleanup", skip_all)
)]
pub(crate) fn cleanup_candidates(job: &mut Job, identities: &mut HashSet<String>) -> Result<()> {
    let mut cursor = 0;
    loop {
        let batch = job.db.files(
            &format!(
                "SELECT {FILE_COLUMNS} FROM files WHERE id>?1 AND active=1 ORDER BY id LIMIT 256"
            ),
            [cursor],
        )?;
        if batch.is_empty() {
            break;
        }
        for file in batch {
            job.context.control.checkpoint()?;
            cursor = file.id;
            // C-01/C-08：多条清理规则同时命中时逐类裁决（各类独立覆盖删除方式，
            // 未覆盖时跟随全局）；任一已启用规则请求永久删除即形成一次清理删除，
            // 某类选择保留只取消该类的删除请求，不否决其他类别。
            let hits = rules::cleanup_hits(&file.rel, file.snapshot.size, &job.config);
            let requesting: Vec<&'static str> = hits
                .iter()
                .filter_map(|(kind, reason)| {
                    matches!(
                        rules::cleanup_delete(&job.config, *kind).resolve(job.config.global_delete),
                        DeleteMode::Permanent
                    )
                    .then_some(*reason)
                })
                .collect();
            if !requesting.is_empty() {
                // 计划列出实际导致删除的全部规则，不能只显示选择了保留的那项。
                let reason = requesting.join("；");
                remove_candidate(job, &file, None, &reason, DeleteMode::Permanent, identities)?;
            }
        }
    }
    Ok(())
}

#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "plan_dedup", skip_all)
)]
pub(super) fn deduplicate(job: &mut Job, identities: &mut HashSet<String>) -> Result<()> {
    if !job.config.dedup_same_name && !job.config.dedup_copy_names && !job.config.dedup_other_names
    {
        return Ok(());
    }
    job.context
        .status("分析相同内容：相同名称 / 副本名称 / 不同名称分别应用规则");
    // hash_candidates 在哈希前从 active 集合生成：只装载可能存在候选边的文件，
    // 同时保留哈希失败后被置为 inactive 的节点作为连通图桥接项。它们的 hash
    // 仍为空，绝不会成为保留者或删除目标。
    let records = job.db.files(
        &format!(
            "SELECT {} FROM hash_candidates AS c JOIN files AS f ON f.id=c.id WHERE f.active=1 OR f.hash IS NULL ORDER BY f.id",
            jchtools_file_core::db::file_columns_qualified("f")
        ),
        [],
    )?;
    let groups = find_duplicate_groups(
        &records,
        DedupRules {
            same_name: job.config.dedup_same_name,
            copy_names: job.config.dedup_copy_names,
            other_names: job.config.dedup_other_names,
        },
        job.config.keep_duplicate,
    );
    // C-04：既有硬链接保护由扫描标识决定，不依赖组内最终保留者。
    // 保留者是独立文件时，同物理文件的两个目录项仍不得作为独立副本删除。
    // 另一链接可能不在启用的名称候选中；只查询既有扫描记录，不扩大哈希范围。
    let needs_link_check = groups.iter().any(|group| {
        group
            .duplicates
            .iter()
            .any(|&index| records[index].snapshot.links >= 2)
    });
    let linked_identities: HashSet<String> = if needs_link_check {
        let mut statement = job.db.conn.prepare(
            "SELECT identity FROM files WHERE links>=2 AND (active=1 OR id IN (SELECT id FROM hash_candidates)) GROUP BY identity HAVING COUNT(*)>1",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut identities = HashSet::new();
        for identity in rows {
            let identity = identity?;
            if cfg!(windows) && jchtools_file_core::hash_cache::identity_is_degenerate(&identity) {
                continue;
            }
            identities.insert(identity);
        }
        identities
    } else {
        HashSet::new()
    };
    for group in groups {
        let keeper = &records[group.keeper];
        for duplicate_index in group.duplicates {
            let file = &records[duplicate_index];
            job.context.control.checkpoint()?;
            // 仅可靠标识与两侧链接数共同证明的目录项才豁免；退化标识不能缩小去重范围。
            if file.snapshot.links >= 2
                && linked_identities.contains(file.snapshot.identity.as_str())
            {
                job.log(
                    "去重",
                    &file.rel,
                    "",
                    "保留",
                    "已可靠识别为既有硬链接，不作为独立副本删除",
                    file.snapshot.size,
                )?;
                continue;
            }
            let reason = if file.name == keeper.name {
                "相同名称且完整 Hash 相同"
            } else if file.normalized == keeper.normalized {
                "副本命名且完整 Hash 相同"
            } else {
                "名称不同但完整 Hash 相同"
            };
            // C-04/S-02：副本处置只有「保留副本」和「永久删除副本」，随全局删除方式。
            remove_candidate(
                job,
                file,
                Some(keeper),
                reason,
                job.config.global_delete,
                identities,
            )?;
        }
    }
    Ok(())
}
