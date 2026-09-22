use crate::{
    config::DeleteMode,
    db::FILE_COLUMNS,
    engine::Job,
    fsutil,
    model::{Action, ActionKind, FileRecord},
    rules,
};
use anyhow::{Context, Result};
use chrono::{Datelike, Offset};
use rusqlite::{params, OptionalExtension};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

/// C-14 固定集合容器名（建在本次所选根下）。
pub const GIT_COLLECTION_DIR: &str = "Git项目集合";

fn action(file: &FileRecord, kind: ActionKind, reason: &str, mode: DeleteMode) -> Action {
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
fn remove_candidate(
    job: &mut Job,
    file: &FileRecord,
    keeper: Option<&FileRecord>,
    reason: &str,
    mode: DeleteMode,
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
    // Already-hardlinked files do not represent distinct physical allocation.
    if file.snapshot.links <= 1 {
        job.summary.candidate_bytes = job
            .summary
            .candidate_bytes
            .saturating_add(file.snapshot.size);
    }
    Ok(())
}
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "plan_cleanup", skip_all)
)]
fn cleanup_candidates(job: &mut Job) -> Result<()> {
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
            if let Some((kind, reason)) =
                rules::cleanup_reason(&file.rel, file.snapshot.size, &job.config)
            {
                // C-08：三类清理各自独立覆盖删除方式，未覆盖时跟随全局文件删除方式。
                let mode =
                    rules::cleanup_delete(&job.config, kind).resolve(job.config.global_delete);
                remove_candidate(job, &file, None, reason, mode)?;
            }
        }
    }
    Ok(())
}
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "plan_dedup", skip_all)
)]
fn deduplicate(job: &mut Job) -> Result<()> {
    if !job.config.dedup_same_name && !job.config.dedup_copy_names && !job.config.dedup_other_names
    {
        return Ok(());
    }
    job.context
        .status("分析相同内容：相同名称 / 副本名称 / 不同名称分别应用规则");
    let order = rules::ordering_sql(job.config.keep_duplicate);
    job.db.conn.execute_batch(&format!("DROP TABLE IF EXISTS duplicate_order; CREATE TEMP TABLE duplicate_order AS SELECT ROW_NUMBER() OVER(ORDER BY hash,{order}) AS seq,id FROM files WHERE active=1 AND hash IS NOT NULL; CREATE INDEX duplicate_order_seq ON duplicate_order(seq); DELETE FROM keepers;"))?;
    let mut cursor = 0i64;
    loop {
        // 一条 JOIN 语句取整页（duplicate_order 游标 × files 全列），替代逐候选的
        // file(id) 主键单行查询；keeper 查找保留逐行——它依赖本页内已注册的 keepers。
        // 写语句无需页内事务：engine 在 planner::build 外层已包一个整体事务。
        let items = job.db.duplicate_page(cursor, 256)?;
        if items.is_empty() {
            break;
        }
        for (seq, file) in items {
            cursor = seq;
            job.context.control.checkpoint()?;
            let hash = file.hash.as_ref().context("重复候选缺少 Hash")?;
            let keeper_id: Option<i64> = {
                let mut statement = job.db.conn.prepare_cached(
                    "SELECT file_id FROM keepers WHERE hash=?1 AND ((name=?2 AND ?4) OR (name<>?2 AND normal=?3 AND ?5) OR (name<>?2 AND normal<>?3 AND ?6)) ORDER BY rowid LIMIT 1")?;
                statement
                    .query_row(
                        params![
                            hash,
                            file.name,
                            file.normalized,
                            job.config.dedup_same_name,
                            job.config.dedup_copy_names,
                            job.config.dedup_other_names
                        ],
                        |r| r.get(0),
                    )
                    .optional()?
            };
            if let Some(keeper_id) = keeper_id {
                let keeper = job.db.file(keeper_id)?;
                // C-04：只有可靠标识 + 两侧链接数证明是同一物理文件时才跳过；标识退化
                // （如 Windows 卷不提供索引）时不得据此跳过去重，也不得重复计数。
                if rules::identity_proves_same_file(&keeper, &file) {
                    job.log(
                        "去重",
                        &file.rel,
                        &keeper.rel,
                        "保留",
                        "已经是同一个文件的硬链接，不重复计算可释放空间",
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
                // C-04/S-02：副本处置只有「保留副本」和「永久删除副本」，随全局文件删除方式。
                let mode = job.config.global_delete;
                // 清理命中且该类清理的删除方式为「保留」的文件由清理规则管辖（保留承诺）：
                // cleanup 阶段已让其保持 active，这里若无守卫，同组 keeper 先注册时它会按
                // 全局方式被删，结果随排序翻转。
                if rules::cleanup_reason(&file.rel, file.snapshot.size, &job.config).is_some() {
                    job.log(
                        "去重",
                        &file.rel,
                        &keeper.rel,
                        "跳过",
                        "文件命中清理规则且清理方式为保留；不按重复规则删除",
                        file.snapshot.size,
                    )?;
                    continue;
                }
                remove_candidate(job, &file, Some(&keeper), reason, mode)?;
            } else if rules::cleanup_reason(&file.rel, file.snapshot.size, &job.config).is_none() {
                // 清理命中文件即使该类的删除方式为「保留」（remove_candidate 直接返回、文件仍 active=1）
                // 也不得进入 keepers 成为去重唯一保留者：否则正常副本反被删除，只留下垃圾文件。
                job.db
                    .insert_keeper(file.id, hash, &file.name, &file.normalized)?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// C-05 / C-14 / C-16～C-21：固定归类与统一冲突消解
// ---------------------------------------------------------------------------

/// Windows 序数忽略大小写的近似折叠（与库内 targets/directories 折叠口径一致；
/// 非 Windows 平台大小写敏感）。
fn fold(name: &str) -> String {
    if cfg!(windows) {
        name.to_lowercase()
    } else {
        name.to_string()
    }
}
fn parent_of(rel: &str) -> &str {
    match rel.rfind('/') {
        Some(index) => &rel[..index],
        None => "",
    }
}
fn same_dir(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.to_lowercase() == b.to_lowercase()
    } else {
        a == b
    }
}
fn same_component(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.to_lowercase() == b.to_lowercase()
    } else {
        a == b
    }
}
/// 附录 A 大类名 + 「大文件」的固定容器集合（C-18 来源链识别用）。
fn is_category_label(name: &str) -> bool {
    matches!(
        name,
        "视频" | "音频" | "图片" | "文档" | "压缩包" | "程序" | "其他" | "大文件"
    )
}
fn is_year(text: &str) -> bool {
    text.len() == 4 && text.bytes().all(|b| b.is_ascii_digit())
}
fn is_month(text: &str) -> bool {
    text.len() == 2
        && text.bytes().all(|b| b.is_ascii_digit())
        && text.parse::<u8>().is_ok_and(|m| (1..=12).contains(&m))
}
/// C-18 来源段：parent 各目录段（最近一级在前），剔除紧邻的标准分类链
///（附录 A 大类或大文件/四位年/两位月；普通文件要求年月与该项归类时间一致，项目不比对）、
/// 以及项目来源的直接集合容器「Git项目集合」。段按已开启的 NFC/空白规则规范化（仅普通文件），
/// 不剥副本标记、不改源目录。
fn source_levels(
    parent: &str,
    date_chain: Option<(&str, &str)>,
    drop_collection: bool,
    normalize: bool,
) -> Vec<String> {
    let mut segments: Vec<String> = parent
        .split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if segments.len() >= 3 {
        let count = segments.len();
        let (cat, year, month) = (
            segments[count - 3].clone(),
            segments[count - 2].clone(),
            segments[count - 1].clone(),
        );
        let chain_matches = is_category_label(&cat)
            && is_year(&year)
            && is_month(&month)
            && date_chain.is_none_or(|(y, m)| y == year && m == month);
        if chain_matches {
            segments.truncate(count - 3);
        }
    }
    if drop_collection {
        if let Some(last) = segments.last() {
            if same_component(last, GIT_COLLECTION_DIR) {
                segments.pop();
            }
        }
    }
    // 最近一级在前（k=1 只加最近的父目录；level_prefix 拼接时再转回外→内顺序）。
    segments
        .into_iter()
        .rev()
        .map(|segment| {
            if normalize {
                rules::normalize_stem(&segment)
            } else {
                segment
            }
        })
        .filter(|segment| !segment.is_empty())
        .collect()
}
/// C-18 第 k 级候选前缀：最近 k 个来源段按外→内顺序以 `_` 连接（华东_客户A_正式版_方案.pdf）。
fn level_prefix(sources: &[String], k: usize) -> String {
    let take = sources.len().min(k);
    let mut parts: Vec<&str> = sources[..take].iter().map(String::as_str).collect();
    parts.reverse();
    parts.join("_")
}
/// C-05 归类时间：创建时间优先，缺失回落修改时间；以分析开始时的系统本地时区解释，
/// 年四位 / 月两位；无法表示时返回 None（该项不归类，保留并显示原因）。
fn year_month(stamp_ns: i64, offset: chrono::FixedOffset) -> Option<(String, String)> {
    let seconds = stamp_ns.div_euclid(1_000_000_000);
    let subsec = u32::try_from(stamp_ns.rem_euclid(1_000_000_000)).unwrap_or(0);
    let local = chrono::DateTime::from_timestamp(seconds, subsec)?.with_timezone(&offset);
    let year = local.year();
    if !(1..=9999).contains(&year) {
        return None;
    }
    Some((format!("{year:04}"), format!("{:02}", local.month())))
}

/// 一个待定位项（存活文件或 Git 项目目录）。
struct Item {
    id: i64,
    rel: String,
    current_name: String,
    stem: String,
    extension: String,
    target_dir: String,
    sources: Vec<String>,
    /// 已位于目标目录内（C-21 按位置判定）。
    in_place: bool,
    /// 派生名与当前名一致且组内唯一：保持名称与位置，不参与消解、名作固定占用。
    settled: bool,
    /// 消解失败原因（该项保留源项）。
    failed: Option<String>,
    /// 当前候选完整名。
    candidate: String,
    /// 当前来源级数（C-18 的 k；对来源不足的项按可用级数封顶）。
    k: usize,
    /// 摘要阶段（0=未进入；1..=29 对应 8,10,…,64 位十六进制）。
    digest_step: u32,
    /// C-20 长度触发时已去掉来源段；后续摘要扩展保持无来源形式。
    digest_no_source: bool,
    /// C-19 稳定序号兜底（摘要 64 位后追加 _N）。
    index_suffix: Option<u32>,
}
impl Item {
    fn derived(&self) -> String {
        format!("{}{}", self.stem, self.extension)
    }
    fn source_parent(&self) -> &str {
        parent_of(&self.rel)
    }
    fn digest_candidate(&self, hex_units: usize, no_source: bool) -> Option<String> {
        let digest = rules::path_digest(&self.rel, hex_units);
        let nearest = if no_source {
            None
        } else {
            self.sources.first().map(String::as_str)
        };
        // 序号由 rules::digest_candidate 计入长度预算并插在扩展名之前（H-07 同口径）。
        rules::digest_candidate(
            nearest,
            &self.stem,
            &digest,
            &self.extension,
            self.index_suffix,
        )
    }
    /// 当前阶段的候选名（C-18 来源前缀 / C-19 摘要 / C-20 长度受限形式）。
    fn current_candidate(&self) -> Option<String> {
        if self.digest_step == 0 {
            let derived = self.derived();
            let take = self.k.min(self.sources.len());
            if take == 0 {
                return Some(derived);
            }
            let prefix = level_prefix(&self.sources, take);
            let candidate = format!("{prefix}_{derived}");
            // C-20：逐层候选一旦超过 40 单元立即停止加来源，转无来源段的摘要形式。
            if candidate.encode_utf16().count() > rules::CONFLICT_YIELD_UNITS {
                return self.digest_candidate(8, true);
            }
            return Some(candidate);
        }
        let hex_units = usize::try_from((6 + 2 * self.digest_step).min(64)).unwrap_or(64);
        self.digest_candidate(hex_units, self.digest_no_source)
    }
}
/// 单个目标目录的占用与可用性。
struct DirPlan {
    /// 不参与本次操作的占用（fold 后）：范围外磁盘条目、保持原名的已就位项。
    fixed: HashSet<String>,
    /// 容器不可用（被文件/链接/读取失败占用）：依赖它的项全部失败（S-01）。
    blocked: Option<String>,
}
impl DirPlan {
    fn new() -> Self {
        Self {
            fixed: HashSet::new(),
            blocked: None,
        }
    }
}

/// 收集目标目录的磁盘占用：范围外条目与已就位保留项为固定占用；活动项的当前名
/// 执行后会腾空，不算占用。`moved_roots` 内的目录整树将在执行期先行移走，其内部
/// 条目不构成占用（C-14 先移动项目，分类目录后创建）。
fn gather_dir_plans(job: &Job, items: &[Item], moved_roots: &[String]) -> HashMap<String, DirPlan> {
    let mut dirs: HashMap<String, DirPlan> = HashMap::new();
    let mut by_dir: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, item) in items.iter().enumerate() {
        if item.failed.is_some() {
            continue;
        }
        by_dir
            .entry(item.target_dir.as_str())
            .or_default()
            .push(index);
    }
    for (dir, members) in by_dir {
        let mut plan = DirPlan::new();
        let dir_folded = fold(dir);
        let under_moved_root = moved_roots.iter().any(|root| {
            let root_folded = fold(root);
            dir_folded == root_folded || dir_folded.starts_with(&format!("{root_folded}/"))
        });
        if !under_moved_root {
            // 活动且当前就在该目录里的项：当前名执行后会腾空，从占用中排除。
            let freeing: HashSet<String> = members
                .iter()
                .filter(|&&i| !items[i].settled && same_dir(items[i].source_parent(), dir))
                .map(|&i| fold(&items[i].current_name))
                .collect();
            match fsutil::safe_join(&job.root, dir) {
                Ok(path) => match std::fs::read_dir(&path) {
                    Ok(entries) => {
                        for entry in entries.flatten() {
                            if let Some(name) = entry.file_name().to_str() {
                                let folded = fold(name);
                                if !freeing.contains(&folded) {
                                    plan.fixed.insert(folded);
                                }
                            }
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        // NotFound 可能只是“目录尚未创建”，也可能是某个祖先已被普通
                        // 文件占用（穿过文件组件报路径未找到）：逐段核对，S-01 拒绝把
                        // 分类目录建到文件之下。
                        if let Some(reason) = blocked_by_file_ancestor(&job.root, dir) {
                            plan.blocked = Some(reason);
                        }
                    }
                    Err(error) => {
                        plan.blocked = Some(format!(
                            "目标目录存在但无法读取（可能被同名文件占用）：{error}"
                        ));
                    }
                },
                Err(error) => plan.blocked = Some(format!("目标路径不可用：{error:#}")),
            }
        }
        dirs.insert(dir.to_string(), plan);
    }
    dirs
}

/// 目标目录的祖先链上是否存在普通文件占位（目录无法创建，S-01）。
/// 只报告“已存在且不是目录”的组件；尚不存在的组件视为执行期可创建。
fn blocked_by_file_ancestor(root: &Path, dir: &str) -> Option<String> {
    let mut current = root.to_path_buf();
    for segment in dir.split('/') {
        current.push(segment);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if !meta.is_dir() => {
                return Some(format!(
                    "固定容器被普通文件占用（{}）：不删除、不挪走占用项",
                    current.display()
                ))
            }
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

/// C-17 统一冲突消解：全部活动候选项互不冲突、且不撞任何固定占用为止；
/// 不同活动组互撞时合并碰撞集合统一继续，不按扫描顺序抢名。
fn resolve_all(items: &mut [Item], dirs: &mut HashMap<String, DirPlan>) {
    for item in items.iter_mut() {
        item.candidate = item.derived();
    }
    // 按目标目录 + 派生名分组决定「已就位保持原名」（C-17 / C-21 / 附录 E）：
    // 组内恰好一个「已就位且派生名与当前名一致」的项 → 它保持原名并作为固定占用，
    // 其余项退让；两个及以上这样的已就位项 → 全部统一消解；没有 → 全部活动。
    {
        let mut groups: HashMap<(String, String), Vec<usize>> = HashMap::new();
        for (index, item) in items.iter().enumerate() {
            if item.failed.is_some() {
                continue;
            }
            groups
                .entry((item.target_dir.clone(), fold(&item.derived())))
                .or_default()
                .push(index);
        }
        for members in groups.values() {
            let capable: Vec<usize> = members
                .iter()
                .copied()
                .filter(|&i| items[i].in_place && items[i].derived() == items[i].current_name)
                .collect();
            if let Some(&only) = capable.first().filter(|_| capable.len() == 1) {
                items[only].settled = true;
            }
        }
    }
    // 已就位保留项的名称是固定占用（保护既有项优先于任何候选）。
    for item in items.iter().filter(|i| i.settled && i.failed.is_none()) {
        if let Some(plan) = dirs.get_mut(&item.target_dir) {
            plan.fixed.insert(fold(&item.candidate));
        }
    }
    // C-17/C-18 逐轮消解：碰撞簇里仍有可用来源级数的成员先加前缀（k+1），
    // 来源已用尽的成员本轮等待——给同伴腾位的时机；当碰撞簇全部成员都用尽来源
    // 仍撞名（或撞固定占用）时，这些成员进入 C-19 摘要阶梯（8,10,…,64 位，再到
    // 稳定序号兜底）。这样“x/合同 + y/合同 + 根/合同”在 k=1 即互不相同：根文件
    // 无来源、保持原名，x/y 加前缀（C-18 最少级数，不按扫描顺序抢名）。
    let mut rounds = 0usize;
    loop {
        rounds += 1;
        if rounds > 1_000_000 {
            // 每项阶梯有限且序号兜底单调递增，正常必然收敛；此处防御性终止。
            break;
        }
        let mut by_slot: HashMap<(String, String), Vec<usize>> = HashMap::new();
        for (index, item) in items.iter().enumerate() {
            if item.failed.is_some() || item.settled {
                continue;
            }
            by_slot
                .entry((item.target_dir.clone(), fold(&item.candidate)))
                .or_default()
                .push(index);
        }
        let mut escalate_prefix: Vec<usize> = Vec::new();
        let mut escalate_digest: Vec<usize> = Vec::new();
        for ((dir, name), members) in &by_slot {
            let fixed_hit = dirs.get(dir).is_some_and(|plan| plan.fixed.contains(name));
            if members.len() > 1 || fixed_hit {
                // 仍有来源级数可加的成员先加前缀；其余本轮不动，等同伴分化。
                let advancing: Vec<usize> = members
                    .iter()
                    .copied()
                    .filter(|&i| items[i].digest_step == 0 && items[i].k < items[i].sources.len())
                    .collect();
                if advancing.is_empty() {
                    escalate_digest.extend(members.iter().copied());
                } else {
                    escalate_prefix.extend(advancing);
                }
            }
        }
        if escalate_prefix.is_empty() && escalate_digest.is_empty() {
            break;
        }
        for index in escalate_prefix {
            items[index].k += 1;
            let no_source = items[index].digest_no_source;
            let item = &mut items[index];
            match item.current_candidate() {
                Some(candidate) => item.candidate = candidate,
                None => {
                    item.failed = Some("无法生成合法的冲突消解名称（名称与扩展名过长）".into());
                }
            }
            let _ = no_source;
        }
        for index in escalate_digest {
            let item = &mut items[index];
            if item.digest_step == 0 {
                // 首次进入摘要：保留最近来源（C-19 完整形式）；C-20 超长时内部去源。
                item.digest_step = 1;
            } else if item.digest_step < 29 {
                item.digest_step += 1;
            } else {
                // 摘要 64 位仍冲突：稳定序号兜底，逐次递增直到可用。
                item.index_suffix = Some(item.index_suffix.map_or(1, |n| n + 1));
            }
            match item.current_candidate() {
                Some(candidate) => item.candidate = candidate,
                None => {
                    item.failed = Some("无法生成合法的冲突消解名称（名称与扩展名过长）".into());
                }
            }
        }
    }
}

/// C-14：Git 项目整体移入所选根下的「Git项目集合」。
/// 返回已计划移动的项目根列表（供文件归类阶段忽略其内部占用）。
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "plan_git_collection", skip_all)
)]
fn git_collection(job: &mut Job) -> Result<Vec<String>> {
    let roots: Vec<String> = {
        let mut statement = job
            .db
            .conn
            .prepare("SELECT rel FROM git_roots ORDER BY rel")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut items: Vec<Item> = Vec::new();
    for rel in &roots {
        // 已位于本次所选根的「Git项目集合」下：不再移动（C-10 幂等 / C-14）。
        if same_component(parent_of(rel), GIT_COLLECTION_DIR) {
            continue;
        }
        let Some(name) = Path::new(rel).file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let parent = parent_of(rel).to_string();
        // 项目来源排除分类层级与直接集合容器（C-14），不比对日期（项目无归类时间）。
        let sources = source_levels(&parent, None, true, false);
        items.push(Item {
            id: 0,
            rel: rel.clone(),
            current_name: name.to_string(),
            stem: name.to_string(),
            extension: String::new(),
            target_dir: GIT_COLLECTION_DIR.to_string(),
            sources,
            in_place: false,
            settled: false,
            failed: None,
            candidate: String::new(),
            k: 0,
            digest_step: 0,
            digest_no_source: false,
            index_suffix: None,
        });
    }
    if items.is_empty() {
        // 没有待移入项目：不创建集合目录，全部项目保持 staying。
        return Ok(Vec::new());
    }
    let mut dirs = gather_dir_plans(job, &items, &[]);
    // 集合容器被文件/链接占用：全部项目保留原位（S-01，不挪走占用项）。
    if let Some(reason) = dirs
        .get(GIT_COLLECTION_DIR)
        .and_then(|plan| plan.blocked.clone())
    {
        for item in &mut items {
            item.failed = Some(reason.clone());
        }
    }
    resolve_all(&mut items, &mut dirs);
    let mut moved: Vec<String> = Vec::new();
    for item in &items {
        if let Some(reason) = &item.failed {
            job.log(
                "Git归类",
                &item.rel,
                "",
                "失败",
                &format!("{reason}；原项目保留在原位置"),
                0,
            )?;
            job.summary.errors += 1;
            continue;
        }
        let target = format!("{GIT_COLLECTION_DIR}/{}", item.candidate);
        if let Err(error) = fsutil::safe_relative(&target) {
            job.log(
                "Git归类",
                &item.rel,
                "",
                "失败",
                &format!("目标名不合法，原项目保留：{error}"),
                0,
            )?;
            job.summary.errors += 1;
            continue;
        }
        let planned = Action {
            id: 0,
            kind: ActionKind::Move,
            source: item.rel.clone(),
            target: Some(target.clone()),
            reason: "Git 项目整体移入「Git项目集合」（C-14：不进入项目内部）".into(),
            expected: None,
            keeper: None,
            hash: None,
            mode: DeleteMode::Keep,
            selected: true,
            state: "pending".into(),
        };
        job.db.add_action(&planned)?;
        job.db.reserve_target(&target, 0)?;
        job.summary.planned_move += 1;
        job.summary.planned_git += 1;
        moved.push(item.rel.clone());
    }
    if !moved.is_empty() {
        job.log(
            "Git归类",
            "",
            "",
            "提示",
            &format!(
                "识别到 {} 个 Git 项目，其中 {} 个将整体移入「{}」（其余项目保持原位）；项目内部不做任何处理",
                roots.len(),
                moved.len(),
                GIT_COLLECTION_DIR
            ),
            0,
        )?;
    }
    Ok(moved)
}

/// C-05 / C-16～C-21：存活文件的固定归类与命名。
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "plan_classify", skip_all)
)]
fn classify_files(job: &mut Job, moved_roots: &[String]) -> Result<()> {
    job.context
        .status("按「大类/创建年/创建月」确定存活文件的归类目标与最终名称");
    // C-05：日期采用分析开始时的系统本地时区。
    let local_offset = chrono::Local::now().offset().fix();
    let _ = &local_offset;
    let mut items: Vec<Item> = Vec::new();
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
            let Some(current_name) = Path::new(&file.rel)
                .file_name()
                .and_then(|value| value.to_str())
            else {
                continue;
            };
            let (stem, extension) = rules::derive_stem_ext(current_name, &job.config);
            let mut extension = extension;
            // C-08 内容签名修正（默认关）：只在分析阶段判定最终扩展名（C-01）。
            if job.config.fix_extension && !(stem.starts_with('.') && extension.is_empty()) {
                if let Ok(source) = fsutil::safe_join(&job.root, &file.rel) {
                    if let Ok(Some(kind)) = infer::get_from_path(&source) {
                        let old = extension.trim_start_matches('.').to_lowercase();
                        if rules::extension_needs_fix(&old, kind.extension()) {
                            extension = format!(".{}", kind.extension());
                            job.log(
                                "类型检测",
                                &file.rel,
                                "",
                                "发现",
                                &format!(
                                    "扩展名 {old}，内容识别为 {}，将按识别结果修正",
                                    kind.extension()
                                ),
                                file.snapshot.size,
                            )?;
                        }
                    }
                }
            }
            // 合法性处理只作用于派生名（附录 B）。
            let stem = rules::legalize_derived(&stem);
            let derived_name = format!("{stem}{extension}");
            if let Err(error) = fsutil::validate_component(&derived_name) {
                job.log(
                    "归类",
                    &file.rel,
                    "",
                    "失败",
                    &format!("派生名称不合法，保留源项：{error}"),
                    0,
                )?;
                job.summary.errors += 1;
                continue;
            }
            let stamp_ns = file
                .snapshot
                .created_ns
                .unwrap_or(file.snapshot.modified_ns);
            let Some((year, month)) = year_month(stamp_ns, local_offset) else {
                // C-05：两个时间都不可用或无法表示时不归类，保留并显示原因。
                job.log(
                    "归类",
                    &file.rel,
                    "",
                    "失败",
                    "创建时间与修改时间均不可用或超出可表示范围；该项不归类，保留源项",
                    0,
                )?;
                job.summary.errors += 1;
                continue;
            };
            let category = if job.config.large_files
                && file.snapshot.size >= job.config.large_threshold_bytes
            {
                "大文件"
            } else {
                rules::category_for(&derived_name.to_lowercase())
            };
            let target_dir = format!("{category}/{year}/{month}");
            let parent = parent_of(&file.rel).to_string();
            let in_place = same_dir(&parent, &target_dir);
            let sources = source_levels(
                &parent,
                Some((&year, &month)),
                false,
                job.config.normalize_names,
            );
            items.push(Item {
                id: file.id,
                rel: file.rel.clone(),
                current_name: current_name.to_string(),
                stem,
                extension,
                target_dir,
                sources,
                in_place,
                settled: false,
                failed: None,
                candidate: String::new(),
                k: 0,
                digest_step: 0,
                digest_no_source: false,
                index_suffix: None,
            });
        }
    }
    let mut dirs = gather_dir_plans(job, &items, moved_roots);
    // 容器被占用的目录：依赖它的项全部失败并保留源项（S-01）。
    let blocked: Vec<(String, String)> = dirs
        .iter()
        .filter_map(|(dir, plan)| {
            plan.blocked
                .as_ref()
                .map(|reason| (dir.clone(), reason.clone()))
        })
        .collect::<Vec<_>>();
    for (dir, reason) in &blocked {
        for item in &mut items {
            if item.target_dir == *dir {
                item.failed = Some(reason.clone());
            }
        }
    }
    resolve_all(&mut items, &mut dirs);
    for item in &items {
        if let Some(reason) = &item.failed {
            job.log(
                "归类",
                &item.rel,
                "",
                "失败",
                &format!("{reason}；已保留源项"),
                0,
            )?;
            job.summary.errors += 1;
            continue;
        }
        if item.settled {
            continue;
        }
        let target = format!("{}/{}", item.target_dir, item.candidate);
        // 候选回落到自身当前名与当前目录：无需移动（防御性，正常不会出现）。
        if same_dir(&item.target_dir, item.source_parent())
            && same_component(&item.candidate, &item.current_name)
        {
            continue;
        }
        if let Err(error) = fsutil::safe_relative(&target) {
            job.log(
                "归类",
                &item.rel,
                "",
                "失败",
                &format!("目标路径不合法，保留源项：{error}"),
                0,
            )?;
            job.summary.errors += 1;
            continue;
        }
        let file = job.db.file(item.id)?;
        let mut planned = action(
            &file,
            ActionKind::Move,
            "按「大类/创建年/创建月」归类（同名冲突已统一消解；目标不覆盖）",
            DeleteMode::Keep,
        );
        planned.target = Some(target.clone());
        job.db.add_action(&planned)?;
        job.db.reserve_target(&target, item.id)?;
        job.summary.planned_move += 1;
    }
    Ok(())
}

#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "plan_empty_dirs", skip_all)
)]
fn empty_directories(job: &mut Job, moved_roots: &[String]) -> Result<()> {
    // C-07/H-05：空目录清理是强制步骤——没有开关，也不受文件删除方式（全局或按类别覆盖）
    // 影响；删除对象只可能是执行期复查后实际为空的目录，不涉及任何文件内容。
    // 递归关闭时子目录内容未知（扫描未下钻），不得据库内条目判定为空目录。
    if !job.config.recursive {
        return Ok(());
    }
    let mode = DeleteMode::Permanent;
    // 自底向上推算：只把“计划执行后仍会为空”的目录写进计划。
    // 目录为空 = 其下没有会留在原地的文件，且其子目录也都为空。
    // 「会留在原地」= 磁盘上仍会存在：没有选中的删除/移动。分卷源、失败包等 protected 文件
    // 虽 active=0，但仍占目录，不能被算成空目录。
    //
    // 性能：旧实现对每个目录跑三次 `rel LIKE '前缀%'` 全表扫描，目录多的树上会到
    // O(目录数×文件数)。这里预先把文件/目录按“父目录”物化成带索引的临时表，全部
    // 查询退化为等值查找；配合自底向上的处理顺序，深层留驻文件会通过“子目录不在
    // empty_will”逐层向上传播，结果与按全部后代判断完全一致。
    let mut cursor = 0i64;
    // kind 在库里是 serde_json 序列化的枚举字符串，只可能是这两个值之一，直接内联安全。
    let move_kind = serde_json::to_string(&ActionKind::Move)?;
    let delete_kind = serde_json::to_string(&ActionKind::Delete)?;
    job.db.conn.execute_batch(&format!(
        "DROP TABLE IF EXISTS empty_order; DROP TABLE IF EXISTS empty_will;
         DROP TABLE IF EXISTS stay_parents; DROP TABLE IF EXISTS dir_children;
         DROP TABLE IF EXISTS doomed_sources; DROP TABLE IF EXISTS tainted_will;
         CREATE TEMP TABLE doomed_sources (rel TEXT PRIMARY KEY);
         INSERT OR IGNORE INTO doomed_sources SELECT source FROM actions
          WHERE kind IN ('{move_kind}','{delete_kind}') AND selected=1 AND state='pending';
         CREATE TEMP TABLE stay_parents (parent TEXT);
         INSERT INTO stay_parents SELECT rtrim(rtrim(rel,replace(rel,'/','')),'/') FROM files
          WHERE NOT EXISTS (SELECT 1 FROM doomed_sources WHERE doomed_sources.rel=files.rel);
         CREATE INDEX stay_parents_parent ON stay_parents(parent);
         CREATE TEMP TABLE dir_children (parent TEXT, rel TEXT PRIMARY KEY);
         INSERT INTO dir_children SELECT rtrim(rtrim(rel,replace(rel,'/','')),'/'),rel FROM directories;
         CREATE INDEX dir_children_parent ON dir_children(parent);
         CREATE TEMP TABLE empty_will (rel TEXT PRIMARY KEY);
         CREATE TEMP TABLE tainted_will (rel TEXT PRIMARY KEY);"))?;
    job.db.conn.execute_batch(
        // seq 是本表唯一的游标列，CREATE TABLE AS SELECT 不会继承任何约束或索引；
        // 缺索引时分页会退化成「每次重扫全表 + 临时 B 树排序」（实测 20 万目录 4.9s vs 0.07s）。
        "CREATE TEMP TABLE empty_order AS SELECT ROW_NUMBER() OVER(ORDER BY depth DESC,rel) seq,rel FROM directories;
         CREATE INDEX empty_order_seq ON empty_order(seq);")?;
    // stay_parents 只覆盖「无 Delete/Move、执行后仍在原地」的文件。同目录改名
    // （A/x → A/y）与迁入新建子目录（A/x → A/分类/x，分类在规划时可能尚未入库、
    // 不在 directories 表里）会让 source 带 Move 而离开 stay_parents，若只看
    // stay_parents 会把仍被占用的源目录误标进 empty_will。预取全部待执行 Move
    // 的目标：凡落点在该目录（或其子树）下的，执行后该目录仍非空。
    let mut move_targets: Vec<String> = {
        let mut stmt = job.db.conn.prepare(
            "SELECT target FROM actions WHERE kind=?1 AND selected=1 AND state='pending' AND target IS NOT NULL")?;
        let rows = stmt.query_map([&move_kind], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    // C-14：保持原位的 Git 项目整树仍受 H-06 保护——其祖先目录不得被当成空目录
    //（顺着删祖先等于间接改动该树）；已计划移入集合的项目不再保护，移走后变空的
    // 祖先按 C-07 消失。staying = git_roots − moved（Rust 侧计算）。
    // 前缀比较按 Unicode 折叠（与 fold 同口径）。
    let staying_git_roots: Vec<String> = {
        let mut stmt = job.db.conn.prepare("SELECT rel FROM git_roots")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut roots: Vec<String> = rows
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter(|rel| !moved_roots.contains(rel))
            .collect();
        if cfg!(windows) {
            roots = roots.into_iter().map(|rel| rel.to_lowercase()).collect();
        }
        roots.sort();
        roots
    };
    // Windows 前缀比较按 Unicode 折叠（与 under_path 口径一致）。目标清单固定，
    // 在此预折叠一次；折叠必须在排序之前——排序结果要用于二分定位前缀区间，
    // 折叠会改变字符的字典序。折叠后相同的目标去重，每个前缀只需检查一次。
    if cfg!(windows) {
        move_targets = move_targets
            .into_iter()
            .map(|target| target.to_lowercase())
            .collect();
    }
    move_targets.sort_unstable();
    move_targets.dedup();
    loop {
        let batch = {
            let mut statement = job.db.conn.prepare_cached(
                "SELECT seq,rel FROM empty_order WHERE seq>?1 ORDER BY seq LIMIT 256",
            )?;
            let rows = statement.query_map([cursor], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if batch.is_empty() {
            break;
        }
        for (seq, rel) in batch {
            cursor = seq;
            job.context.control.checkpoint()?;
            // H-06：本目录或其子树内仍有保持原位的 Git 项目时不按空目录处理。
            let rel_folded = if cfg!(windows) {
                rel.to_lowercase()
            } else {
                rel.clone()
            };
            let probe_git = format!("{rel_folded}/");
            let holds_git = staying_git_roots
                .iter()
                .any(|root| *root == rel_folded || root.starts_with(&probe_git));
            if holds_git {
                continue;
            }
            // 污点自底向上传播（empty_order 按 depth DESC，子目录必然先处理）：
            // 自身或任一子目录里存在「盘上可见但未入盘点」的内容（扫描期被过滤/读取
            // 失败，记录于 scan_taint）时，本目录不得按空目录处理，且继续向祖先传播。
            let tainted: bool = {
                let mut self_stmt = job
                    .db
                    .conn
                    .prepare_cached("SELECT EXISTS(SELECT 1 FROM scan_taint WHERE rel=?1)")?;
                let direct: bool = self_stmt.query_row([&rel], |r| r.get(0))?;
                let mut child_stmt = job.db.conn.prepare_cached(
                    "SELECT EXISTS(SELECT 1 FROM dir_children AS c JOIN tainted_will AS t ON t.rel=c.rel WHERE c.parent=?1)")?;
                let via_child: bool = child_stmt.query_row([&rel], |r| r.get(0))?;
                direct || via_child
            };
            if tainted {
                let mut insert = job
                    .db
                    .conn
                    .prepare_cached("INSERT OR IGNORE INTO tainted_will(rel) VALUES(?1)")?;
                insert.execute([&rel])?;
                continue;
            }
            // 执行后该目录（含子树）将接收被 Move 进来的内容时，不能按空目录处理。
            let probe = if cfg!(windows) {
                format!("{rel}/").to_lowercase()
            } else {
                format!("{rel}/")
            };
            // 有序目标表中以 probe 为前缀的目标构成连续区间：二分定位第一个 >= probe 的
            // 条目，只需检查它——若任何目标以 probe 开头，字典序最小的命中者必然是它。
            let receives_move = {
                let index = move_targets.partition_point(|t| t.as_str() < probe.as_str());
                move_targets
                    .get(index)
                    .is_some_and(|t| t.starts_with(probe.as_str()))
            };
            if receives_move {
                continue;
            }
            // 执行后会留在该目录（含其子树）里的文件：深层留驻文件会让对应子目录进不了
            // empty_will，在这里只需检查直接子文件即可得到相同结论。
            let has_file: bool = {
                let mut statement = job
                    .db
                    .conn
                    .prepare_cached("SELECT EXISTS(SELECT 1 FROM stay_parents WHERE parent=?1)")?;
                statement.query_row([&rel], |r| r.get(0))?
            };
            if has_file {
                continue;
            }
            // 子目录是否都已判定会为空
            let child_total: i64 = job.db.conn.query_row(
                "SELECT COUNT(1) FROM dir_children WHERE parent=?1",
                [&rel],
                |r| r.get(0),
            )?;
            let child_empty: i64 = job.db.conn.query_row(
                "SELECT COUNT(1) FROM dir_children WHERE parent=?1 AND rel IN (SELECT rel FROM empty_will)",[&rel],|r|r.get(0))?;
            if child_total > child_empty {
                continue;
            }
            {
                let mut statement = job
                    .db
                    .conn
                    .prepare_cached("INSERT OR IGNORE INTO empty_will(rel) VALUES(?1)")?;
                statement.execute([&rel])?;
            }
            job.db.add_action(&Action {
                id: 0,
                kind: ActionKind::EmptyDirectory,
                source: rel,
                target: None,
                reason: "计划执行后该目录将为空；执行时再次确认，只有实际为空才删除".into(),
                expected: None,
                keeper: None,
                hash: None,
                mode,
                selected: true,
                state: "pending".into(),
            })?;
            job.summary.planned_empty += 1;
        }
    }
    Ok(())
}
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "plan", skip_all)
)]
pub fn build(job: &mut Job) -> Result<()> {
    job.db
        .conn
        .execute_batch("DELETE FROM actions; DELETE FROM targets; DELETE FROM keepers;")?;
    cleanup_candidates(job)?; // Cleanup candidates must never become the sole duplicate keeper.
    deduplicate(job)?;
    // C-14 先于文件归类：项目移动先行执行，腾出的目录名可被分类层级复用；
    // 空目录规划依赖 git_roots.staying 标记，必须在集合规划之后。
    let moved_roots = git_collection(job)?;
    classify_files(job, &moved_roots)?;
    empty_directories(job, &moved_roots)?;
    Ok(())
}
