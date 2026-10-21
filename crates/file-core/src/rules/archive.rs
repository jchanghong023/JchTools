use super::cleanup::has_ext;
use crate::fsutil;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// X-01 / X-10：解压白名单与分卷
// ---------------------------------------------------------------------------

/// X-01：自动解压白名单。判断口径不是「这个文件是不是压缩格式」，而是「它是不是
/// 用户意义上的归档文件」——`.cab` 是安装介质的一部分（Office/驱动/更新包），
/// `.iso`/`.wim`/`.esd` 是系统镜像，`.msi`/`.msix`/`.appx` 是安装包，`.jar`/`.apk`/
/// `.whl`/`.nupkg` 是程序包，`.docx`/`.xlsx`/`.epub`/`.odt` 是文档容器，`.exe`/`.com`
/// 可能是自解压包：它们即使能被 7-Zip 打开，也一律不自动解压（只看扩展名，不猜内容）。
/// 分卷只认可独立解开的第一卷：数字尾卷族认白名单后缀 + 规范编号（最少三位）的入口卷
/// （`.001`；`.000` 是可识别的非法起始编号，入队后整组失败隔离）、`.partN.rar` 认
/// part1；其余卷（`.z01`/`.r00`/`.002`）由删除与隔离的卷集合逻辑成组处理，不单独入队。
/// `.partN.rar` 分卷正则：[`archive_name`] 与 [`multipart_name`] 共用同一实例，
/// 保证「只有 `.partN.rar` 才算 RAR 分卷」的口径只有一处实现。
fn part_rar_regex() -> &'static regex::Regex {
    static PART: OnceLock<regex::Regex> = OnceLock::new();
    PART.get_or_init(|| match regex::Regex::new(r"\.part(0*[1-9][0-9]*)\.rar$") {
        Ok(re) => re,
        // 常量正则语法错误只可能是开发期笔误，按不可达处理
        Err(_) => unreachable!("constant regex"),
    })
}
pub fn archive_name(name: &str) -> bool {
    // 附录 B：只有首位点的名称整体为主体，不凭主体字样授权解压、删源。
    if name.starts_with('.') && !name[1..].contains('.') {
        return false;
    }
    let name = name.to_lowercase();
    if has_ext(&name, "rar") {
        // 只有 `.partN.rar` 的 part1 算分卷主体；`report.partial.rar` 这类普通包不受影响。
        let re = part_rar_regex();
        if let Some(caps) = re.captures(&name) {
            return caps[1].trim_start_matches('0') == "1";
        }
        return true;
    }
    // X-10 数字尾卷族入口：主包完整名具有白名单任一后缀（含复合扩展名），再带规范
    // 编号（最少三位补零，`.999` 后自然增长）。合法起始编号是 `.001`；末尾 `.000`
    // 是该族的非法起始编号，同样按族
    // 识别入队——捆绑引擎打不开错误起始编号的分卷集，解压失败后按 X-06 把整组卷集
    // 移入「解压失败」，不作为普通独立包处理。`.002` 起的后续卷不是入口，由 archive.rs
    // 的卷集合逻辑成组删除/隔离，不单独入队（一组仅入队、计数、判定一次）。
    if let Some((_, digits)) = numbered_volume_tail(&name) {
        return matches!(digits, "000" | "001");
    }
    // 白名单之外的一切格式（含引擎能打开的 cab/iso/wim/lzh/cpio 等）都不自动解压：
    // 维护「禁止列表」必然漏掉新出现的容器格式，白名单是唯一可靠的边界。
    // 压缩流（.gz/.bz2/.xz/.zst/.lzma/.z）允许前面再带一层 `.tar`，整体作为一个包
    // （`.tar.gz` 等由流后缀本身覆盖，`.tgz`/`.tbz2`/`.txz`/`.tzst` 是别名）。
    // `.lz4` / `.br` / `.lz`（真 lzip）不在白名单内：捆绑引擎没有对应解码器，
    // 放进来只会把这类文件一律推进「解压失败」，不如完全不碰（X-09）。
    // 卷（`.002`、`.part2.rar`、`.r00`、`.z01`）不是独立解压对象，不在这里放行；
    // 白名单外格式的卷（`.iso.001`）与配不上主包的孤立编号文件（`data.001`）同样不匹配。
    ARCHIVE_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
}
/// X-02/X-10：同目录候选入口的唯一包键；非法 `.000` 与 `.001`、歧义
/// `part1/part01/part001` 各只计一包，普通主包与数字尾卷族严格分开。
pub fn archive_entry_key(name: &str) -> Option<String> {
    if !archive_name(name) {
        return None;
    }
    let name = name.to_ascii_lowercase();
    if let Some((rest, _)) = numbered_volume_tail(&name) {
        return Some(format!("numbered:{}", fsutil::fold_rel(rest)));
    }
    if let Some(matched) = part_rar_regex().find(&name) {
        return Some(format!(
            "part-rar:{}",
            fsutil::fold_rel(&name[..matched.start()])
        ));
    }
    Some(format!("single:{}", fsutil::fold_rel(&name)))
}
/// X-01 白名单后缀（小写；压缩流允许前面再带一层 `.tar` 的复合形式由 `.gz` 等
/// 流后缀整体覆盖）。数字尾卷族按 [`numbered_volume_tail`] 另行判定。
const ARCHIVE_SUFFIXES: [&str; 14] = [
    ".zip", ".7z", ".rar", ".tar", ".gz", ".bz2", ".xz", ".zst", ".lzma", ".z", ".tgz", ".tbz2",
    ".txz", ".tzst",
];
/// X-10 数字尾卷族：名称（小写）以至少三位 ASCII 十进制数字结尾，超过三位无前导零，
/// 且去掉数字尾卷后的剩余部分以 X-01 白名单任一后缀结尾（`.tar.gz` 天然被 `.gz` 覆盖）时
/// 返回 (剩余部分, 完整编号)，否则 None。`.iso.001`、`.docx.001`、`data.001` 这类
/// 白名单外或无格式的编号文件不构成该族（X-09 完全不碰）。
pub(super) fn numbered_volume_tail(name_lower: &str) -> Option<(&str, &str)> {
    let (rest, digits) = name_lower.rsplit_once('.')?;
    if !fsutil::is_numbered_volume_digits(digits) {
        return None;
    }
    ARCHIVE_SUFFIXES
        .iter()
        .any(|suffix| rest.len() > suffix.len() && rest.ends_with(suffix))
        .then_some((rest, digits))
}
/// X-10 老式分卷族的尾卷形态：`主干.z01`～`主干.z99`（zip 族，编号从 01 起）与
/// `主干.r00`～`主干.r99`（rar 族，编号从 00 起）。入口是 `主干.zip` / `主干.rar`；
/// 只发现尾卷、没有主包时仍归组报告缺主包（入队与清点见
/// [`count_tail_only_old_style_groups`]，缺主包处置在 archive::missing_old_style_main）。
pub struct OldStyleTail<'a> {
    pub stem: &'a str,
    /// 该族主包的扩展名（`zip` 或 `rar`）。
    pub main_ext: &'static str,
}
pub fn old_style_tail(name_lower: &str) -> Option<OldStyleTail<'_>> {
    let bytes = name_lower.as_bytes();
    // 形态：倒数第四字节是 `.`，倒数第三是 `z`/`r`，末两位是数字（`.z01`/`.r00`）。
    if bytes.len() < 5 || bytes[bytes.len() - 4] != b'.' {
        return None;
    }
    let main_ext = match bytes[bytes.len() - 3] {
        b'z' => "zip",
        b'r' => "rar",
        _ => return None,
    };
    let digits = &name_lower[name_lower.len() - 2..];
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // zip 族的起始编号是 01（`z00` 不属命名族，与 category_for 口径一致）；rar 族从 00 起。
    if main_ext == "zip" && digits == "00" {
        return None;
    }
    let stem = &name_lower[..name_lower.len() - 4];
    if stem.is_empty() {
        return None;
    }
    Some(OldStyleTail { stem, main_ext })
}
/// X-10/X-02：文件名清单只做 ASCII 语法小写，保留 Unicode 主干，再按 Windows 序数
/// 忽略大小写统计老式族组数——残缺但可归组的卷集计一包；主包在场的
/// 尾卷属于其卷集，不计；同主干同族的多个尾卷只计一组（一组仅入队、计数、判定一次）。
pub fn count_tail_only_old_style_groups(names_lower: &[String]) -> u64 {
    let files: HashSet<String> = names_lower
        .iter()
        .map(|name| fsutil::fold_rel(name))
        .collect();
    let mut seen: HashSet<(String, &'static str)> = HashSet::new();
    let rar_suffix = fsutil::fold_rel(".rar");
    let zip_suffix = fsutil::fold_rel(".zip");
    let mut count = 0u64;
    for name in names_lower {
        let Some(tail) = old_style_tail(name) else {
            continue;
        };
        let mut key = fsutil::fold_rel(tail.stem);
        let stem_length = key.len();
        key.push_str(if tail.main_ext == "rar" {
            &rar_suffix
        } else {
            &zip_suffix
        });
        if files.contains(&key) {
            continue;
        }
        key.truncate(stem_length);
        if !seen.insert((key, tail.main_ext)) {
            continue;
        }
        count += 1;
    }
    count
}
/// X-10 part rar 族「缺入口卷 part1」的残缺组：返回每组一个代表卷的文件名
/// （清单序首个，确定性排序），供确认清点（X-02）与入队复用。
/// 分组键 = 主干，入口判定按**卷号数值**：`part1` 在场即有入口——「位数超过
/// 最小宽度时自然增长」（X-10），`a.part1.rar…a.part9.rar` 与 `a.part10.rar`
/// 属同一族，不得把宽 2 的自然增长卷拆成「缺 part01」的伪组（完整 10+ 卷集
/// 曾被误计为两包并可能整组误隔离）。附录 E：`a.part02.rar`、`a.part03.rar`
/// 缺 01 → 一组缺首卷失败，不当两个完整 rar 解压；同一卷号混用补零宽度
/// （`part1` 与 `part01` 并存）属命名歧义，由 archive 侧整组失败处置。
pub fn part_rar_missing_first_groups(names_lower: &[String]) -> Vec<String> {
    let re = part_rar_regex();
    // 主干 ->（卷号 1 是否在场〔任意写法〕, 最小卷号 N≥2 的代表名）
    let mut groups: HashMap<String, (bool, Option<(String, String)>)> = HashMap::new();
    for name in names_lower {
        let Some(caps) = re.captures(name) else {
            continue;
        };
        // 正整数不受机器字宽限制；去掉前导零后按位数与字典序比较。
        let number = caps[1].trim_start_matches('0');
        let Some(matched) = caps.get(0) else {
            continue;
        };
        let stem = &name[..name.len() - matched.as_str().len()];
        let entry = groups
            .entry(fsutil::fold_rel(stem))
            .or_insert((false, None));
        if number == "1" {
            entry.0 = true;
        } else {
            let better = entry.1.as_ref().is_none_or(|(smallest, _)| {
                (number.len(), number) < (smallest.len(), smallest.as_str())
            });
            if better {
                entry.1 = Some((number.to_string(), name.clone()));
            }
        }
    }
    let mut representatives: Vec<String> = groups
        .into_values()
        .filter(|(has_first, _)| !has_first)
        .filter_map(|(_, representative)| representative.map(|(_, name)| name))
        .collect();
    representatives.sort();
    representatives
}
/// X-10 数字尾卷族「只有 `.NNN`（N≥2）、缺 `.001` 入口（也无 `.000` 非法起始
/// 标记）」的残缺组：返回每组一个代表卷的文件名。`.000` 由 [`archive_name`]
/// 作为可识别非法起始直接入队，这里只收集完全没有入口的组；同族多次出现只计
/// 一组（一组仅入队、计数、判定一次）。
pub fn numbered_volume_missing_entry_groups(names_lower: &[String]) -> Vec<String> {
    // 主干 ->（入口 `.000`/`.001` 是否在场, 首个 N≥2 代表名）
    let mut groups: HashMap<String, (bool, Option<String>)> = HashMap::new();
    for name in names_lower {
        let Some((stem, digits)) = numbered_volume_tail(name) else {
            continue;
        };
        let entry = groups
            .entry(fsutil::fold_rel(stem))
            .or_insert((false, None));
        if digits == "000" || digits == "001" {
            // `.001` 是合法入口、`.000` 是可识别的非法起始编号，两者都已由
            // [`archive_name`] 作为该组入口入队，不属「缺入口」组。
            entry.0 = true;
        } else if entry.1.is_none() {
            entry.1 = Some(name.clone());
        }
    }
    let mut representatives: Vec<String> = groups
        .into_values()
        .filter(|(has_entry, _)| !has_entry)
        .filter_map(|(_, representative)| representative)
        .collect();
    representatives.sort();
    representatives
}
pub fn multipart_name(name: &str) -> bool {
    let n = name.to_lowercase();
    // 与 archive_name 对齐：只有 .partN.rar 才算 RAR 分卷；contains(".part") 会把
    // report.partial.rar 这类普通包误判为分卷。
    let re = part_rar_regex();
    if re.is_match(&n) {
        return true;
    }
    // 数字尾卷族入口与 archive_name 同口径（X-10）：规范编号（最少三位）+ 白名单主干，
    // .001 为合法入口、.000 为可识别的非法起始编号；.002 起的后续卷与白名单外
    // 编号（data.001 等）都不是组入口。
    matches!(numbered_volume_tail(&n), Some((_, "000" | "001")))
}
