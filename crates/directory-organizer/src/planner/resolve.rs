use jchtools_file_core::{
    config::Config,
    fsutil,
    model::ActionKind,
    rules,
    task::{is_category_label, Job, GIT_COLLECTION_DIR},
};
use rusqlite::params;
use std::collections::{HashMap, HashSet};
use std::path::Path;

// ---------------------------------------------------------------------------
// C-05 / C-14 / C-16～C-21：固定归类与统一冲突消解
// ---------------------------------------------------------------------------

/// Windows 序数忽略大小写的近似折叠——委托 [`fsutil::fold_rel`]（唯一实现）。
pub(super) fn fold(name: &str) -> String {
    fsutil::fold_rel(name)
}
/// 折叠排序表上的前缀区间二分命中：`sorted` 已按折叠序升序去重，探测前缀也是
/// 折叠后的字符串。有序表中以该前缀开头的条目构成连续区间：二分定位第一个
/// 不小于探测前缀的条目，只需检查它——若任何条目以探测前缀开头，字典序最小的
/// 命中者必然是它。classify::empty_directories 与 planning::recompute_plan 共用
/// 同一判定，两处的排序准备必须保持同口径（折叠必须先于排序——排序结果要用于
/// 二分定位前缀区间，折叠会改变字符的字典序）。
pub(crate) fn sorted_prefix_hit(sorted: &[String], dir_folded: &str) -> bool {
    let index = sorted.partition_point(|item| item.as_str() < dir_folded);
    sorted
        .get(index)
        .is_some_and(|item| item.starts_with(dir_folded))
}
/// 目录位于任一被移走 Git 项目根之下（含相等）的折叠前缀判定：C-14 项目整树
/// 先移走，其内部条目不构成占用。
fn under_any_moved_root(dir_folded: &str, moved_roots: &[String]) -> bool {
    moved_roots.iter().any(|root| {
        let root = fold(root);
        dir_folded == root || dir_folded.starts_with(&format!("{root}/"))
    })
}
pub(super) fn parent_of(rel: &str) -> &str {
    match rel.rfind('/') {
        Some(index) => &rel[..index],
        None => "",
    }
}
pub(super) fn same_dir(a: &str, b: &str) -> bool {
    fold(a) == fold(b)
}
pub(super) fn same_component(a: &str, b: &str) -> bool {
    fold(a) == fold(b)
}
/// C-18 来源段：parent 各目录段（最近一级在前），剔除紧邻的标准分类段
///（附录 A 大类或大文件的单个目录段；旧版功能目录、年月目录等其他名称不剔除，
/// 作为普通来源目录参与消解）、以及项目来源的直接集合容器「Git项目集合」。
/// 段按已开启的 NFC/空白规则规范化（仅普通文件），不剥副本标记、不改源目录。
pub(super) fn source_levels(parent: &str, drop_collection: bool, normalize: bool) -> Vec<String> {
    let mut segments: Vec<String> = parent
        .split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if drop_collection {
        if let Some(last) = segments.last() {
            if same_component(last, GIT_COLLECTION_DIR) {
                segments.pop();
            }
        }
    }
    if let Some(last) = segments.last() {
        if is_category_label(last) {
            segments.pop();
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

/// 一个待定位项（存活文件或 Git 项目目录）。
pub(super) struct Item {
    pub(super) id: i64,
    pub(super) rel: String,
    pub(super) current_name: String,
    pub(super) stem: String,
    pub(super) extension: String,
    pub(super) target_dir: String,
    pub(super) sources: Vec<String>,
    /// 已位于目标目录内（C-21 按位置判定）。
    pub(super) in_place: bool,
    /// 派生名与当前名一致且组内唯一：保持名称与位置，不参与消解、名作固定占用。
    pub(super) settled: bool,
    /// 消解失败原因（该项保留源项）。
    pub(super) failed: Option<String>,
    /// 当前候选完整名。
    pub(super) candidate: String,
    /// 当前来源级数（C-18 的 k；对来源不足的项按可用级数封顶）。
    pub(super) k: usize,
    /// 摘要阶段（0=未进入；1..=29 对应 8,10,…,64 位十六进制）。
    pub(super) digest_step: u32,
    /// C-20 长度触发时已去掉来源段；后续摘要扩展保持无来源形式。
    pub(super) digest_no_source: bool,
    /// C-19 稳定序号兜底（摘要 64 位后追加 _N）。
    pub(super) index_suffix: Option<u32>,
}
/// 条目的文件名分解：现行名与 C-16 派生的主干/扩展名，两处构造共用同一口径。
pub(super) struct NameParts {
    pub(super) current_name: String,
    pub(super) stem: String,
    pub(super) extension: String,
}
impl Item {
    /// 构造待定位项：实质字段由调用方给定；settled/failed/candidate/k/digest_step/
    /// digest_no_source/index_suffix 七个消解状态字段恒为零值起步，固定在构造器内，
    /// 后续新增字段只需改这一处（git_collection 与 classify_files 两处调用共用）。
    pub(super) fn new(
        id: i64,
        rel: String,
        parts: NameParts,
        target_dir: String,
        sources: Vec<String>,
        in_place: bool,
    ) -> Self {
        let NameParts {
            current_name,
            stem,
            extension,
        } = parts;
        Self {
            id,
            rel,
            current_name,
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
        }
    }
    fn derived(&self) -> String {
        format!("{}{}", self.stem, self.extension)
    }
    pub(super) fn source_parent(&self) -> &str {
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
            no_source,
        )
    }
    /// 当前阶段的候选名（C-18 来源前缀 / C-19 摘要 / C-20 长度受限形式）。
    fn current_candidate(&mut self, config: Option<&Config>) -> Option<String> {
        let candidate = self.raw_candidate(config)?;
        // 使用原拆分边界：刚暴露的 .tar 等仍是主体；点文件加前缀后由完整名归类。
        let stem = candidate.strip_suffix(self.extension.as_str())?;
        let stem = match config {
            Some(config) => rules::stabilize_derived_stem(stem, config),
            None => rules::legalize_derived(stem),
        };
        let candidate = format!("{stem}{}", self.extension);
        fsutil::validate_component(&candidate).ok()?;
        Some(candidate)
    }
    /// 当前阶段的原始候选名（长度检查前先收敛主体）。
    fn raw_candidate(&mut self, config: Option<&Config>) -> Option<String> {
        if self.digest_step == 0 {
            let take = self.k.min(self.sources.len());
            if take == 0 {
                return Some(self.derived());
            }
            let prefix = level_prefix(&self.sources, take);
            let stem = format!("{prefix}_{}", self.stem);
            let stem = match config {
                Some(config) => rules::stabilize_derived_stem(&stem, config),
                None => rules::legalize_derived(&stem),
            };
            let candidate = format!("{stem}{}", self.extension);
            // C-20：逐层候选一旦超过 40 单元立即停止加来源，转无来源段的摘要形式。
            if candidate.encode_utf16().count() > rules::CONFLICT_YIELD_UNITS {
                self.digest_step = 1;
                self.digest_no_source = true;
                return self.digest_candidate(8, true);
            }
            return Some(candidate);
        }
        let hex_units = usize::try_from((6 + 2 * self.digest_step).min(64)).unwrap_or(64);
        // C-19 的首次摘要候选超长时同样进入 C-20；后续不能重新加回来源。
        if !self.digest_no_source {
            let source_units = self
                .sources
                .first()
                .map_or(0, |source| source.encode_utf16().count() + 1);
            let index_units = self
                .index_suffix
                .map_or(0, |index| index.to_string().len() + 1);
            if source_units
                + self.stem.encode_utf16().count()
                + 1
                + hex_units
                + self.extension.encode_utf16().count()
                + index_units
                > rules::CONFLICT_YIELD_UNITS
            {
                self.digest_no_source = true;
            }
        }
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
    // 在释放源名之前先判定所有目标容器是否可用。否则某项虽然看似会移出当前
    // 目录，却因目标容器被普通文件占用而根本没有 Move 计划，原名称不得腾空。
    let blocked_targets: HashMap<&str, String> = by_dir
        .keys()
        .filter_map(|&dir| {
            let folded = fold(dir);
            if under_any_moved_root(&folded, moved_roots) {
                return None;
            }
            let reason = match fsutil::safe_join(&job.root, dir) {
                Err(error) => Some(format!("目标路径不可用：{error:#}")),
                Ok(path) => match std::fs::read_dir(&path) {
                    // 目录可复用还必须不是受保护树：固定容器名（大类、「Git项目集合」）
                    // 被留在原地的 Git 项目占用时不得向树内移入任何文件（H-06/S-01），
                    // 依赖它的项全部失败并保留源项；边界无法检查时同样不放行。
                    Ok(_) => match fsutil::is_git_root(&path) {
                        Ok(true) => Some(format!(
                            "目标目录「{dir}」是受保护的 Git 项目：不进入树内，相关项保留源项（H-06）"
                        )),
                        Ok(false) => None,
                        Err(error) => Some(format!(
                            "无法检查目标目录「{dir}」的 Git 边界：{error:#}；相关项保留源项"
                        )),
                    },
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        blocked_by_file_ancestor(&job.root, dir)
                    }
                    Err(error) => Some(format!(
                        "目标目录存在但无法读取（可能被同名文件占用）：{error}"
                    )),
                },
            };
            reason.map(|reason| (dir, reason))
        })
        .collect();
    // C-17/C-18「最少级数」：已确定删除或移出本目录的文件（selected=1 且未执行的
    // Delete/Move 源）执行后名字腾空，不构成固定占用——否则幸存者会被迫多加一级
    // 来源前缀。口径与 empty_directories 的 doomed_sources 相同；查询失败时保守
    // 退化为「无腾出」（等同不扩展，命名偏保守，无覆盖风险）。
    let doomed_by_dir: HashMap<String, HashSet<String>> = (|| {
        let mut map: HashMap<String, HashSet<String>> = HashMap::new();
        let move_kind = serde_json::to_string(&ActionKind::Move).ok()?;
        let delete_kind = serde_json::to_string(&ActionKind::Delete).ok()?;
        let mut statement = job.db.conn.prepare(
            "SELECT source FROM actions WHERE kind IN (?1, ?2) AND selected=1 AND state='pending'",
        )
        .ok()?;
        let rows = statement
            .query_map(params![move_kind, delete_kind], |row| {
                row.get::<_, String>(0)
            })
            .ok()?;
        for source in rows.collect::<rusqlite::Result<Vec<_>>>().ok()? {
            let Some((parent, name)) = source.rsplit_once('/') else {
                continue;
            };
            map.entry(parent.to_string())
                .or_default()
                .insert(fold(name));
        }
        Some(map)
    })()
    .unwrap_or_default();
    for (dir, members) in by_dir {
        let mut plan = DirPlan::new();
        plan.blocked = blocked_targets.get(dir).cloned();
        let dir_folded = fold(dir);
        let under_moved_root = under_any_moved_root(&dir_folded, moved_roots);
        if !under_moved_root {
            // 活动且当前就在该目录里的项：当前名执行后会腾空，从占用中排除；
            // 外加本目录内已确定删除/移出的文件（同样腾空）。归类移动之间的依赖
            // 由执行器按目标→源关系排序，环形依赖使用临时暂存，因此这里可以按
            // C-17 把所有最终移出本目录的活动项视作会腾空。
            let mut freeing: HashSet<String> = items
                .iter()
                .filter(|item| {
                    item.failed.is_none()
                        && (!item.settled || !same_component(&item.candidate, &item.current_name))
                        && same_dir(item.source_parent(), dir)
                        // C-17：从当前目标目录移出的归类项也会释放原名称。
                        // 执行器会按依赖顺序处理这些移动；环形依赖使用临时暂存，
                        // 因而不会把“可能腾空”的名称误当成固定占用。
                        && !same_dir(&item.target_dir, dir)
                        && !blocked_targets.contains_key(item.target_dir.as_str())
                })
                .map(|item| fold(&item.current_name))
                .collect();
            freeing.extend(
                members
                    .iter()
                    .filter(|&&i| {
                        (!items[i].settled
                            || !same_component(&items[i].candidate, &items[i].current_name))
                            && items[i].failed.is_none()
                            && same_dir(items[i].source_parent(), dir)
                    })
                    .map(|&i| fold(&items[i].current_name)),
            );
            if let Some(doomed) = doomed_by_dir.get(dir) {
                freeing.extend(doomed.iter().cloned());
            }
            if plan.blocked.is_none() {
                if let Ok(path) = fsutil::safe_join(&job.root, dir) {
                    if let Ok(entries) = std::fs::read_dir(path) {
                        for entry in entries.flatten() {
                            if let Some(name) = entry.file_name().to_str() {
                                let folded = fold(name);
                                if !freeing.contains(&folded) {
                                    plan.fixed.insert(folded);
                                }
                            }
                        }
                    }
                }
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
pub(super) fn resolve_all(
    items: &mut [Item],
    job: &Job,
    moved_roots: &[String],
    config: Option<&Config>,
) {
    for item in items.iter_mut() {
        item.candidate = item.derived();
    }
    let mut dirs = gather_dir_plans(job, items, moved_roots);
    assign_blocked_failures(items, &dirs);
    // 按目标目录 + 派生名分组决定「已就位保持原名」（C-17 / C-21 / 附录 E）：
    // settled 标记在本步内才产生，之后才能把 settled 名称插为固定占用。
    assign_settled(items, &dirs);
    insert_settled_fixed(items, &mut dirs);
    // C-17/C-18 逐轮消解：碰撞簇里仍有可用来源级数的成员先加前缀（k+1），
    // 来源已用尽的成员本轮等待——给同伴腾位的时机；当碰撞簇全部成员都用尽来源
    // 仍撞名（或撞固定占用）时，这些成员进入 C-19 摘要阶梯（8,10,…,64 位，再到
    // 稳定序号兜底）。这样“x/合同 + y/合同 + 根/合同”在 k=1 即互不相同：根文件
    // 无来源、保持原名，x/y 加前缀（C-18 最少级数，不按扫描顺序抢名）。
    let mut rounds = 0usize;
    let mut refresh_dirs = false;
    loop {
        rounds += 1;
        if rounds > 1_000_000 {
            // 每项阶梯有限且序号兜底单调递增，正常必然收敛；此处防御性终止。
            break;
        }
        if refresh_dirs {
            // 只有类别或失败集合变化才重查磁盘占用；普通前缀/摘要阶梯不重复扫描。
            dirs = gather_dir_plans(job, items, moved_roots);
            assign_blocked_failures(items, &dirs);
            insert_settled_fixed(items, &mut dirs);
            refresh_dirs = false;
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
            // 阶梯推进（k+=1）留在本分支。
            items[index].k += 1;
            apply_candidate(&mut items[index], config, &mut refresh_dirs);
        }
        // C-19 极端兜底：摘要 64 位仍冲突的成员按附录 B 原路径稳定升序逐项分配
        // 最小可用序号（避开固定占用；同槽成员共享基名，先到者占小号），其余
        // 成员沿摘要阶梯继续 +2 位。生产表结构保证 rel 唯一（files UNIQUE /
        // git_roots 主键），同槽多成员仅在重复摘要输入时出现；即便出现也须
        // 有限收敛、不产生同名目标（不无限循环、不覆盖）。
        let mut suffix_groups: HashMap<(String, String), Vec<usize>> = HashMap::new();
        let mut digest_advance: Vec<usize> = Vec::new();
        for index in escalate_digest {
            let item = &items[index];
            if item.digest_step >= 29 {
                suffix_groups
                    .entry((item.target_dir.clone(), fold(&item.candidate)))
                    .or_default()
                    .push(index);
            } else {
                digest_advance.push(index);
            }
        }
        for index in digest_advance {
            let item = &mut items[index];
            if item.digest_step == 0 {
                // 首次进入摘要：保留最近来源（C-19 完整形式）；C-20 超长时内部去源。
                item.digest_step = 1;
            } else {
                item.digest_step += 1;
            }
            apply_candidate(item, config, &mut refresh_dirs);
        }
        for (_slot, mut group) in suffix_groups {
            group.sort_by(|&a, &b| stable_path_order(&items[a].rel, &items[b].rel));
            let mut probe = 1u32;
            for index in group {
                loop {
                    items[index].index_suffix = Some(probe);
                    apply_candidate(&mut items[index], config, &mut refresh_dirs);
                    // 生成失败保留失败态退出；序号撞固定占用时递增重试。
                    let occupied = items[index].failed.is_none()
                        && dirs.get(&items[index].target_dir).is_some_and(|plan| {
                            plan.fixed.contains(&fold(&items[index].candidate))
                        });
                    if !occupied {
                        break;
                    }
                    probe += 1;
                }
                // 同基名的下一成员从已占序号之后取各自最小可用值。
                probe += 1;
            }
        }
    }
}

/// 附录 B 稳定升序：目录分隔符统一 `/` 后先做忽略大小写的序数比较（折叠序），
/// 仍相同按原始 UTF-16 单元序列逐单元升序决胜。
fn stable_path_order(a: &str, b: &str) -> std::cmp::Ordering {
    let folded = fold(a).cmp(&fold(b));
    if folded != std::cmp::Ordering::Equal {
        return folded;
    }
    a.encode_utf16().cmp(b.encode_utf16())
}

/// 目标容器不可用（blocked）的项全部失败并保留源项（S-01）；初始 gather 与
/// refresh_dirs 重查之后各调用一次，口径一致。
fn assign_blocked_failures(items: &mut [Item], dirs: &HashMap<String, DirPlan>) {
    for item in items.iter_mut() {
        if let Some(reason) = dirs
            .get(&item.target_dir)
            .and_then(|plan| plan.blocked.clone())
        {
            item.failed = Some(reason);
        }
    }
}

/// 按目标目录 + 派生名分组决定「已就位保持原名」（C-17 / C-21 / 附录 E）：
/// 组内恰有一个已就位项且派生名可用 → 它占住该派生名（名字已物化则保持原名；
/// 未物化但派生名不被磁盘固定占用时以就地改名物化），其余项退让；组内有多个
/// 已就位项（不论名字是否已物化）→ 全部统一消解，不得只固定其中一个。
fn assign_settled(items: &mut [Item], dirs: &HashMap<String, DirPlan>) {
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
        let in_place_count = members.iter().filter(|&&i| items[i].in_place).count();
        let capable: Vec<usize> = members
            .iter()
            .copied()
            .filter(|&i| items[i].in_place && items[i].derived() == items[i].current_name)
            .collect();
        // C-17：多个已就位项自身规范化后同名时，这些已就位项也统一消解
        //（附录 E：`报告 (1).pdf` 与 `报告_1.pdf` 双方都改名），
        // 新移入项不得抢占其候选名。
        if in_place_count != 1 {
            continue;
        }
        if capable.len() == 1 {
            items[capable[0]].settled = true;
        } else if let Some(only) = members.iter().copied().find(|&i| items[i].in_place) {
            // 唯一已就位项的派生名尚未物化（如 `报告 (1).pdf` → `报告_1.pdf`）：
            // 派生名不被该目录磁盘固定占用时先保护其规范化后名称（C-17），
            // 由新移入项消解；本项以就地改名物化派生名（classify 阶段出计划）。
            let derived = fold(&items[only].derived());
            let occupied = dirs
                .get(&items[only].target_dir)
                .is_some_and(|plan| plan.fixed.contains(&derived));
            if !occupied {
                items[only].settled = true;
            }
        }
    }
}

/// 已就位保留项的名称是固定占用（保护既有项优先于任何候选）；必须在
/// assign_settled 产生 settled 标记之后调用，否则占用插入落空。
fn insert_settled_fixed(items: &[Item], dirs: &mut HashMap<String, DirPlan>) {
    for item in items.iter().filter(|i| i.settled && i.failed.is_none()) {
        if let Some(plan) = dirs.get_mut(&item.target_dir) {
            plan.fixed.insert(fold(&item.candidate));
        }
    }
}

/// 单个候选项的候选名应用与目标重算：escalate_prefix 与 escalate_digest 两分支
/// 尾部共用。生成当前阶段候选成功则更新候选名，并在启用配置且非大文件时按候选
/// 重算大类目标（类别变化触发 refresh_dirs）；失败则标记该项消解失败并重查。
fn apply_candidate(item: &mut Item, config: Option<&Config>, refresh_dirs: &mut bool) {
    if let Some(candidate) = item.current_candidate(config) {
        item.candidate = candidate;
        if config.is_some() && item.target_dir != "大文件" {
            let target_dir = rules::category_for(&item.candidate.to_lowercase());
            *refresh_dirs |= item.target_dir != target_dir;
            item.target_dir = target_dir.into();
            item.in_place = same_dir(item.source_parent(), &item.target_dir);
        }
    } else {
        item.failed = Some("无法生成合法的冲突消解名称（名称与扩展名过长）".into());
        *refresh_dirs = true;
    }
}
