use anyhow::{ensure, Context, Result};
use jchtools_file_core::{
    fsutil, rules,
    task::{enqueue_archive, Job},
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub(super) fn composite_stream(archive: &Path) -> bool {
    let Some(name) = archive.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let name = match name.rsplit_once('.') {
        Some((rest, digits)) if fsutil::is_numbered_volume_digits(digits) => rest,
        _ => name,
    };
    [
        ".tgz",
        ".tbz2",
        ".txz",
        ".tzst",
        ".tar.gz",
        ".tar.bz2",
        ".tar.xz",
        ".tar.zst",
        ".tar.lzma",
        ".tar.z",
    ]
    .iter()
    .any(|suffix| {
        name.get(name.len().saturating_sub(suffix.len())..)
            .is_some_and(|ending| ending.eq_ignore_ascii_case(suffix))
    })
}

pub(super) fn compressed_stream_kind(kind: &str) -> bool {
    ["gzip", "bzip2", "xz", "zstd", "lzma", "lzma86", "z"]
        .iter()
        .any(|supported| kind.eq_ignore_ascii_case(supported))
}

pub(super) fn potential_archive_member(name: &str) -> bool {
    if rules::archive_name(name) {
        return true;
    }
    let Some((base, suffix)) = name.rsplit_once('.') else {
        return false;
    };
    if suffix.eq_ignore_ascii_case("rar") {
        return rar_part_stem(&name.to_ascii_lowercase()).is_some();
    }
    if fsutil::is_numbered_volume_digits(suffix) {
        return rules::archive_name(base);
    }
    if suffix.len() == 3
        && matches!(suffix.as_bytes()[0], b'r' | b'R' | b'z' | b'Z')
        && suffix.as_bytes()[1..].iter().all(u8::is_ascii_digit)
    {
        return rules::old_style_tail(&name.to_ascii_lowercase()).is_some();
    }
    false
}

pub(super) fn enqueue_nested_groups(
    job: &Job,
    directories: BTreeMap<PathBuf, BTreeMap<String, PathBuf>>,
    depth: u32,
) -> Result<()> {
    for members in directories.into_values() {
        let mut seen = HashSet::new();
        for (name, path) in &members {
            if let Some(key) = rules::archive_entry_key(name) {
                if seen.insert(key) {
                    enqueue_archive(job, path, depth)?;
                }
            }
        }
        let names: Vec<String> = members.keys().cloned().collect();
        for name in rules::part_rar_missing_first_groups(&names) {
            let path = members.get(&name).context("缺首卷候选不存在")?;
            if missing_part_rar_first(path).is_none() {
                continue;
            }
            let stem = rar_part_stem(&name).context("缺首卷候选形态无效")?;
            let key = rules::archive_entry_key(&format!("{stem}.part1.rar"))
                .context("缺首卷族无法归组")?;
            if seen.insert(key) {
                enqueue_archive(job, path, depth)?;
            }
        }
        for name in rules::numbered_volume_missing_entry_groups(&names) {
            let path = members.get(&name).context("缺入口候选不存在")?;
            if missing_numbered_entry(path).is_none() {
                continue;
            }
            let (stem, _) = name.rsplit_once('.').context("缺入口候选形态无效")?;
            let key =
                rules::archive_entry_key(&format!("{stem}.001")).context("缺入口族无法归组")?;
            if seen.insert(key) {
                enqueue_archive(job, path, depth)?;
            }
        }
        for (name, path) in &members {
            let Some(tail) = rules::old_style_tail(name) else {
                continue;
            };
            if !missing_old_style_main(path) {
                continue;
            }
            let key = rules::archive_entry_key(&format!("{}.{}", tail.stem, tail.main_ext))
                .context("缺主包族无法归组")?;
            if seen.insert(key) {
                enqueue_archive(job, path, depth)?;
            }
        }
    }
    Ok(())
}

/// 流式压缩包去掉**一层**压缩后缀后的名字；不是流式格式时返回 None。
/// 后缀集合与 X-01 白名单里的压缩流一致（`.tar.<流后缀>` 只去一层，剩下的 `.tar`
/// 由 tar 处理逻辑继续展开）。
fn stream_stem(archive: &Path) -> Option<String> {
    let name = archive.file_name()?.to_str()?.to_string();
    let lower = name.to_ascii_lowercase();
    for suffix in [
        ".tgz", ".tbz2", ".tbz", ".txz", ".tzst", ".bz2", ".gz", ".xz", ".lzma", ".zst", ".z",
    ] {
        if lower.ends_with(suffix) {
            let stem = &name[..name.len() - suffix.len()];
            if stem.is_empty() {
                return None;
            }
            // tgz/tbz/tbz2/txz/tzst 本质是 tar 容器，名字里补回 .tar
            if matches!(suffix, ".tgz" | ".tbz" | ".tbz2" | ".txz" | ".tzst")
                && !stem.to_ascii_lowercase().ends_with(".tar")
            {
                return Some(format!("{stem}.tar"));
            }
            return Some(stem.to_string());
        }
    }
    None
}
/// 流式压缩包（bzip2/xz/gzip）成员名：7-Zip 有时不输出 Path，用包名去掉**一层**压缩后缀合成。
pub(super) fn stream_member_name(archive: &Path) -> String {
    if let Some(stem) = stream_stem(archive) {
        return stem;
    }
    let name = archive
        .file_name()
        .map_or_else(|| "content".into(), |s| s.to_string_lossy().into_owned());
    match archive.file_stem() {
        Some(stem) => stem.to_string_lossy().into_owned(),
        None => name,
    }
}

/// 解析 RAR 新式分卷名：把小写文件名拆成主干与 `.partN` 的数字串。
/// 与 rules 的分卷规则对齐：ASCII 正整数，允许前导零且不受整数机器宽度限制；
/// report.partial.rar、report.part0.rar 不得当作分卷。
fn split_rar_part(name: &str) -> Option<(&str, &str)> {
    let base = name.strip_suffix(".rar")?;
    let (stem, part) = base.rsplit_once(".part")?;
    (!part.is_empty()
        && part.bytes().all(|b| b.is_ascii_digit())
        && part.bytes().any(|b| b != b'0'))
    .then_some((stem, part))
}
/// 将小写文件名解析为 RAR 新式分卷主干（去掉末尾 `.partN.rar` 后的部分）。
pub(super) fn rar_part_stem(name: &str) -> Option<&str> {
    split_rar_part(name).map(|(stem, _)| stem)
}
/// part rar 族的卷号数字串（`a.part01.rar` → `01`），判定与 [`rar_part_stem`] 同口径。
fn part_digits(name: &str) -> Option<&str> {
    split_rar_part(name).map(|(_, part)| part)
}

pub(super) struct ArchiveVolumes {
    pub(super) kind: String,
    pub(super) count: usize,
    pub(super) new_rar_names: bool,
}

/// 分卷命名仅描述引擎逐卷打开时使用的路径；是否多卷以及实际数量必须由引擎确认。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VolumeScheme {
    Single,
    RarParts,
    Numbered,
    OldRar,
    SplitZip,
}
impl VolumeScheme {
    pub(super) fn actual_paths(
        self,
        archive: &Path,
        info: &ArchiveVolumes,
    ) -> Result<Vec<PathBuf>> {
        let kind = &info.kind;
        let count = info.count;
        if count == 1 {
            return Ok(vec![archive.to_path_buf()]);
        }
        let compatible = match self {
            Self::RarParts | Self::OldRar => {
                kind.eq_ignore_ascii_case("rar") || kind.eq_ignore_ascii_case("rar5")
            }
            Self::Numbered => kind.eq_ignore_ascii_case("split"),
            Self::SplitZip => kind.eq_ignore_ascii_case("zip"),
            Self::Single => false,
        };
        ensure!(compatible, "引擎格式与分卷命名不一致，保留源包");
        let name = archive
            .file_name()
            .and_then(|name| name.to_str())
            .context("无效分卷名称")?;
        // X-10：档案头证明卷数，但不能扩大精确命名族的删除授权。
        ensure!(
            self != Self::OldRar || (!info.new_rar_names && !kind.eq_ignore_ascii_case("rar5")),
            "RAR 新式卷名不属于老式 .rNN 命名族，保留源包"
        );
        ensure!(
            match self {
                Self::OldRar => count <= 101,
                Self::SplitZip => count <= 100,
                _ => true,
            },
            "实际卷数超出 X-10 精确命名族，保留源包"
        );
        let (base, _) = name.rsplit_once('.').context("无效分卷名称")?;
        let mut rar_number = if self == Self::RarParts {
            base.as_bytes()
                .iter()
                .rposition(u8::is_ascii_digit)
                .map(|last| {
                    let end = last + 1;
                    let start = base.as_bytes()[..end]
                        .iter()
                        .rposition(|byte| !byte.is_ascii_digit())
                        .map_or(0, |position| position + 1);
                    (start, end, name.as_bytes()[start..end].to_vec())
                })
        } else {
            None
        };
        let mut paths = vec![archive.to_path_buf()];
        for index in 1..count {
            let next = match self {
                Self::RarParts | Self::OldRar => {
                    if let Some((start, end, digits)) = &mut rar_number {
                        let mut carry = true;
                        for digit in digits.iter_mut().rev() {
                            if *digit == b'9' {
                                *digit = b'0';
                            } else {
                                *digit += 1;
                                carry = false;
                                break;
                            }
                        }
                        if carry {
                            digits.insert(0, b'1');
                        }
                        format!(
                            "{}{}{}",
                            &name[..*start],
                            std::str::from_utf8(digits)?,
                            &name[*end..]
                        )
                    } else {
                        format!("{base}.r{:02}", index - 1)
                    }
                }
                Self::Numbered => format!("{base}.{:03}", index + 1),
                Self::SplitZip => format!("{base}.z{index:02}"),
                Self::Single => unreachable!(),
            };
            let path = archive.with_file_name(next);
            let metadata = fs::symlink_metadata(&path)
                .with_context(|| format!("实际分卷不可访问：{}", path.display()))?;
            ensure!(
                metadata.file_type().is_file(),
                "实际分卷不是普通文件，保留源包：{}",
                path.display()
            );
            paths.push(path);
        }
        Ok(paths)
    }
}

/// 名称是否以规范 ASCII 数字分卷后缀结尾（最少三位补零，超过三位自然增长）。
/// 主干按末点切分；`.000` 保留在族中，由预检整组拒绝非法起始卷。
pub(super) fn numbered_entry(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(stem, digits)| !stem.is_empty() && fsutil::is_numbered_volume_digits(digits))
}

/// 候选组供可逆隔离使用；成功删除前必须转换成经引擎确认的实际卷集合。
#[derive(Debug)]
pub(super) struct VolumeSet {
    pub(super) paths: Vec<PathBuf>,
    pub(super) scheme: VolumeScheme,
    /// X-10 part rar 命名歧义组（同一卷号数值存在多种补零写法，如 part1 与
    /// part01 并存）的描述；Some 时调用方必须整组按失败包处置——不猜测归属、
    /// 不解码删源。宽匹配的 paths 仍是整组卷，隔离可复用。
    pub(super) part_ambiguity: Option<String>,
}

/// X-10：该文件是老式族尾卷、且同目录没有对应主包（`主干.zip`/`主干.rar`）。
/// 只发现这些尾卷时整组按失败包处置并报告缺主包（X-10：缺入口仍是一组失败包）。
/// X-10 卷集形态预检（纯文件系统判定，不依赖 7-Zip 引擎）：命名歧义组、老式族
/// 缺主包、part rar 缺首卷、数字尾卷缺入口，任一命中即整组按失败包隔离。
/// E-05 边界：这类隔离不是解压，不得因宿主无引擎而被整体报错吞掉——预检必须
/// 先于引擎解析执行（回归：CI run 37131023474 上缺首卷组因引擎解析前置报错，
/// 未能按 X-06 隔离）。
pub(super) fn x10_volume_precheck(
    archive_rel: &str,
    archive: &Path,
    named: &VolumeSet,
) -> Result<()> {
    // X-10：同主干混用补零模式（如 part1 与 part01 并存）是命名歧义组：
    // 列出全部歧义卷，不猜测归属、不解码删源——整组按失败包走 X-06 隔离。
    if let Some(ambiguity) = &named.part_ambiguity {
        anyhow::bail!("X-10 命名歧义分卷组：{archive_rel}（{ambiguity}）");
    }
    // X-10：老式 zip/rar 族只发现尾卷、没有主包时整组按失败包处置并报告缺主包，
    // 不把尾卷交给引擎猜格式（引擎对孤立 .zNN/.rNN 的报错只会说「无法打开」，
    // 指向不了真正原因）。
    if missing_old_style_main(archive) {
        anyhow::bail!(
            "老式分卷族缺主包：{archive_rel}（只发现 .zNN/.rNN 尾卷，未找到 主干.zip/主干.rar）"
        );
    }
    // X-10：part rar 族缺首卷——当前卷是 partN（N≥2）且同目录没有对应宽度的
    // part1 入口（缺首卷组由扫描按代表入队）。整组按失败包处置并报告缺首卷，
    // 不把余卷当独立包或交给引擎猜。附录 E：a.part02+a.part03 缺 01 → 一组
    // 缺首卷失败，不当两个完整 rar 解压。
    if let Some(reason) = missing_part_rar_first(archive) {
        anyhow::bail!("part rar 分卷族缺首卷：{archive_rel}（{reason}）");
    }
    // X-10：.000 不是入口；它即使与 .001 或后续卷共存也使整组非法。
    // 入口扫描会将 .000 入队，后续卷检查也必须能从卷集合中发现它。
    if named.scheme == VolumeScheme::Numbered {
        if let Some(zero) = named.paths.iter().find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".000"))
        }) {
            anyhow::bail!(
                "数字尾卷族包含非法起始卷：{archive_rel}（{}）",
                zero.display()
            );
        }
    }
    // X-10：数字尾卷族缺入口——规范编号 ≥ 2 且同目录没有 .001 入口。
    // .000 已在上面作为非法起始卷单独拒绝，不得用它替代 .001。
    if let Some(stem) = missing_numbered_entry(archive) {
        anyhow::bail!("数字尾卷族缺起始卷：{archive_rel}（未找到 {stem}.001 入口卷）");
    }
    if named.scheme != VolumeScheme::Single {
        let mut numbers = Vec::with_capacity(named.paths.len());
        for path in &named.paths {
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .context("分卷名称无法无损表示")?
                .to_ascii_lowercase();
            let number = match named.scheme {
                VolumeScheme::Numbered => name
                    .rsplit_once('.')
                    .context("无效数字分卷名")?
                    .1
                    .parse::<u64>()
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "X-10 分卷不全：{archive_rel}（编号超出本组可能的连续范围）"
                        )
                    })?,
                VolumeScheme::RarParts => part_digits(&name)
                    .context("无效 part 分卷名")?
                    .parse::<u64>()
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "X-10 分卷不全：{archive_rel}（编号超出本组可能的连续范围）"
                        )
                    })?,
                VolumeScheme::OldRar | VolumeScheme::SplitZip => {
                    if rules::old_style_tail(&name).is_some() {
                        let number = name[name.len() - 2..].parse::<u64>()?;
                        if named.scheme == VolumeScheme::OldRar {
                            number + 1
                        } else {
                            number
                        }
                    } else {
                        0
                    }
                }
                VolumeScheme::Single => unreachable!(),
            };
            numbers.push(number);
        }
        numbers.sort_unstable();
        let start = u64::from(matches!(
            named.scheme,
            VolumeScheme::Numbered | VolumeScheme::RarParts
        ));
        for (offset, number) in numbers.into_iter().enumerate() {
            ensure!(
                number == start + u64::try_from(offset)?,
                "X-10 分卷不全：{archive_rel}（缺起始编号或中间断号）"
            );
        }
    }
    Ok(())
}

pub(super) fn missing_old_style_main(archive: &Path) -> bool {
    let Some(name) = archive.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    let Some(tail) = rules::old_style_tail(&lower) else {
        return false;
    };
    !fs::symlink_metadata(archive.with_file_name(format!("{}.{}", tail.stem, tail.main_ext)))
        .is_ok_and(|metadata| metadata.file_type().is_file())
}

/// X-10：该文件是 partN（N≥2）的分卷、且同目录没有同主干同宽度的 part1 入口。
/// 返回缺首卷的说明；不是该形态或入口在场时返回 None。任意补零写法均以卷号 1 为入口，
/// 同主干宽度冲突由 volume_set 另行判为命名歧义。
fn missing_part_rar_first(archive: &Path) -> Option<String> {
    let name = archive.file_name().and_then(|s| s.to_str())?;
    let lower = name.to_ascii_lowercase();
    let (stem, digits) = split_rar_part(&lower)?;
    let number = digits.trim_start_matches('0');
    if number == "1" {
        return None;
    }
    // 入口按卷号数值判定（X-10「位数超过最小宽度时自然增长」）：`part1` 在场
    // 即有入口，任何写法（part1/part01/part001）都算——按当前卷的补零宽度拼
    // `part01` 会把完整无补零 10+ 卷集（…part9 + part10）误判成缺首卷。
    if sibling_part_number_one(archive, stem) {
        return None;
    }
    Some(format!(
        "未找到 {stem} 命名族的入口卷（part1，任意补零写法）"
    ))
}

/// 同目录下同主干的 part rar 卷里是否存在卷号 1（任意补零写法）。
fn sibling_part_number_one(archive: &Path, stem: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(archive.parent().unwrap_or_else(|| Path::new("."))) else {
        // 无法枚举兄弟卷时保守视为入口在场，交由引擎整包校验兜底。
        return true;
    };
    let stem_key = fsutil::fold_rel(stem);
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().into_string().ok() else {
            continue;
        };
        let lower = name.to_ascii_lowercase();
        let Some((sibling_stem, sibling_digits)) = split_rar_part(&lower) else {
            continue;
        };
        if fsutil::fold_rel(sibling_stem) == stem_key
            && sibling_digits.trim_start_matches('0') == "1"
            && entry.file_type().is_ok_and(|kind| kind.is_file())
        {
            // 卷号 1 的任意补零写法（part1/part01/part001）都是该族入口。
            return true;
        }
    }
    false
}

/// X-10：该文件是数字尾卷族的规范编号（≥ 2）、且同目录没有 `.001` 入口
///（也没有作为可识别非法起始入队的 `.000`）。返回主干名供错误指认。
fn missing_numbered_entry(archive: &Path) -> Option<String> {
    let name = archive.file_name().and_then(|s| s.to_str())?;
    let lower = name.to_ascii_lowercase();
    if !numbered_entry(&lower) {
        return None;
    }
    let (stem, digits) = lower.rsplit_once('.')?;
    // 已确认规范语法；除 .000/.001 外均是后续卷，不受机器整数宽度限制。
    if matches!(digits, "000" | "001") {
        return None;
    }
    if archive.with_file_name(format!("{stem}.001")).exists()
        || archive.with_file_name(format!("{stem}.000")).exists()
    {
        return None;
    }
    Some(stem.to_string())
}

/// RAR part 编号中显式前导零代表最小补零宽度；无前导零时返回 None。
fn part_padding_width(digits: &str) -> Option<usize> {
    let natural_width = digits.trim_start_matches('0').len().max(1);
    (digits.len() > natural_width).then_some(digits.len())
}

/// X-10：单个 part rar 候选的两处歧义状态更新——补零宽度与已见卷号写法的记录，
/// 任一冲突即写入歧义描述。判定结果与消息文本与原内联写法一致；
/// 主体自身（首卷）的无条件初始化仍留在 volume_set。
fn note_part_candidate(
    digits: &str,
    padded_width: &mut Option<usize>,
    part_seen: &mut HashMap<String, String>,
    part_ambiguity: &mut Option<String>,
) {
    if let Some(width) = part_padding_width(digits) {
        match *padded_width {
            Some(existing) if existing != width => {
                part_ambiguity.get_or_insert_with(|| "同一主干混用不同补零位数".to_string());
            }
            None => *padded_width = Some(width),
            _ => {}
        }
    }
    let key = digits.trim_start_matches('0');
    match part_seen.get(key) {
        Some(existing) if existing != digits && part_ambiguity.is_none() => {
            *part_ambiguity = Some(format!(
                "同一卷号存在多种补零写法（{existing} 与 {digits} 并存）"
            ));
        }
        None => {
            part_seen.insert(key.to_string(), digits.to_string());
        }
        _ => {}
    }
}

/// 分卷组解析：返回主体自身 + 同目录下的兄弟卷（X-05 删除与 X-06 隔离的处置单位）。
/// 非分卷包（命名不能匹配任何分卷方案）返回只含主体自身的单项集合，且**不枚举目录**：
/// 单卷 7z/tar/gz 等格式没有可匹配的兄弟卷命名，逐包扫描目录是纯粹的重复工作。
/// 识别口径与旧 protect_volumes 一致：分卷识别只走 rar_part_stem，不用 starts_with
/// 宽匹配（report.partial.rar 不是分卷）。
pub(super) fn volume_set(archive: &Path) -> Result<VolumeSet> {
    let name = archive
        .file_name()
        .and_then(|s| s.to_str())
        .context("无效压缩包名称")?
        .to_ascii_lowercase();
    let (stem, scheme) = if let Some(stem) = rar_part_stem(&name) {
        (stem.to_string(), VolumeScheme::RarParts)
    } else if numbered_entry(&name) {
        // X-10：主包完整名带白名单扩展名（含复合扩展名），再带最少三位数字后缀。
        // 入口侧（rules）只放行白名单主包名；这里复用规范编号语法，按末点去掉编号。
        let (stem, _) = name.rsplit_once('.').context("无效数字分卷名")?;
        (stem.to_string(), VolumeScheme::Numbered)
    } else if let Some(tail) = rules::old_style_tail(&name) {
        // X-10：老式族尾卷按其族方案成组——主包缺位时（尾卷组已按缺主包入队），
        // 失败处置与隔离仍能按整组移动；主包在场时尾卷不会单独走到这里（不单独入队）。
        let scheme = if tail.main_ext == "zip" {
            VolumeScheme::SplitZip
        } else {
            VolumeScheme::OldRar
        };
        (tail.stem.to_string(), scheme)
    } else if let Some(stem) = name.strip_suffix(".rar") {
        (stem.to_string(), VolumeScheme::OldRar)
    } else if let Some(stem) = name.strip_suffix(".zip") {
        (stem.to_string(), VolumeScheme::SplitZip)
    } else {
        return Ok(VolumeSet {
            paths: vec![archive.to_path_buf()],
            scheme: VolumeScheme::Single,
            part_ambiguity: None,
        });
    };
    // 语法只折叠 ASCII；主干用文件系统序数键，不能先 Unicode 展开再猜测卷归属。
    let stem_key = fsutil::fold_rel(&stem);
    let matches_candidate = |candidate: &str| -> bool {
        match scheme {
            VolumeScheme::RarParts => {
                rar_part_stem(candidate).is_some_and(|stem| fsutil::fold_rel(stem) == stem_key)
            }
            VolumeScheme::Numbered => candidate.rsplit_once('.').is_some_and(|(stem, digits)| {
                fsutil::is_numbered_volume_digits(digits) && fsutil::fold_rel(stem) == stem_key
            }),
            VolumeScheme::OldRar | VolumeScheme::SplitZip => rules::old_style_tail(candidate)
                .is_some_and(|tail| {
                    (tail.main_ext == "rar") == (scheme == VolumeScheme::OldRar)
                        && fsutil::fold_rel(tail.stem) == stem_key
                }),
            VolumeScheme::Single => false,
        }
    };
    let mut paths = vec![archive.to_path_buf()];
    // X-10 part rar 命名歧义检测：混用补零宽度或同一卷号有不同写法都不猜测归属。
    // 键为去前导零的十进制串，正整数卷号不受机器整数宽度限制。
    let mut part_ambiguity: Option<String> = None;
    let mut padded_width: Option<usize> = None;
    let mut part_seen: HashMap<String, String> = HashMap::new();
    if matches!(scheme, VolumeScheme::RarParts) {
        if let Some(digits) = part_digits(&name) {
            part_seen.insert(
                digits.trim_start_matches('0').to_string(),
                digits.to_string(),
            );
            padded_width = part_padding_width(digits);
        }
    }
    for entry in fs::read_dir(archive.parent().context("压缩包缺少目录")?)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let candidate = name.to_ascii_lowercase();
        // 主体自身已在集合里（如 part1.rar 对主干同判）；大小写不敏感路径上可能重复命名，去重交给文件系统唯一性。
        if matches_candidate(&candidate) && entry.path() != archive && entry.file_type()?.is_file()
        {
            if matches!(scheme, VolumeScheme::RarParts) {
                if let Some(digits) = part_digits(&candidate) {
                    note_part_candidate(
                        digits,
                        &mut padded_width,
                        &mut part_seen,
                        &mut part_ambiguity,
                    );
                }
            }
            paths.push(entry.path());
        }
    }
    if part_ambiguity.is_none()
        && padded_width.is_some_and(|width| {
            part_seen.values().any(|digits| {
                let natural_width = digits.trim_start_matches('0').len().max(1);
                digits.len() != width.max(natural_width)
            })
        })
    {
        part_ambiguity = Some("同一主干混用补零模式".to_string());
    }
    // 列出全部歧义卷：整组卷清单随描述一并返回（X-10「列出全部歧义卷」）。
    let part_ambiguity = part_ambiguity.map(|reason| {
        let names = paths
            .iter()
            .filter_map(|path| path.file_name().and_then(|name| name.to_str()))
            .collect::<Vec<_>>()
            .join("、");
        format!("{reason}；整组卷：{names}")
    });
    Ok(VolumeSet {
        paths,
        scheme,
        part_ambiguity,
    })
}
