use super::validate::{relative_string, safe_join, validate_component};
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// 返回目标直接父目录是否由本次调用实际创建；既有目录不授予收尾删除所有权。
pub fn ensure_parent(root: &Path, target: &Path) -> Result<bool> {
    let rel = relative_string(root, target)?;
    safe_join(root, &rel)?;
    let parent = target.parent().context("目标没有父目录")?;
    let creation = match fs::create_dir(parent) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(parent.parent().context("目标父目录没有上层目录")?)?;
            fs::create_dir(parent)
        }
        result => result,
    };
    let created = match creation {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => return Err(error.into()),
    };
    safe_join(root, &rel)?;
    Ok(created)
}
/// 创建目录链本身（`directory` 就是要创建的目录，不再上溯一层）。
/// `ensure_parent` 只创建传入路径的父级，拿目录调用它只会创建到祖父目录：
/// 解压成员合入必须用本函数，否则带子目录的成员会以「系统找不到指定的路径」整包失败。
/// 与 `ensure_parent` 一样对目录链做链接/junction 校验（拒绝把成员写进重定向目录），
/// 但不校验 `directory` 之下的最终名——那是成员的最终文件名，调用方另有落盘兜底。
pub fn ensure_dir(root: &Path, directory: &Path) -> Result<()> {
    let rel = relative_string(root, directory)?;
    if rel.is_empty() {
        return Ok(());
    } // 选定的根目录自身，无需创建
    safe_join(root, &rel)?;
    fs::create_dir_all(directory)?;
    safe_join(root, &rel)?;
    Ok(())
}
/// 生成「stem (N)ext」形式的候选名；整体超过 Windows 单组件上限（255 个 UTF-16
/// 单元）时按「序号标记 → 扩展名 → stem」的优先级截断，而不是让 validate_component
/// 把整次分配搞失败。截断按 UTF-16 单元预算逐字符进行，不会切开代理对；扩展名
/// 截断后剥掉尾部 ASCII 点/空格，保证结果仍是合法组件。ext 自带前导点（可为空串）。
pub fn suffixed_candidate(stem: &str, ext: &str, index: u64) -> String {
    let sep = format!(" ({index})");
    let budget = 255usize.saturating_sub(sep.encode_utf16().count());
    // stem 非空时至少给 stem 保留 1 个单元，避免扩展名占满预算后候选名以序号空格开头。
    let ext_cap = budget.saturating_sub(usize::from(!stem.is_empty()));
    // 只剥掉 Windows 禁止的尾部 ASCII 点/空格；合法 Unicode 空白属于原扩展名。
    let ext: String = truncate_utf16(ext, ext_cap)
        .trim_end_matches(['.', ' '])
        .to_string();
    let budget = budget - ext.encode_utf16().count();
    let cut = truncate_utf16(stem, budget);
    format!("{cut}{sep}{ext}")
}
/// 按 UTF-16 单元上限截断字符串：逐字符累计 len_utf16，不切开代理对。
fn truncate_utf16(text: &str, max_units: usize) -> String {
    let mut out = String::new();
    let mut units = 0usize;
    for ch in text.chars() {
        let need = ch.len_utf16();
        if units + need > max_units {
            break;
        }
        units += need;
        out.push(ch);
    }
    out
}
/// X-10 数字尾卷编号：至少三位 ASCII 数字；超过三位时不得有多余前导零。
/// `.000` 保留为可识别的非法起始编号，合法起始与入口由调用方判定。
pub fn is_numbered_volume_digits(digits: &str) -> bool {
    digits.len() >= 3
        && digits.bytes().all(|byte| byte.is_ascii_digit())
        && (digits.len() == 3 || digits.as_bytes()[0] != b'0')
}

/// X-10：规范数字尾卷编号（最少三位补零，`.999` 后自然增长）前只并入最后一个白名单扩展名；
/// `.tar.<流后缀>` 再按附录 B 整体并入一层。
fn is_volume_suffix_component(component: &str) -> bool {
    [
        "gz", "bz2", "xz", "zst", "lzma", "z", "7z", "zip", "rar", "tar", "tgz", "tbz2", "txz",
        "tzst",
    ]
    .iter()
    .any(|kind| component.eq_ignore_ascii_case(kind))
}

/// 附录 B：只有六类压缩流允许与紧邻的 `.tar` 组成复合扩展名。
fn tar_stream_split(stem: &str, extension: &str) -> Option<usize> {
    if !["gz", "bz2", "xz", "zst", "lzma", "z"]
        .iter()
        .any(|kind| extension.eq_ignore_ascii_case(kind))
    {
        return None;
    }
    stem.rfind('.')
        .filter(|&dot| dot > 0 && stem[dot + 1..].eq_ignore_ascii_case("tar"))
}

/// 内段是否为 part rar 的编号段（`part<正整数>`；X-10 / 附录 B）。
fn is_part_rar_component(component: &str) -> bool {
    // 先验证切分点在字符边界上，避免多字节字符处切出 panic。
    component.len() > 4 && component.is_char_boundary(4) && {
        let (head, tail) = component.split_at(4);
        head.eq_ignore_ascii_case("part")
            && tail.bytes().all(|byte| byte.is_ascii_digit())
            && tail.bytes().any(|byte| byte != b'0')
    }
}

/// 分离文件名主体与完整扩展名；扩展名含前导点，直接借用原串。
/// 不可拆分后缀（X-10 / 附录 B）：
/// - 数字尾卷（最少三位 ASCII 数字，超过三位无前导零）只并入前一白名单扩展名及允许的
///   一层 `.tar.<流后缀>`：`x.tar.gz.001` → 主体 `x` + 扩展名 `.tar.gz.001`；
///   非白名单组件不并入（`foo.bar.001` 保持主体 `foo.bar`）。
/// - part rar 的 `.partN.rar` 整体并入扩展名（`资料.part01.rar` → `资料`）。
/// - 普通复合压缩流仍按「`.tar` + 流后缀」并入一层（`资料.tar.gz` → `资料`）。
pub fn split_compound_name(name: &str) -> (&str, &str) {
    let Some(mut split) = name.rfind('.').filter(|&index| index > 0) else {
        return (name, "");
    };
    let (stem, extension) = name.split_at(split);
    if is_numbered_volume_digits(&extension[1..]) {
        if let Some(dot) = stem.rfind('.').filter(|&index| index > 0) {
            let component = &stem[dot + 1..];
            if is_volume_suffix_component(component) {
                split = tar_stream_split(&stem[..dot], component).unwrap_or(dot);
            }
        }
        return name.split_at(split);
    }
    if extension.eq_ignore_ascii_case(".rar") {
        if let Some(dot) = stem.rfind('.').filter(|&index| index > 0) {
            if is_part_rar_component(&stem[dot + 1..]) {
                return name.split_at(dot);
            }
        }
    }
    if let Some(inner_dot) = tar_stream_split(stem, &extension[1..]) {
        split = inner_dot;
    }
    name.split_at(split)
}
pub fn unique_target(root: &Path, requested: &Path) -> Result<PathBuf> {
    let parent = requested.parent().context("目标没有父目录")?;
    let name = requested
        .file_name()
        .and_then(|value| value.to_str())
        .context("无效文件名")?;
    let (stem, ext) = split_compound_name(name);
    let extension_units = ext.encode_utf16().count();
    for index in 1u64..=1_000_000 {
        if extension_units + index.ilog10() as usize + 5 > 255 {
            bail!("原扩展名过长，无法保留扩展名并追加冲突序号");
        }
        let name = suffixed_candidate(stem, ext, index);
        let path = parent.join(name);
        let rel = relative_string(root, &path)?;
        for part in rel.split('/') {
            validate_component(part)?;
        }
        // 符号链接/坏链视为占用并试下一个序号（与 planner::target_will_be_free 对齐），
        // 不得因 safe_join 的链接拒绝而整函数失败。
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(path),
            Err(error) => {
                return Err(error).with_context(|| format!("无法检查目标占用：{}", path.display()));
            }
        }
    }
    bail!(
        "无法为 {} 分配不冲突的名称：已尝试 {stem} (1)…{stem} (1000000) 均已被占用",
        requested.display()
    )
}
