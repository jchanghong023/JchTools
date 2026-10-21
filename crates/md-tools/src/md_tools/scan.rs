//! 合并输入扫描与 M-03 排序：链接边界检查、输出排除与自然排序全序决胜。
use anyhow::{bail, Context as _, Result};
use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
    time::SystemTime,
};
use walkdir::WalkDir;

use jchtools_file_core::control::Control;

/// 合并扫描出的一个输入文件（M-02/M-03）。
#[derive(Debug, Clone)]
pub struct MergeEntry {
    /// 文件路径（以所选根为前缀的绝对路径）
    pub path: PathBuf,
    /// 相对所选根的路径，分隔符统一 `/`（M-03 排序第二键）
    pub rel: String,
    /// 文件名本身（含 `.md` 后缀，M-04 标题文本）
    pub file_name: String,
}

/// Windows 口径的同一文件判定：大小写与分隔符（`/` 与 `\`）不敏感、`..`/`.` 词法消解、
/// 相对路径补当前目录后的路径文本比较。仅用于输出文件排除这一用途；排序仍用原始路径，
/// 不受本归一化影响（M-07：`sub\..\merged.md` 与 `merged.md` 指向同一文件，必须同口径排除）。
/// 大小写折叠复用 Windows 序数比较键，不使用会展开字符的 Unicode 小写转换，以免漏掉不同输入。
pub(super) fn same_file_path(a: &Path, b: &Path) -> bool {
    fn normalized_absolute_text(path: &Path) -> String {
        use std::path::Component;
        let base = if path.is_absolute() {
            PathBuf::new()
        } else {
            // 相对拼写（如界面手动输入）按当前目录补全后再比较；取不到当前目录时
            // 退化为原样消解，仅影响文本比较，不影响磁盘访问。
            std::env::current_dir().unwrap_or_default()
        };
        let joined = base.join(path);
        let mut prefix = String::new();
        let mut parts: Vec<String> = Vec::new();
        for component in joined.components() {
            match component {
                Component::Prefix(item) => {
                    let text = item.as_os_str().to_string_lossy();
                    // 普通盘符/UNC 与 canonicalize 返回的扩展拼写须排除同一输出。
                    let text = match item.kind() {
                        std::path::Prefix::VerbatimDisk(_) | std::path::Prefix::VerbatimUNC(..) => {
                            jchtools_file_core::platform::display_path_text(&text)
                        }
                        _ => text.into_owned(),
                    };
                    prefix = jchtools_file_core::fsutil::fold_rel(&text);
                }
                Component::RootDir | Component::CurDir => {}
                Component::ParentDir => {
                    let _ = parts.pop();
                }
                Component::Normal(item) => {
                    parts.push(jchtools_file_core::fsutil::fold_rel(
                        &item.to_string_lossy(),
                    ));
                }
            }
        }
        if prefix.is_empty() {
            format!("/{}", parts.join("/"))
        } else {
            format!("{prefix}/{}", parts.join("/"))
        }
    }
    normalized_absolute_text(a) == normalized_absolute_text(b)
}

/// M-02/S-04 同口径的入口检查：所选根本身与其路径上的每一级祖先都不得是符号链接 /
/// junction / 其他 reparse point。目录整理（S-04）在「所选根或其访问路径经过链接边界」
/// 时拒绝开始；MD 合并的扫描必须同口径，否则会把链接目标树当作输入读取。
fn ensure_root_is_real_directory(root: &Path, control: &Control) -> Result<()> {
    // 先补齐隐含的当前目录，但保留访问路径中的 junction 与 `..`，不能 canonicalize。
    // Windows 的 `C:docs` 依赖该驱动器当前目录，单纯 current_dir().join(root) 并不绝对。
    let absolute_root;
    let root = if root.is_absolute() {
        root
    } else {
        let mut components = root.components();
        absolute_root = match components.next() {
            Some(std::path::Component::Prefix(prefix)) => {
                let mut absolute = std::path::absolute(Path::new(prefix.as_os_str()))
                    .with_context(|| format!("无法补全输入目录：{}", root.display()))?;
                absolute.extend(components);
                absolute
            }
            _ => std::env::current_dir()
                .context("无法取得当前目录，拒绝检查相对输入目录")?
                .join(root),
        };
        &absolute_root
    };
    let mut current = PathBuf::new();
    for component in root.components() {
        control.checkpoint()?;
        current.push(component.as_os_str());
        // 扩展盘符前缀 `\\?\C:` 尚不是可查询路径，补上 RootDir 后再检查卷根。
        if matches!(component, std::path::Component::Prefix(_)) {
            continue;
        }
        match std::fs::symlink_metadata(&current) {
            Ok(meta) => {
                if jchtools_file_core::fsutil::is_link(&meta) {
                    tracing::error!(
                        event = "md_input_rejected",
                        stage = "root_boundary",
                        error_type = "link_boundary",
                        error_code = "root_link",
                        "MD输入访问路径经过链接边界，拒绝读取"
                    );
                    bail!(
                        "所选目录或其上级经过符号链接 / junction（{}）：与目录整理同口径，\
                         MD 整理不读取链接目标，请直接选择真实目录",
                        current.display()
                    );
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                tracing::error!(
                    event = "md_input_rejected",
                    stage = "root_boundary",
                    error_type = "io",
                    error_code = error.raw_os_error(),
                    "MD输入目录不存在，拒绝读取"
                );
                bail!("输入目录不存在或无法访问：{}", root.display());
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("无法检查目录边界：{}", current.display()));
            }
        }
    }
    Ok(())
}

/// M-07/M-08 数据安全：输出文件已存在且与任一输入是同一实体（硬链接、`..` 别名、
/// 大小写/分隔符变体等文本比较漏掉的同文件）时，写入会截断另一名字下的原始输入——
/// 必须在进入覆盖确认与任何写入之前拒绝整个任务，要求更换输出名或输出目录。
/// 输出不存在或无法取文件标识（如目标被目录占用）时无从比较，按常规流程继续。
pub(super) fn reject_output_aliasing_input(
    output: &Path,
    entries: &[MergeEntry],
    control: &Control,
) -> Result<()> {
    control.checkpoint()?;
    let Ok(output_snapshot) = jchtools_file_core::fsutil::snapshot(output) else {
        return Ok(());
    };
    for entry in entries {
        control.checkpoint()?;
        let Ok(snapshot) = jchtools_file_core::fsutil::snapshot(&entry.path) else {
            continue; // 单个输入取不到标识不阻断：合并打开该文件时自会给出明确错误
        };
        if snapshot.identity == output_snapshot.identity {
            tracing::error!(event = "md_output_rejected", stage = "output_alias", error_type = "same_file",
                error_code = "output_input_alias", file = %jchtools_foundation::logging::safe_error(&entry.rel), "MD输出与原始输入是同一实体，拒绝写入");
            bail!(
                "输出文件与输入是同一个文件（{} ↔ {}，硬链接或同一实体的不同写法）：\
                 合并会破坏原始输入；请更换输出文件名或输出目录",
                output.display(),
                entry.path.display()
            );
        }
    }
    Ok(())
}

fn is_markdown(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
}

/// 创建时间不可得时的兜底时间（M-03：不可得按修改时间替代，再不可得取零点）。
fn effective_created(meta: &std::fs::Metadata) -> SystemTime {
    meta.created()
        .or_else(|_| meta.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// 扫描参与合并的 Markdown 文件并按 M-03 排序：创建时间早→晚；
/// 创建时间完全相同时按相对路径自然排序（数字段按数值比较，`2.md` 在 `10.md` 之前）。
/// `exclude` 是本次输出文件路径：位于被扫描目录中时不得作为输入参与合并（M-07）。
/// 扫描不跟随符号链接 / junction 等链接边界（M-02）。
/// 扫描逐项响应暂停/取消（H-02/U-12），只读准备阶段不必等到正式合并才停止。
pub fn scan_markdown(
    root: &Path,
    recursive: bool,
    exclude: Option<&Path>,
    control: &Control,
) -> Result<Vec<MergeEntry>> {
    let span = jchtools_foundation::logging::operation_span("md_tools", "scan");
    let _entered = span.enter();
    let started = std::time::Instant::now();
    tracing::debug!(event = "md_scan_started", recursive, "MD扫描开始");
    let result: Result<Vec<MergeEntry>> = (|| {
        control.checkpoint()?;
        // M-02/S-04：根自身与全部祖先都不经过链接边界，否则拒绝整个任务
        ensure_root_is_real_directory(root, control)?;
        let mut rows: Vec<(SystemTime, MergeEntry)> = Vec::new();
        let max_depth = if recursive { usize::MAX } else { 1 };
        let mut walker = WalkDir::new(root)
            .follow_links(false)
            // walkdir 2.5 起根链接默认被跟随（与 follow_links 无关），必须显式关闭（M-02）
            .follow_root_links(false)
            .min_depth(1)
            .max_depth(max_depth)
            .into_iter();
        while let Some(item) = walker.next() {
            control.checkpoint()?;
            let entry = item.with_context(|| format!("扫描目录失败：{}", root.display()))?;
            // M-02：is_symlink 只涵盖部分 reparse tag；其他重解析文件/目录同样排除。
            // Windows 下目录项已缓存不跟随链接的属性，复用它避免额外属性读取。
            let meta = entry
                .metadata()
                .with_context(|| format!("读取文件属性失败：{}", entry.path().display()))?;
            if jchtools_file_core::fsutil::is_link(&meta) {
                if entry.file_type().is_dir() {
                    walker.skip_current_dir();
                }
                continue;
            }
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if !is_markdown(path) {
                continue;
            }
            // Windows 路径不区分大小写、两种分隔符等价：输入框与输出框对同一目录的
            // 大小写拼写不同时，按字节比较会漏排除，把本次输出误当输入合并（M-07，
            // 回归见 tests/md_tools.rs merge_excludes_output_regardless_of_path_casing）。
            if exclude.is_some_and(|out| same_file_path(path, out)) {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .map(|rel| rel.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            let file_name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            rows.push((
                effective_created(&meta),
                MergeEntry {
                    path: path.to_path_buf(),
                    rel,
                    file_name,
                },
            ));
        }
        control.checkpoint()?;
        sort_rows(&mut rows);
        let entries: Vec<MergeEntry> = rows.into_iter().map(|(_, entry)| entry).collect();
        // M-07：输出与输入是同一实体（硬链接/别名写法）时在扫描期即拒绝，
        // 界面不得进入覆盖确认——确认后写入会截断另一名字下的原始输入。
        if let Some(exclude) = exclude {
            reject_output_aliasing_input(exclude, &entries, control)?;
        }
        control.checkpoint()?;
        Ok(entries)
    })();
    match &result {
        Ok(entries) => tracing::debug!(
            event = "md_scan_completed",
            files = entries.len(),
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "MD扫描完成"
        ),
        Err(_) if control.is_cancelled() => tracing::info!(
            event = "md_scan_cancelled",
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "MD扫描已取消"
        ),
        Err(error) => {
            let io = error
                .chain()
                .find_map(|cause| cause.downcast_ref::<std::io::Error>());
            tracing::error!(event = "md_scan_failed", stage = "input_scan", error_type = "filesystem", error_code = io.and_then(std::io::Error::raw_os_error), elapsed_ms = jchtools_foundation::logging::elapsed_ms(started), error = %jchtools_foundation::logging::safe_error(&format!("{error:#}")), "MD扫描失败");
        }
    }
    result
}

/// M-03 排序落地：创建时间早→晚，同一创建时间按相对路径自然排序决胜；
/// 自然排序对数值相等但写法不同的路径（`1.md` / `01.md`）返回 Equal，
/// 追加「归一化原始路径文本（小写、分隔符统一 `/`）字节序」决胜，保证任意
/// 枚举初始顺序都得到同一最终顺序（严格全序、可重复）。
/// 单独成函数供回归测试以受控初始顺序直接驱动（目录枚举顺序不可控，无法检验全序性）。
pub(super) fn sort_rows(rows: &mut [(SystemTime, MergeEntry)]) {
    rows.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| rel_total_cmp(&a.1.rel, &b.1.rel))
    });
}

/// M-03 第二排序键的全序比较：自然排序优先，耗尽仍相等时用归一化文本决胜
///（归一规则与 `same_file_path` 的文本部分一致：小写、`\` 归一为 `/`；
/// 相对路径不含 `..` 组件，无需绝对化）。
fn rel_total_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    fn normalize_rel_text(text: &str) -> String {
        text.to_lowercase().replace('\\', "/")
    }
    natural_cmp(a, b).then_with(|| normalize_rel_text(a).cmp(&normalize_rel_text(b)))
}

/// 相对路径自然排序（M-03 第二排序键）：数字段按数值比较（前导零不参与），
/// 非数字段按字节逐字符比较（UTF-8 字节序与 Unicode 码点序一致）。
/// 数值相等但写法不同的路径（`1.md` / `01.md`）在本层返回 Equal；
/// 严格全序由 `rel_total_cmp` 追加归一化文本决胜保证，排序入口一律走后者。
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let mut ai = a.as_bytes();
    let mut bi = b.as_bytes();
    loop {
        match (ai.first(), bi.first()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(&x), Some(&y)) => {
                if x.is_ascii_digit() && y.is_ascii_digit() {
                    let (an, arest) = split_digits(ai);
                    let (bn, brest) = split_digits(bi);
                    let at = strip_leading_zeros(an);
                    let bt = strip_leading_zeros(bn);
                    match at.len().cmp(&bt.len()) {
                        Ordering::Equal => match at.cmp(bt) {
                            Ordering::Equal => {
                                ai = arest;
                                bi = brest;
                            }
                            other => return other,
                        },
                        other => return other,
                    }
                } else if x.is_ascii_digit() {
                    // 数字段排在非数字段之前（按首字节序，数字字符都小于大多数字母，
                    // 与字节序保持同一口径即可，此处仅保证全序确定性）。
                    return x.cmp(&y).then(Ordering::Less);
                } else if y.is_ascii_digit() {
                    return x.cmp(&y).then(Ordering::Greater);
                } else {
                    // M-03 按「段序列」比较：非数字段 a 是 a! 的前缀时，
                    // 必须先以短段决胜，不能拿下一数字段的 2 与 ! 混比。
                    let aend = ai.iter().position(u8::is_ascii_digit).unwrap_or(ai.len());
                    let bend = bi.iter().position(u8::is_ascii_digit).unwrap_or(bi.len());
                    match ai[..aend].cmp(&bi[..bend]) {
                        Ordering::Equal => {
                            ai = &ai[aend..];
                            bi = &bi[bend..];
                        }
                        other => return other,
                    }
                }
            }
        }
    }
}

fn split_digits(bytes: &[u8]) -> (&[u8], &[u8]) {
    let end = bytes
        .iter()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(bytes.len());
    bytes.split_at(end)
}

fn strip_leading_zeros(bytes: &[u8]) -> &[u8] {
    let start = bytes.iter().position(|b| *b != b'0').unwrap_or(bytes.len());
    &bytes[start..]
}
