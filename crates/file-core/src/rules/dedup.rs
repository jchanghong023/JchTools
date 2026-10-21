#[cfg(test)]
use crate::config::Config;
use crate::config::KeepPolicy;
use crate::model::FileRecord;
use std::cmp::Ordering;
use std::ffi::OsStr;

/// 名称关系参考实现：计划器以候选连通组实现 C-02；此函数供规则测试对照。
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
pub fn duplicate_allowed(a: &FileRecord, b: &FileRecord, cfg: &Config) -> bool {
    if a.name == b.name {
        cfg.dedup_same_name
    } else if a.normalized == b.normalized {
        cfg.dedup_copy_names
    } else {
        cfg.dedup_other_names
    }
}
/// Ordering::Less means a is the preferred keeper. Ties never rely on traversal order.
/// C-03：最短名称按完整文件名的 UTF-16 单元数升序；平局按相对路径长度（UTF-16 单元）
/// 升序、再按路径稳定升序决胜（附录 B）。
pub fn compare(a: &FileRecord, b: &FileRecord, policy: KeepPolicy) -> Ordering {
    let primary = match policy {
        KeepPolicy::Newest => b.snapshot.modified_ns.cmp(&a.snapshot.modified_ns),
        KeepPolicy::Oldest => a.snapshot.modified_ns.cmp(&b.snapshot.modified_ns),
        // files.name 是序数比较键，不是原始名称；C-03 必须按原始
        // 相对路径末段的完整文件名决胜。
        KeepPolicy::ShortestName => a
            .rel
            .rsplit('/')
            .next()
            .unwrap_or(&a.name)
            .encode_utf16()
            .count()
            .cmp(
                &b.rel
                    .rsplit('/')
                    .next()
                    .unwrap_or(&b.name)
                    .encode_utf16()
                    .count(),
            ),
    };
    primary
        .then_with(|| {
            a.rel
                .encode_utf16()
                .count()
                .cmp(&b.rel.encode_utf16().count())
        })
        .then_with(|| {
            crate::platform::compare_names_ordinal_ignore_case(
                OsStr::new(&a.rel),
                OsStr::new(&b.rel),
            )
        })
        .then_with(|| a.rel.encode_utf16().cmp(b.rel.encode_utf16()))
}
/// C-04：可靠文件标识是否证明两个目录项指向同一物理文件。
/// 条件：标识相同、两侧链接数 >= 2（两个目录项必然抬高链接数）、且标识未退化。
/// Windows 的「卷:索引高:索引低」在部分文件系统上索引恒为 0，无法区分不同文件
/// （与 C-13 的 hash_cache 同口径）；退化时不得据此跳过重复处理。
/// Unix 的「设备:inode」两段结构本身就唯一标识文件，不套用该三段口径。
pub fn identity_proves_same_file(a: &FileRecord, b: &FileRecord) -> bool {
    a.snapshot.identity == b.snapshot.identity
        && a.snapshot.links >= 2
        && b.snapshot.links >= 2
        && !(cfg!(windows) && crate::hash_cache::identity_is_degenerate(&a.snapshot.identity))
}
