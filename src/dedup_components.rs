//! C-02/C-03 的候选连通组计算。
//!
//! 该模块只负责从已完成扫描和哈希的文件元数据计算候选组及重复组。
//! 计划器负责把返回的索引映射回数据库行，并在 C-04 下处理硬链接保护。

use crate::{config::KeepPolicy, model::FileRecord, rules};
use std::collections::HashMap;

/// C-02 中启用的名称关系。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DedupRules {
    pub same_name: bool,
    pub copy_names: bool,
    pub other_names: bool,
}

/// 一个完整哈希重复组。索引指向传入 `records`，而不是数据库主键。
/// `keeper` 按 C-03 的规则确定，`duplicates` 已按同一稳定顺序排列。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateGroup {
    pub keeper: usize,
    pub duplicates: Vec<usize>,
}

#[derive(Debug)]
struct DisjointSet {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl DisjointSet {
    fn new(len: usize) -> Self {
        Self {
            parent: (0..len).collect(),
            rank: vec![0; len],
        }
    }

    fn find(&mut self, mut value: usize) -> usize {
        let mut root = value;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        while self.parent[value] != value {
            let next = self.parent[value];
            self.parent[value] = root;
            value = next;
        }
        root
    }

    fn union(&mut self, left: usize, right: usize) {
        let mut left = self.find(left);
        let mut right = self.find(right);
        if left == right {
            return;
        }
        if self.rank[left] < self.rank[right] {
            std::mem::swap(&mut left, &mut right);
        }
        self.parent[right] = left;
        if self.rank[left] == self.rank[right] {
            self.rank[left] += 1;
        }
    }
}

/// 计算 C-02 的候选连通组，再在组内按完整哈希拆出重复组。
///
/// `records` 包含当前活动候选以及哈希失败后停用的空哈希桥接节点。
/// 因为它可能连接两个候选边；它本身永远不会进入返回的删除组。
pub fn find_duplicate_groups(
    records: &[FileRecord],
    ruleset: DedupRules,
    keep_policy: KeepPolicy,
) -> Vec<DuplicateGroup> {
    if records.is_empty() || (!ruleset.same_name && !ruleset.copy_names && !ruleset.other_names) {
        return Vec::new();
    }

    let mut components: HashMap<u64, Vec<usize>> = HashMap::new();
    for (index, record) in records.iter().enumerate() {
        components
            .entry(record.snapshot.size)
            .or_default()
            .push(index);
    }

    let mut output = Vec::new();
    for indices in components.into_values() {
        let mut dsu = DisjointSet::new(indices.len());

        if ruleset.same_name {
            let mut by_name: HashMap<String, usize> = HashMap::new();
            for (local, &index) in indices.iter().enumerate() {
                let key = records[index].name.clone();
                if let Some(&first) = by_name.get(&key) {
                    dsu.union(first, local);
                } else {
                    by_name.insert(key, local);
                }
            }
        }

        if ruleset.copy_names {
            let mut by_normal: HashMap<String, Vec<usize>> = HashMap::new();
            for (local, &index) in indices.iter().enumerate() {
                by_normal
                    .entry(records[index].normalized.clone())
                    .or_default()
                    .push(local);
            }
            for bucket in by_normal.values() {
                // 副本名边只存在于原名不同的记录之间；桶中出现至少一对
                // 不同原名后，桶内记录通过这些边形成传递连接。
                let first_name = records[indices[bucket[0]]].name.clone();
                if let Some(&bridge) = bucket
                    .iter()
                    .find(|&&local| records[indices[local]].name != first_name)
                {
                    for &local in bucket {
                        dsu.union(bridge, local);
                    }
                }
            }
        }

        if ruleset.other_names {
            union_other_name_components(&indices, records, &mut dsu);
        }

        let mut candidate_components: HashMap<usize, Vec<usize>> = HashMap::new();
        for (local, &index) in indices.iter().enumerate() {
            let root = dsu.find(local);
            candidate_components.entry(root).or_default().push(index);
        }

        for component in candidate_components.into_values() {
            let mut by_hash: HashMap<&str, Vec<usize>> = HashMap::new();
            for &index in &component {
                if let Some(hash) = records[index].hash.as_deref() {
                    by_hash.entry(hash).or_default().push(index);
                }
            }
            for mut duplicate_indices in by_hash.into_values() {
                if duplicate_indices.len() < 2 {
                    continue;
                }
                duplicate_indices.sort_by(|&left, &right| {
                    rules::compare(&records[left], &records[right], keep_policy)
                });
                let keeper = duplicate_indices.remove(0);
                output.push(DuplicateGroup {
                    keeper,
                    duplicates: duplicate_indices,
                });
            }
        }
    }

    output.sort_by(|left, right| {
        records[left.keeper]
            .rel
            .cmp(&records[right.keeper].rel)
            .then_with(|| left.duplicates.len().cmp(&right.duplicates.len()))
    });
    output
}

/// 为“原名不同且副本名键不同”的图加入一组足以连通各组件的边。
///
/// 该关系是两个属性相等关系的补图。选定一个枢轴后，所有同时不同于枢轴
/// 两个属性的记录都与枢轴相连；其余记录只能落在枢轴的同名行或同键列中。
/// 同名行与同键列之间是完全二分连接，因而只需保留一对代表边即可，避免
/// 对大型同尺寸文件集执行 O(n²) 的全量两两比较。
fn union_other_name_components(indices: &[usize], records: &[FileRecord], dsu: &mut DisjointSet) {
    if indices.len() < 2 {
        return;
    }
    let pivot = 0;
    let pivot_record = &records[indices[pivot]];
    let pivot_name = pivot_record.name.as_str();
    let pivot_normal = pivot_record.normalized.as_str();
    let mut direct_neighbors = Vec::new();
    let mut root_by_name = HashMap::new();
    let mut root_by_normal = HashMap::new();
    for local in 1..indices.len() {
        let record = &records[indices[local]];
        if record.name != pivot_name && record.normalized != pivot_normal {
            dsu.union(pivot, local);
            direct_neighbors.push(local);
            root_by_name.entry(record.name.as_str()).or_insert(local);
            root_by_normal
                .entry(record.normalized.as_str())
                .or_insert(local);
        }
    }

    let mut same_name_leftovers = Vec::new();
    let mut same_normal_leftovers = Vec::new();
    let mut added_name = vec![false; indices.len()];
    let mut added_normal = vec![false; indices.len()];
    loop {
        let mut changed = false;
        for local in 1..indices.len() {
            let record = &records[indices[local]];
            if record.name == pivot_name && record.normalized != pivot_normal {
                if added_name[local] {
                    continue;
                }
                if let Some(neighbor) = root_by_normal
                    .iter()
                    .find(|(normal, _)| **normal != record.normalized)
                    .map(|(_, &index)| index)
                {
                    dsu.union(pivot, local);
                    dsu.union(pivot, neighbor);
                    added_name[local] = true;
                    root_by_name.entry(pivot_name).or_insert(local);
                    changed = true;
                }
            } else if record.normalized == pivot_normal && record.name != pivot_name {
                if added_normal[local] {
                    continue;
                }
                if let Some(neighbor) = root_by_name
                    .iter()
                    .find(|(name, _)| **name != record.name)
                    .map(|(_, &index)| index)
                {
                    dsu.union(pivot, local);
                    dsu.union(pivot, neighbor);
                    added_normal[local] = true;
                    root_by_normal.entry(pivot_normal).or_insert(local);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }

    for local in 1..indices.len() {
        let record = &records[indices[local]];
        if record.name == pivot_name && record.normalized == pivot_normal {
            if !direct_neighbors.is_empty() {
                dsu.union(pivot, local);
            }
        } else if record.name == pivot_name && !added_name[local] {
            same_name_leftovers.push(local);
        } else if record.normalized == pivot_normal && !added_normal[local] {
            same_normal_leftovers.push(local);
        }
    }

    // 剩余同名行与同键列之间是完全二分连接，只需保留代表边。
    if let (Some(&left), Some(&right)) =
        (same_name_leftovers.first(), same_normal_leftovers.first())
    {
        dsu.union(left, right);
        for &local in same_name_leftovers.iter().skip(1) {
            dsu.union(local, right);
        }
        for &local in same_normal_leftovers.iter().skip(1) {
            dsu.union(left, local);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Snapshot;

    fn record(id: i64, rel: &str, name: &str, normal: &str, hash: Option<&str>) -> FileRecord {
        FileRecord {
            id,
            rel: rel.into(),
            name: name.into(),
            normalized: normal.into(),
            snapshot: Snapshot {
                size: 1,
                modified_ns: id,
                created_ns: None,
                identity: id.to_string(),
                links: 1,
            },
            hash: hash.map(str::to_owned),
            cleanable: false,
        }
    }

    #[test]
    fn copy_name_bridge_is_transitive_and_hash_null_can_bridge() {
        let records = vec![
            record(1, "a/report.txt", "report.txt", "report.txt", Some("same")),
            record(2, "b/report.txt", "report.txt", "report.txt", Some("same")),
            record(3, "c/report (1).txt", "report (1).txt", "report.txt", None),
            record(
                4,
                "d/report (2).txt",
                "report (2).txt",
                "report.txt",
                Some("same"),
            ),
        ];
        let groups = find_duplicate_groups(
            &records,
            DedupRules {
                same_name: false,
                copy_names: true,
                other_names: false,
            },
            KeepPolicy::Oldest,
        );
        assert_eq!(
            groups,
            vec![DuplicateGroup {
                keeper: 0,
                duplicates: vec![1, 3]
            }]
        );
    }

    #[test]
    fn copy_flag_does_not_reenable_same_name_by_itself() {
        let records = vec![
            record(1, "a/report.txt", "report.txt", "report.txt", Some("same")),
            record(2, "b/report.txt", "report.txt", "report.txt", Some("same")),
        ];
        assert!(find_duplicate_groups(
            &records,
            DedupRules {
                same_name: false,
                copy_names: true,
                other_names: false,
            },
            KeepPolicy::Newest,
        )
        .is_empty());
    }

    #[test]
    fn different_name_rule_does_not_match_same_name_only() {
        let records = vec![
            record(1, "a/report.txt", "report.txt", "report.txt", Some("same")),
            record(2, "b/report.txt", "report.txt", "report.txt", Some("same")),
        ];
        assert!(find_duplicate_groups(
            &records,
            DedupRules {
                same_name: false,
                copy_names: false,
                other_names: true,
            },
            KeepPolicy::Newest,
        )
        .is_empty());
    }

    #[test]
    fn different_name_rule_does_not_match_copy_name_only() {
        let records = vec![
            record(1, "a/report.txt", "report.txt", "report.txt", Some("same")),
            record(
                2,
                "b/report (1).txt",
                "report (1).txt",
                "report.txt",
                Some("same"),
            ),
        ];
        assert!(find_duplicate_groups(
            &records,
            DedupRules {
                same_name: false,
                copy_names: false,
                other_names: true,
            },
            KeepPolicy::Newest,
        )
        .is_empty());
    }

    #[test]
    fn names_are_compared_with_the_stored_key() {
        let records = vec![
            record(1, "a/report.txt", "Report.txt", "report.txt", Some("same")),
            record(2, "b/report.txt", "report.txt", "report.txt", Some("same")),
        ];
        assert_eq!(
            find_duplicate_groups(
                &records,
                DedupRules {
                    same_name: false,
                    copy_names: true,
                    other_names: false,
                },
                KeepPolicy::Newest,
            ),
            vec![DuplicateGroup {
                keeper: 1,
                duplicates: vec![0]
            }]
        );
    }

    #[test]
    fn different_name_graph_preserves_cross_pair_topology() {
        let records = vec![
            record(1, "a.txt", "n1", "k1", Some("same")),
            record(2, "b.txt", "n2", "k2", Some("same")),
            record(3, "c.txt", "n1", "k2", Some("same")),
        ];
        let groups = find_duplicate_groups(
            &records,
            DedupRules {
                same_name: false,
                copy_names: false,
                other_names: true,
            },
            KeepPolicy::Newest,
        );
        assert_eq!(
            groups,
            vec![DuplicateGroup {
                keeper: 1,
                duplicates: vec![0]
            }]
        );
    }

    #[test]
    fn different_name_graph_matches_bruteforce_on_random_small_inputs() {
        fn brute(records: &[FileRecord]) -> Vec<Vec<usize>> {
            let mut dsu = DisjointSet::new(records.len());
            for left in 0..records.len() {
                for right in (left + 1)..records.len() {
                    if records[left].name != records[right].name
                        && records[left].normalized != records[right].normalized
                    {
                        dsu.union(left, right);
                    }
                }
            }
            let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
            for index in 0..records.len() {
                groups.entry(dsu.find(index)).or_default().push(index);
            }
            let mut groups: Vec<Vec<usize>> = groups
                .into_values()
                .filter(|group| group.len() > 1)
                .collect();
            for group in &mut groups {
                group.sort_unstable();
            }
            groups.sort_unstable();
            groups
        }

        let mut seed = 0x9e37_79b9_u32;
        for round in 0..80 {
            let count = 2 + (seed as usize % 10);
            let mut records = Vec::with_capacity(count);
            for index in 0..count {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let name = format!("n{}", (seed >> 8) % 4);
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let normal = format!("k{}", (seed >> 8) % 4);
                records.push(record(
                    i64::try_from(index).unwrap(),
                    &format!("{round}/{index}.txt"),
                    &name,
                    &normal,
                    Some("same"),
                ));
            }
            let mut actual: Vec<Vec<usize>> = find_duplicate_groups(
                &records,
                DedupRules {
                    same_name: false,
                    copy_names: false,
                    other_names: true,
                },
                KeepPolicy::Newest,
            )
            .into_iter()
            .map(|group| {
                let mut members = vec![group.keeper];
                members.extend(group.duplicates);
                members.sort_unstable();
                members
            })
            .collect();
            actual.sort_unstable();
            assert_eq!(
                actual,
                brute(&records),
                "random round {round}: {:?}",
                records
                    .iter()
                    .map(|record| (&record.name, &record.normalized))
                    .collect::<Vec<_>>()
            );
        }
    }
}
