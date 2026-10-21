use std::cmp::Ordering;
use std::ffi::{OsStr, OsString};
use std::path::Path;

/// 结果名占用索引（T-11/F29）：按大小写折叠键分桶，桶内候选再用
/// [`jchtools_file_core::platform::compare_names_ordinal_ignore_case`]（Windows `CompareStringOrdinal`
/// 忽略大小写）精确确认。
/// 平铺模式登记全局结果名，层级模式登记「相对父目录+结果名」（见 `scan`）。
/// 折叠只用于缩小候选集：语义上过桶只会多比、不会漏比，occupied 查询近似 O(1)。
#[derive(Default)]
pub(super) struct OccupiedIndex {
    buckets: std::collections::HashMap<Vec<u16>, Vec<OsString>>,
}

impl OccupiedIndex {
    pub(super) fn contains(&self, name: &OsStr) -> bool {
        self.contains_with(name, &mut |left, right| {
            jchtools_file_core::platform::compare_names_ordinal_ignore_case(left, right)
                == Ordering::Equal
        })
    }

    /// 可注入比较谓词的查询（测试用于断言比较次数增长阶）。
    pub(super) fn contains_with(
        &self,
        name: &OsStr,
        is_equal: &mut dyn FnMut(&OsStr, &OsStr) -> bool,
    ) -> bool {
        self.buckets
            .get(&case_fold_key(name))
            .is_some_and(|bucket| bucket.iter().any(|existing| is_equal(existing, name)))
    }

    pub(super) fn insert(&mut self, name: OsString) {
        self.buckets
            .entry(case_fold_key(&name))
            .or_default()
            .push(name);
    }
}

/// 折叠键：逐 UTF-16 单元做简单大写折叠。ASCII 走快路径；非 ASCII 先查内嵌
/// 简单大写映射表（见 [`simple_upper_unit`]），再采用 Unicode 单单元大写映射，
/// 多单元或无映射（含代理项）保留原单元。折叠与 `CompareStringOrdinal` 的
/// 逐单元大写口径一致，不一致的极端情形只会把候选落进不同桶后再精确比较
/// （多比不漏比）。
pub(super) fn case_fold_key(name: &OsStr) -> Vec<u16> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        name.encode_wide().map(fold_unit).collect()
    }
    #[cfg(not(windows))]
    {
        name.to_string_lossy()
            .encode_utf16()
            .map(fold_unit)
            .collect()
    }
}

/// Unicode 简单（单单元）大写映射中与 Rust `to_uppercase` 全映射发散、且经
/// 本机 `CompareStringOrdinal` 忽略大小写运行时核实为判等的块（S4-04）：
/// 0x1F80–0x1F87/0x1F90–0x1F97/0x1FA0–0x1FA7 段按简单大写 +8、0x1FB3→0x1FBC、
/// 0x1FC3→0x1FCC、0x1FF3→0x1FFC 逐对判等（含 0x1F82↔0x1F8A 等中段样本），
/// 而 0x1F88 与 0x1F08 不判等（OS 按简单表折叠且标题形大写保持自身）。
/// 注意边界：带额外声调的变体（0x1FB2、0x1FB4、0x1FB7 等）虽在 UnicodeData
/// 有单单元简单大写（指向 0x1FBA 等裸大写形），OS 实测【不】判等——
/// 不得纳入本表（该组反例已由测试锁定）。带 ypogegrammeni 的希腊预组合
/// 小写全大写为两个字符，`to_uppercase` 路线会保持原单元导致漏比；表内
/// 目标单元（0x1F88 段、0x1FBC 等标题形字符）全大写同为多字符、折叠时
/// 保持原样，两侧落入同桶。
fn simple_upper_unit(unit: u16) -> Option<u16> {
    match unit {
        0x1F80..=0x1F87 | 0x1F90..=0x1F97 | 0x1FA0..=0x1FA7 => Some(unit + 0x08),
        0x1FB3 => Some(0x1FBC),
        0x1FC3 => Some(0x1FCC),
        0x1FF3 => Some(0x1FFC),
        _ => None,
    }
}

fn fold_unit(unit: u16) -> u16 {
    if unit < 0x80 {
        return if (u16::from(b'a')..=u16::from(b'z')).contains(&unit) {
            unit - 32
        } else {
            unit
        };
    }
    if (0xD800..=0xDFFF).contains(&unit) {
        return unit;
    }
    // S4-04：先查简单大写映射——「全大写多字符但简单大写单单元」的单元
    // （希腊 ypogegrammeni 预组合等）必须按简单映射折叠，才能与
    // `CompareStringOrdinal` 的逐单元大写口径一致（否则占用索引漏比）。
    if let Some(simple) = simple_upper_unit(unit) {
        return simple;
    }
    let Some(ch) = char::from_u32(u32::from(unit)) else {
        return unit;
    };
    let mut upper = ch.to_uppercase();
    let Some(mapped) = upper.next() else {
        return unit;
    };
    if upper.next().is_some() || mapped.len_utf16() != 1 {
        return unit;
    }
    u16::try_from(u32::from(mapped)).unwrap_or(unit)
}

/// 结果名占用键（T-11）：平铺模式为全局结果名；层级模式为「相对父目录+
/// 结果名」——同一输出父目录内大小写不敏感等价的结果名互相挤占（提交走
/// rename_noreplace，等价名必然失败，必须在计划阶段计为同名跳过而不是浪费
/// 整份转换后计失败，A'-1），不同子目录互不影响（T-10 保留层级）。键经
/// [`OccupiedIndex`] 按大小写折叠比较。
pub(super) fn occupancy_key(flat: bool, relative_parent: &Path, file_name: &OsStr) -> OsString {
    if flat {
        file_name.to_os_string()
    } else {
        relative_parent.join(file_name).into_os_string()
    }
}

pub(super) fn markdown_name(source: &Path) -> Result<OsString, String> {
    let stem = source
        .file_stem()
        .ok_or_else(|| format!("无法取得文件名：{}", source.display()))?;
    let ext = source
        .extension()
        .and_then(OsStr::to_str)
        .ok_or_else(|| format!("缺少扩展名：{}", source.display()))?;
    let mut output = OsString::from(stem);
    output.push("_");
    output.push(ext.to_ascii_lowercase());
    output.push(".md");
    Ok(output)
}

pub(super) fn path_equal(left: &Path, right: &Path) -> bool {
    compare_paths(left, right) == Ordering::Equal
}

pub(super) fn path_begins_with(path: &Path, prefix: &Path) -> bool {
    let mut path_parts = path.components();
    for prefix_part in prefix.components() {
        let Some(path_part) = path_parts.next() else {
            return false;
        };
        if jchtools_file_core::platform::compare_names_ordinal_ignore_case(
            path_part.as_os_str(),
            prefix_part.as_os_str(),
        ) != Ordering::Equal
        {
            return false;
        }
    }
    true
}

pub(super) fn compare_paths(left: &Path, right: &Path) -> Ordering {
    let case_insensitive = compare_paths_insensitive(left, right);
    if case_insensitive != Ordering::Equal {
        return case_insensitive;
    }
    compare_paths_original(left, right)
}

pub(super) fn compare_paths_insensitive(left: &Path, right: &Path) -> Ordering {
    compare_component_wise(
        left,
        right,
        jchtools_file_core::platform::compare_names_ordinal_ignore_case,
    )
}

// T-11 决胜序等价性论证（A'-低危2）：合同写「相同键再按原始路径 UTF-16 单元
// 序列决胜」，本实现按「逐段原始 UTF-16（首段分出胜负即停）」。两者在决胜段内
// 数学等价：决胜段仅在 [`compare_paths_insensitive`] 全段相等时进入，此时两路径
// 分段数相同、前 i 段原始相等、第 i 段两两大小写不敏感相等但原始不同。大小写
// 不敏感相等的两段不可能互为真前缀——若 c 是 d 的真前缀则 d=c+e（e 非空），逐
// 单元折叠不消灭单元、段内也不含分隔符，fold(d)=fold(c)+fold(e)≠fold(c)，与不
// 区分大小写相等矛盾——故第 i 段的第一差异单元必在两段内部的同一位置，逐段与
// 整路径两种比较的第一差异单元相同，结论一致（性质由
// `flat_tiebreak_per_component_matches_whole_path_sequence` 固定）。
fn compare_paths_original(left: &Path, right: &Path) -> Ordering {
    compare_component_wise(left, right, compare_original)
}

/// 逐段比较的公共骨架：同一分段数与短路语义下，仅段内比较器不同。
/// T-11 决胜序（A'-低危2）依赖「两条路径同为逐段比较」这一不变量，
/// 提取后该不变量只有一处实现。
fn compare_component_wise(
    left: &Path,
    right: &Path,
    compare: impl Fn(&OsStr, &OsStr) -> Ordering,
) -> Ordering {
    let mut left_parts = left.components();
    let mut right_parts = right.components();
    loop {
        match (left_parts.next(), right_parts.next()) {
            (Some(a), Some(b)) => {
                let order = compare(a.as_os_str(), b.as_os_str());
                if order != Ordering::Equal {
                    return order;
                }
            }
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (None, None) => return Ordering::Equal,
        }
    }
}

#[cfg(windows)]
fn compare_original(left: &OsStr, right: &OsStr) -> Ordering {
    use std::os::windows::ffi::OsStrExt;
    left.encode_wide().cmp(right.encode_wide())
}

#[cfg(not(windows))]
fn compare_original(left: &OsStr, right: &OsStr) -> Ordering {
    left.cmp(right)
}
