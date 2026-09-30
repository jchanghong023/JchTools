//! 转 Markdown 的只读扫描、任务编排与结果落盘（T-07～T-12、T-22～T-25）。

use std::{
    cmp::Ordering,
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering as AtomicOrdering},
    time::Duration,
};

#[cfg(test)]
use std::{
    io::{self, BufRead, BufReader},
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::Instant,
};

use crate::{fsutil, markdown_assets, markdown_document};

const PDF: &[&str] = &["pdf"];
const OFFICE: &[&str] = &[
    "doc", "docx", "docm", "dot", "dotx", "dotm", "ppt", "pptx", "pptm", "pps", "ppsx", "pot",
    "potx", "potm", "xls", "xlsx", "xlsm", "xlsb", "xlt", "xltx", "xltm", "xla", "xlam", "odt",
    "ods", "odp",
];
const IMAGES: &[&str] = &[
    "png", "jpg", "jpeg", "webp", "bmp", "gif", "tif", "tiff", "jp2", "j2k", "j2c", "jpx", "jpm",
    "mj2", "jbig2", "jb2", "pnm", "pbm", "pgm", "ppm",
];
const MEDIA: &[&str] = &["mp4", "m4a"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatGroup {
    Pdf,
    Office,
    Images,
    Media,
    Other,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub input_dir: PathBuf,
    pub output_dir: PathBuf,
    pub flat: bool,
    pub groups: Vec<FormatGroup>,
    pub timeout_secs: u64,
}

#[derive(Clone, Debug)]
pub enum Event {
    Started {
        total: usize,
    },
    FileStarted {
        relative: PathBuf,
        index: usize,
        total: usize,
    },
    FileFinished {
        relative: PathBuf,
        success: bool,
        partial: bool,
        message: String,
    },
    Log(String),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Summary {
    pub success: usize,
    pub partial: usize,
    pub failed: usize,
    pub skipped_existing: usize,
    pub skipped_duplicate: usize,
    pub stopped: bool,
}

#[derive(Clone, Debug)]
struct Item {
    source: PathBuf,
    relative: PathBuf,
    target: PathBuf,
    is_media: bool,
}

#[derive(Clone, Debug, Default)]
struct Plan {
    output_root: PathBuf,
    items: Vec<Item>,
    summary: Summary,
}

pub fn readiness() -> Result<(), String> {
    platform_preflight()?;
    markdown_assets::readiness()
}

pub fn initialize(cancel: &AtomicBool, progress: impl FnMut(String)) -> Result<(), String> {
    platform_preflight()?;
    markdown_assets::initialize(cancel, progress)
}

pub fn run(
    options: &Options,
    cancel: &AtomicBool,
    mut events: impl FnMut(Event),
) -> Result<Summary, String> {
    if options.timeout_secs == 0 {
        return Err("单文件超时必须为正整秒".to_string());
    }
    if options.groups.is_empty() {
        return Err("至少选择一组文件类型".to_string());
    }
    readiness()?;
    let runtime_dir = markdown_assets::runtime_dir()?;
    let supported = supported_formats(&options.groups)?;
    let plan = scan(options, &supported)?;
    let total = plan.items.len() + plan.summary.skipped_existing + plan.summary.skipped_duplicate;
    events(Event::Started { total });
    let mut summary = plan.summary;
    let output_root = plan.output_root;
    // 文档和媒体逐文件提交到会话共享引擎；批次不拥有引擎生命周期。
    for (index, item) in plan.items.iter().enumerate() {
        if cancel.load(AtomicOrdering::Relaxed) {
            events(Event::Log("已停止：当前文件之外不再开始转换".to_string()));
            break;
        }
        events(Event::FileStarted {
            relative: item.relative.clone(),
            index: index + 1,
            total,
        });
        // F21/T-29：单文件预算从进入该文件起算，页数预检与转换共用同一 deadline。
        let deadline = markdown_document::Deadline::new(Duration::from_secs(options.timeout_secs));
        let outcome = if item.is_media {
            convert_media(&item.source, &deadline).map(|markdown| {
                markdown_document::DocumentOutput {
                    markdown,
                    warnings: Vec::new(),
                }
            })
        } else {
            let pages = markdown_document::page_count(&item.source, &deadline);
            let fast = pages.is_some_and(|count| count > 200);
            if fast {
                events(Event::Log(format!(
                    "{}：{} 页，快速模式关闭版面识别与图片 OCR",
                    item.relative.display(),
                    pages.unwrap_or_default()
                )));
            }
            markdown_document::convert(&item.source, &runtime_dir, fast, &deadline)
        };
        let outcome = outcome.and_then(|document| {
            write_new_markdown(&output_root, &item.target, &document.markdown)?;
            Ok(document.warnings)
        });
        match outcome {
            Ok(warnings) => {
                let partial = !warnings.is_empty();
                if partial {
                    summary.partial += 1;
                } else {
                    summary.success += 1;
                }
                events(Event::FileFinished {
                    relative: item.relative.clone(),
                    success: !partial,
                    partial,
                    message: if partial {
                        format!("部分内容未提取：{}", warnings.join("；"))
                    } else {
                        "转换成功".to_string()
                    },
                });
            }
            Err(message) => {
                summary.failed += 1;
                events(Event::FileFinished {
                    relative: item.relative.clone(),
                    success: false,
                    partial: false,
                    message,
                });
            }
        }
    }
    // T-22/T-23：批结束或用户停止后不再发送请求，保留共享引擎及已加载模型。
    summary.stopped = cancel.load(AtomicOrdering::Relaxed);
    Ok(summary)
}

#[cfg(windows)]
fn platform_preflight() -> Result<(), String> {
    use windows_sys::Win32::System::SystemInformation::{OSVERSIONINFOEXW, OSVERSIONINFOW};

    #[link(name = "ntdll")]
    extern "system" {
        #[link_name = "RtlGetVersion"]
        fn rtl_get_version(info: *mut OSVERSIONINFOW) -> i32;
    }

    // SAFETY: OSVERSIONINFOEXW 是纯 C 结构；调用时指针有效，API 按声明的大小写入。
    let mut version: OSVERSIONINFOEXW = unsafe { std::mem::zeroed() };
    version.dwOSVersionInfoSize = u32::try_from(std::mem::size_of::<OSVERSIONINFOEXW>())
        .map_err(|_| "Windows 版本结构大小超出范围".to_string())?;
    // SAFETY: OSVERSIONINFOEXW 的起始布局与 API 所需的 OSVERSIONINFOW 相同。
    let status = unsafe { rtl_get_version((&raw mut version).cast::<OSVERSIONINFOW>()) };
    if status < 0 {
        return Err(format!("无法确认 Windows 版本（NTSTATUS {status:#x}）"));
    }
    if version.dwMajorVersion != 10 || version.dwBuildNumber < 22_000 || version.wProductType != 1 {
        return Err("转 Markdown 仅支持 Windows 11 x64".to_string());
    }
    if !std::is_x86_feature_detected!("avx2") {
        return Err("转 Markdown 需要支持 AVX2 的处理器".to_string());
    }
    Ok(())
}

#[cfg(not(windows))]
fn platform_preflight() -> Result<(), String> {
    Err("转 Markdown 仅支持 Windows 11 x64".to_string())
}

/// 结果名占用索引（T-11/F29）：按大小写折叠键分桶，桶内候选再用
/// [`compare_names`]（Windows `CompareStringOrdinal` 忽略大小写）精确确认。
/// 平铺模式登记全局结果名，层级模式登记「相对父目录+结果名」（见 `scan`）。
/// 折叠只用于缩小候选集：语义上过桶只会多比、不会漏比，occupied 查询近似 O(1)。
#[derive(Default)]
struct OccupiedIndex {
    buckets: std::collections::HashMap<Vec<u16>, Vec<OsString>>,
}

impl OccupiedIndex {
    fn contains(&self, name: &OsStr) -> bool {
        self.contains_with(name, &mut |left, right| {
            compare_names(left, right) == Ordering::Equal
        })
    }

    /// 可注入比较谓词的查询（测试用于断言比较次数增长阶）。
    fn contains_with(
        &self,
        name: &OsStr,
        is_equal: &mut dyn FnMut(&OsStr, &OsStr) -> bool,
    ) -> bool {
        self.buckets
            .get(&case_fold_key(name))
            .is_some_and(|bucket| bucket.iter().any(|existing| is_equal(existing, name)))
    }

    fn insert(&mut self, name: OsString) {
        self.buckets
            .entry(case_fold_key(&name))
            .or_default()
            .push(name);
    }
}

/// 折叠键：逐 UTF-16 单元做简单大写折叠。ASCII 走快路径；非 ASCII 采用
/// Unicode 单单元大写映射，多单元或无映射（含代理项）保留原单元。折叠与
/// `CompareStringOrdinal` 的逐单元大写口径一致，不一致的极端情形只会把
/// 候选落进不同桶后再精确比较（多比不漏比）。
fn case_fold_key(name: &OsStr) -> Vec<u16> {
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
fn occupancy_key(flat: bool, relative_parent: &Path, file_name: &OsStr) -> OsString {
    if flat {
        file_name.to_os_string()
    } else {
        relative_parent.join(file_name).into_os_string()
    }
}

fn scan(options: &Options, supported: &BTreeSet<String>) -> Result<Plan, String> {
    let input = checked_directory(&options.input_dir)?;
    let output = checked_directory(&options.output_dir)?;
    if path_equal(&input, &output) {
        return Err("输入与输出目录不能相同".to_string());
    }
    let mut paths = Vec::new();
    // T-09 只授权「输出位于输入目录子树内」时排除整棵输出子树；输出是输入的
    // 祖先或两者在不同分支时不排除任何目录——否则输入子树会因前缀判定被整体
    // 静默跳过，违反 T-07 的递归处理要求。
    let output_in_input = path_begins_with(&output, &input);
    let mut pending = vec![input.clone()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).map_err(|e| format!("无法扫描 {}：{e}", dir.display()))?
        {
            let entry = entry.map_err(|e| format!("无法读取 {} 的目录项：{e}", dir.display()))?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|e| format!("无法检查 {}：{e}", path.display()))?;
            if fsutil::is_link(&metadata) {
                continue;
            }
            if metadata.is_dir() {
                if !(output_in_input && path_begins_with(&path, &output)) {
                    pending.push(path);
                }
            } else if metadata.is_file()
                && path
                    .extension()
                    .and_then(OsStr::to_str)
                    .is_some_and(|ext| supported.contains(&ext.to_ascii_lowercase()))
            {
                paths.push(path);
            }
        }
    }
    paths.sort_by(|a, b| {
        compare_paths(
            a.strip_prefix(&input).unwrap_or(a),
            b.strip_prefix(&input).unwrap_or(b),
        )
    });
    let mut occupied = OccupiedIndex::default();
    let mut plan = Plan {
        output_root: output.clone(),
        ..Plan::default()
    };
    for source in paths {
        let relative = source
            .strip_prefix(&input)
            .map_err(|e| format!("输入路径超出根目录：{e}"))?
            .to_path_buf();
        let file_name = markdown_name(&source)?;
        // 冲突决胜与平铺一致：排序靠前者保留，后续计 skipped_duplicate（T-24：
        // 与转换失败可区分）。
        let occupancy = occupancy_key(
            options.flat,
            relative.parent().unwrap_or_else(|| Path::new("")),
            &file_name,
        );
        if occupied.contains(occupancy.as_os_str()) {
            plan.summary.skipped_duplicate += 1;
            continue;
        }
        occupied.insert(occupancy);
        let target = if options.flat {
            output.join(file_name)
        } else {
            output
                .join(relative.parent().unwrap_or_else(|| Path::new("")))
                .join(file_name)
        };
        if target.exists() {
            plan.summary.skipped_existing += 1;
            continue;
        }
        let is_media = source
            .extension()
            .and_then(OsStr::to_str)
            .is_some_and(|ext| MEDIA.contains(&ext.to_ascii_lowercase().as_str()));
        plan.items.push(Item {
            source,
            relative,
            target,
            is_media,
        });
    }
    Ok(plan)
}

/// 钉住发布物的格式清单：与 `markdown_assets::XBERG_TAG` 同一钉版纪律（XB-09
/// 版本锚是发布 tag 与成员摘要，不是运行期探测）。真实发布物 run49.1 的 worker
/// 协议只提供 `extract` / `ocr_snapshot` / `snapshot_state` / `transcribe`
/// （`formats` / `capabilities` / `cancel` 等共享协议扩展「已实施、尚未发布验收」，
/// 见 Xberg 仓 docs/requirements/WORKER.md），且 XB-14 禁止为查询另起 Xberg
/// 进程，因此清单随钉住版本内置于资源，升级引擎 tag 时必须同步再生成
/// （`xberg.exe formats --format json`，只读诊断）。
fn supported_formats(groups: &[FormatGroup]) -> Result<BTreeSet<String>, String> {
    let table: serde_json::Value =
        serde_json::from_str(include_str!("../resources/markdown-xberg-formats.json"))
            .map_err(|e| format!("内置 Xberg 格式清单无效：{e}"))?;
    if table.get("tag").and_then(serde_json::Value::as_str) != Some(markdown_assets::XBERG_TAG) {
        return Err(
            "内置 Xberg 格式清单与固定版本不一致；请同步再生成 markdown-xberg-formats.json"
                .to_string(),
        );
    }
    let rows = table
        .get("formats")
        .ok_or("内置 Xberg 格式清单缺少 formats 字段")?;
    let bytes = serde_json::to_vec(rows).map_err(|e| e.to_string())?;
    let mut selected = parse_formats(&bytes, groups)?;
    if groups.contains(&FormatGroup::Media) {
        selected.extend(MEDIA.iter().map(|extension| (*extension).to_string()));
    }
    Ok(selected)
}

/// 有界且可取消地运行格式清单探测（F21）：超时或取消后 kill + 限时收尾，
/// 输出被截断时如实报错，不得把半截清单当完整结果。
#[cfg(test)]
fn run_formats_probe(
    command: &mut Command,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<crate::process::CapturedOutput, String> {
    let result = crate::process::run_with_timeout_cancel(command, timeout, cancel);
    let output = match result {
        Ok(output) => output,
        Err(error) => {
            // 先判取消：停止不是错误（T-23），由调用方转为已停止汇总。
            if cancel.load(AtomicOrdering::Relaxed) {
                return Err("格式探测已取消".to_string());
            }
            return Err(format!("无法读取 Xberg 格式清单：{error:#}"));
        }
    };
    if !output.status.success() {
        return Err(format!(
            "Xberg 格式清单失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    if output.stdout_truncated {
        return Err("Xberg 格式清单输出被截断（超过捕获上限），不能当完整清单".to_string());
    }
    Ok(output)
}

/// 解析 Xberg formats JSON 并按分组筛入支持集（纯函数，便于回归）。
fn parse_formats(stdout: &[u8], groups: &[FormatGroup]) -> Result<BTreeSet<String>, String> {
    let rows: Vec<serde_json::Value> =
        serde_json::from_slice(stdout).map_err(|e| format!("Xberg 格式清单无效：{e}"))?;
    let mut selected = BTreeSet::new();
    for row in rows {
        let Some(extension) = row.get("extension").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let extension = extension.trim_start_matches('.').to_ascii_lowercase();
        if extension.is_empty() || extension.contains('.') {
            continue;
        }
        let mime = row
            .get("mime_type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if selected_xberg_extension(&extension, mime, groups) {
            selected.insert(extension);
        }
    }
    Ok(selected)
}

fn selected_xberg_extension(extension: &str, mime: &str, groups: &[FormatGroup]) -> bool {
    if (mime.starts_with("audio/") || mime.starts_with("video/")) && !MEDIA.contains(&extension) {
        return false;
    }
    groups.contains(&classify_format(extension, mime))
}

fn classify_format(extension: &str, mime: &str) -> FormatGroup {
    if PDF.contains(&extension) {
        FormatGroup::Pdf
    } else if OFFICE.contains(&extension)
        || mime.contains("wordprocessing")
        || mime.contains("spreadsheet")
        || mime.contains("presentation")
        || mime.contains("opendocument")
    {
        FormatGroup::Office
    } else if MEDIA.contains(&extension) {
        FormatGroup::Media
    } else if IMAGES.contains(&extension) || mime.starts_with("image/") {
        FormatGroup::Images
    } else {
        FormatGroup::Other
    }
}

fn checked_directory(path: &Path) -> Result<PathBuf, String> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) => current.push(component.as_os_str()),
            Component::RootDir | Component::Normal(_) => {
                current.push(component.as_os_str());
                let metadata = fs::symlink_metadata(&current)
                    .map_err(|e| format!("无法检查目录 {}：{e}", current.display()))?;
                if fsutil::is_link(&metadata) {
                    return Err(format!(
                        "目录路径含符号链接或 junction：{}",
                        current.display()
                    ));
                }
            }
            Component::CurDir => (),
            Component::ParentDir => return Err("请选择不含上级跳转的目录路径".to_string()),
        }
    }
    let canonical =
        fs::canonicalize(path).map_err(|e| format!("无法访问目录 {}：{e}", path.display()))?;
    if !canonical.is_dir() {
        return Err(format!("不是目录：{}", path.display()));
    }
    Ok(canonical)
}

fn markdown_name(source: &Path) -> Result<OsString, String> {
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

fn path_equal(left: &Path, right: &Path) -> bool {
    compare_paths(left, right) == Ordering::Equal
}

fn path_begins_with(path: &Path, prefix: &Path) -> bool {
    let mut path_parts = path.components();
    for prefix_part in prefix.components() {
        let Some(path_part) = path_parts.next() else {
            return false;
        };
        if compare_names(path_part.as_os_str(), prefix_part.as_os_str()) != Ordering::Equal {
            return false;
        }
    }
    true
}

fn compare_paths(left: &Path, right: &Path) -> Ordering {
    let case_insensitive = compare_paths_insensitive(left, right);
    if case_insensitive != Ordering::Equal {
        return case_insensitive;
    }
    compare_paths_original(left, right)
}

fn compare_paths_insensitive(left: &Path, right: &Path) -> Ordering {
    compare_component_wise(left, right, compare_names)
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
fn compare_names(left: &OsStr, right: &OsStr) -> Ordering {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Globalization::CompareStringOrdinal;

    let a: Vec<_> = left.encode_wide().collect();
    let b: Vec<_> = right.encode_wide().collect();
    let Ok(a_len) = i32::try_from(a.len()) else {
        return a.cmp(&b);
    };
    let Ok(b_len) = i32::try_from(b.len()) else {
        return a.cmp(&b);
    };
    // SAFETY: 两个 UTF-16 缓冲区在整个同步调用期间有效，长度与缓冲区一致。
    match unsafe { CompareStringOrdinal(a.as_ptr(), a_len, b.as_ptr(), b_len, 1) } {
        1 => Ordering::Less,
        2 => Ordering::Equal,
        3 => Ordering::Greater,
        _ => a.cmp(&b),
    }
}

#[cfg(not(windows))]
fn compare_names(left: &OsStr, right: &OsStr) -> Ordering {
    left.to_string_lossy()
        .to_lowercase()
        .cmp(&right.to_string_lossy().to_lowercase())
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

fn write_new_markdown(output_root: &Path, target: &Path, content: &str) -> Result<(), String> {
    let parent = target.parent().ok_or_else(|| "结果目录无效".to_string())?;
    let relative_parent = parent
        .strip_prefix(output_root)
        .map_err(|e| format!("结果不在输出目录内：{e}"))?;
    let mut current = output_root.to_path_buf();
    for component in relative_parent.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err("输出路径含非法目录段".to_string());
        }
        current.push(component.as_os_str());
        match fs::create_dir(&current) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(format!("无法创建输出目录 {}：{error}", current.display())),
        }
        let metadata = fs::symlink_metadata(&current)
            .map_err(|error| format!("无法检查输出目录 {}：{error}", current.display()))?;
        if fsutil::is_link(&metadata) || !metadata.is_dir() {
            return Err(format!("输出路径不是普通目录：{}", current.display()));
        }
    }
    let temp = parent.join(format!(".jch-markdown-{}.tmp", uuid::Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|e| format!("无法创建临时结果：{e}"))?;
    let write_result = (|| -> Result<(), String> {
        file.write_all(content.as_bytes())
            .map_err(|e| format!("写入 Markdown 失败：{e}"))?;
        file.sync_all()
            .map_err(|e| format!("同步 Markdown 失败：{e}"))?;
        drop(file);
        // T-25/T-12：完整成功后以不覆盖改名提交（temp 与目标同目录同卷）。
        // 不用 fs::rename——Windows 上它会替换已存在目标；不用硬链接——
        // exFAT/FAT 等文件系统不支持，会把输出在这些卷上的结果全部判失败。
        fsutil::rename_noreplace(&temp, target)
            .map_err(|e| format!("无法提交新结果（已有结果不会覆盖）：{e:#}"))?;
        Ok(())
    })();
    let _ = fs::remove_file(&temp);
    write_result
}

/// 批内常驻 Xberg 转录工作进程的一次请求失败分类（T-24 要求可区分）。
#[derive(Debug)]
#[cfg(test)]
enum TranscribeFailure {
    /// 单文件失败（如解码失败、无响应载荷）：进程存活，批次继续复用同一进程。
    PerFile(String),
    /// 进程意外退出或协议破坏：终结整组进程并丢弃，下一文件重启新进程。
    WorkerLost(String),
    /// 单文件预算耗尽（T-29）：终结整组进程，该文件记失败。
    Timeout(String),
}

/// 优雅关闭的有界等待：worker 收到 EOF 后应自行退出；超限按挂死终结。
#[cfg(test)]
const WORKER_EXIT_GRACE: Duration = Duration::from_secs(10);

/// 增量有界行读取（B'-6/A'-1 加固，读线程不再裸用 `read_line`）：逐块读直到
/// `\n`，只在未超限前把字节累积进返回值——失控子进程输出超长单行时不再把整行
/// 全额分配进内存（旧实现先 `read_line` 读完整行才判 `> MAX_CAPTURE_BYTES`）。
///
/// 行含结尾 `\n` 时原样返回（与 `read_line` 一致，空行与 EOF 可区分）；EOF
/// 返回已读残余（可能为空）。行字节数（含 `\n`）超过 `cap` 时返回
/// `InvalidData` 错误，文案沿用旧口径「单次响应超过捕获上限（N 字节）」，N 为
/// 整行实际字节数；报错前把该行剩余字节排空到 `\n`/EOF（只计数丢弃、不再
/// 分配），保证返回后读位置仍停在行边界、后续行照常解析（协议行边界对齐，
/// 进程与批次继续，语义与旧实现一致）。
#[cfg(test)]
fn read_line_capped(reader: &mut impl BufRead, cap: usize) -> io::Result<Vec<u8>> {
    let mut total = 0usize; // 已观测行字节数（含结尾 \n 口径）
    let mut line: Vec<u8> = Vec::new(); // 只在未超限前累积，内存上界即 cap
    let mut oversize = false;
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(ref error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if available.is_empty() {
            // EOF：超限行到 EOF 仍未见到 \n 时 N 即整行精确长度（无结尾 \n）。
            return if oversize {
                Err(oversize_line_error(total))
            } else {
                Ok(line)
            };
        }
        let newline = available.iter().position(|&byte| byte == b'\n');
        let usable = newline.map_or(available.len(), |index| index + 1);
        if !oversize && total + usable > cap {
            oversize = true;
            line = Vec::new(); // 立即释放已累积字节，后续只计数不分配
        }
        total += usable;
        if oversize {
            // 排空剩余字节凑齐整行精确长度（N），到行边界才报错。
            reader.consume(usable);
            if newline.is_some() {
                return Err(oversize_line_error(total));
            }
            continue;
        }
        line.extend_from_slice(&available[..usable]);
        reader.consume(usable);
        if newline.is_some() {
            return Ok(line);
        }
    }
}

/// 超限错误（B'-6）：`InvalidData` 便于读线程与一般读错误区分；文案沿用旧口径。
#[cfg(test)]
fn oversize_line_error(total: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("单次响应超过捕获上限（{total} 字节）"),
    )
}

/// 批内常驻的 Xberg 转录工作进程（T-19/T-22，Xberg WORKER.md 协议客户端）。
///
/// 一个批次只付一次冷启动，文件之间进程内复用已加载的 SenseVoice 会话；调用方
/// 严格串行地发送 `transcribe` 请求并逐请求等待恰好一行响应。进程组（含 FFmpeg
/// 等子进程）由 [`MediaProcessJob`] 兜底：超时（T-29）或意外退出后终结整组，
/// 批结束（含用户停止，T-23）时关 stdin 优雅关闭。
#[cfg(test)]
struct MediaWorker {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    #[cfg(windows)]
    job: Option<MediaProcessJob>,
    responses: Receiver<String>,
    reader: Option<thread::JoinHandle<()>>,
    stderr_reader: Option<thread::JoinHandle<crate::process::ReadCapture>>,
    /// 当前在途请求 id（读线程用于超限响应的合成错误回显）。
    pending_id: std::sync::Arc<std::sync::atomic::AtomicU64>,
    next_id: u64,
}

#[cfg(test)]
impl MediaWorker {
    /// 旧协议回归夹具的进程接缝，仅编译进测试，不用于产品路径。
    fn spawn(command: &mut Command) -> Result<MediaWorker, String> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // 0x0800_0000 = CREATE_NO_WINDOW：CUI 子进程不新建控制台黑窗
            // （与 worker 侧 xberg spawn 同法）；子进程由 MediaProcessJob 单独收口。
            command.creation_flags(0x0800_0000);
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("无法启动媒体转录进程：{e}"))?;
        #[cfg(windows)]
        let job = match MediaProcessJob::attach(&child) {
            Ok(job) => job,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "无法写入转录请求".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "无法读取转录结果".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "无法读取转录诊断".to_string())?;
        // stdout 按行解析响应：后台线程经 [`read_line_capped`] 增量有界读取并
        // 持续排空，防止子进程写管道阻塞。上限按「单行（= 单次响应）」计：常驻
        // 进程的批次累计流量会随文件数自然超过任何总量上限，误杀健康进程；单个
        // 响应超限按该文件失败处理（合成错误行回显当前请求 id），超长行的剩余
        // 字节在函数内排空到行边界、不整行分配内存，进程与批次继续。读线程经
        // 共享原子获知当前请求 id（严格串行协议下响应只属于最新请求）。
        let (sender, responses) = mpsc::channel::<String>();
        let pending_id = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let reader_pending = std::sync::Arc::clone(&pending_id);
        let reader = thread::spawn(move || {
            let mut lines = BufReader::new(stdout);
            loop {
                match read_line_capped(&mut lines, crate::process::MAX_CAPTURE_BYTES) {
                    // 空行与 EOF 的区分依赖保留结尾换行的行口径。
                    Ok(line) if line.is_empty() => break,
                    Ok(line) => match String::from_utf8(line) {
                        Ok(text) => {
                            if sender.send(text).is_err() {
                                break;
                            }
                        }
                        // 与 read_line 的 UTF-8 校验口径一致：非法字节终止读线程。
                        Err(_) => break,
                    },
                    Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                        // 超限：合成错误行回显当前请求 id（该文件失败；行边界
                        // 已在 read_line_capped 内排空对齐，进程与批次继续）。
                        let id = reader_pending.load(std::sync::atomic::Ordering::Acquire);
                        let oversize =
                            format!("{{\"id\":{id},\"ok\":false,\"error\":\"{error}\"}}");
                        if sender.send(oversize).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        // stderr 同样必须持续排空（诊断日志写满管道会卡死 worker）。
        let stderr_reader = thread::spawn(move || {
            crate::process::read_all_capped(stderr, crate::process::MAX_CAPTURE_BYTES)
        });
        Ok(Self {
            child,
            stdin: Some(stdin),
            #[cfg(windows)]
            job: Some(job),
            responses,
            reader: Some(reader),
            stderr_reader: Some(stderr_reader),
            pending_id,
            next_id: 1,
        })
    }

    /// 串行发送一个 transcribe 请求并等待恰好一行响应（严格串行，T-22）。
    fn transcribe(
        &mut self,
        path: &Path,
        deadline: &markdown_document::Deadline,
    ) -> Result<String, TranscribeFailure> {
        let id = self.next_id;
        self.next_id += 1;
        self.pending_id
            .store(id, std::sync::atomic::Ordering::Release);
        // T-21：只向本地子进程传本地媒体文件路径，不落盘、不联网。
        let request = serde_json::json!({
            "id": id,
            "command": "transcribe",
            "path": path.to_string_lossy(),
        });
        let sent = match self.stdin.as_mut() {
            Some(stdin) => stdin
                .write_all(request.to_string().as_bytes())
                .and_then(|()| stdin.write_all(b"\n"))
                .and_then(|()| stdin.flush()),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "stdin 已关闭",
            )),
        };
        if let Err(error) = sent {
            return Err(TranscribeFailure::WorkerLost(format!(
                "无法向媒体转录进程发送请求：{error}"
            )));
        }
        let line = self.wait_response(id, deadline)?;
        Self::parse_response(id, &line)
    }

    /// 等待本请求的响应行；预算耗尽按超时（T-29），进程退出按意外丢失。
    fn wait_response(
        &mut self,
        id: u64,
        deadline: &markdown_document::Deadline,
    ) -> Result<String, TranscribeFailure> {
        loop {
            let remaining = deadline.remaining();
            if remaining.is_zero() {
                return Err(TranscribeFailure::Timeout(format!(
                    "媒体转换超时（{} 秒）",
                    deadline.total().as_secs()
                )));
            }
            match self.responses.recv_timeout(remaining) {
                Ok(line) => {
                    let echoed = serde_json::from_str::<serde_json::Value>(&line)
                        .ok()
                        .and_then(|value| value.get("id").cloned())
                        .is_some_and(|echoed| echoed == id);
                    if !echoed {
                        return Err(TranscribeFailure::WorkerLost(format!(
                            "媒体转录响应与请求不匹配：{}",
                            line.trim()
                        )));
                    }
                    return Ok(line);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(TranscribeFailure::WorkerLost(self.lost_message()));
                }
            }
        }
    }

    /// 解析响应行：成功取 markdown 字段透传（T-20 结构由 Xberg 生成，
    /// has_audio=false 的说明性 markdown 同样透传）；失败按类别上抛。
    fn parse_response(id: u64, line: &str) -> Result<String, TranscribeFailure> {
        let value: serde_json::Value = serde_json::from_str(line).map_err(|error| {
            TranscribeFailure::WorkerLost(format!("媒体转录响应格式错误：{error}"))
        })?;
        if !value
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            let message = value
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("未知错误");
            return Err(TranscribeFailure::PerFile(format!(
                "媒体转换失败：{message}"
            )));
        }
        value
            .get("markdown")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                TranscribeFailure::WorkerLost(format!("媒体转录响应缺少 Markdown（id {id}）"))
            })
    }

    /// 进程意外丢失时的诊断消息：携带退出状态与 stderr 尾部（不回传正文）。
    fn lost_message(&mut self) -> String {
        let status = self.child.try_wait().ok().flatten();
        let mut message = match status {
            Some(status) => format!("媒体转录进程意外退出（{status}）"),
            None => "媒体转录进程意外退出".to_string(),
        };
        if let Some(handle) = self.stderr_reader.take() {
            if let Some(captured) =
                crate::process::join_with_deadline(handle, crate::process::PIPE_DRAIN_GRACE)
            {
                let tail = String::from_utf8_lossy(&captured.data);
                if let Some(suffix) = stderr_tail_for_message(&tail) {
                    message.push_str(&suffix);
                }
            }
        }
        message
    }

    /// 立即终结整组进程并回收读线程（超时/协议破坏后调用，T-24/T-29）。
    fn terminate(&mut self) {
        self.stdin.take();
        #[cfg(windows)]
        if let Some(job) = &self.job {
            job.terminate();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.reap_readers();
    }

    /// 读线程限时收尾：孙进程持管道写端时超限放弃，不永久阻塞宿主（F21）。
    fn reap_readers(&mut self) {
        if let Some(handle) = self.reader.take() {
            let _ = crate::process::join_with_deadline(handle, crate::process::PIPE_DRAIN_GRACE);
        }
        if let Some(handle) = self.stderr_reader.take() {
            let _ = crate::process::join_with_deadline(handle, crate::process::PIPE_DRAIN_GRACE);
        }
    }
}

/// 进程意外丢失时 stderr 的诊断后缀（T-24）：空尾部返回 `None`；非空时取尾部
/// 最多 300 个**字符**（多字节 UTF-8 不得按字节切片，否则起点落在字符内部时
/// panic 杀死转换线程，A2），发生截断时以「：…」衔接并从尾部首个空白之后
/// 取起，未截断时仅以「：」衔接。
#[cfg(test)]
fn stderr_tail_for_message(stderr_text: &str) -> Option<String> {
    let tail = stderr_text.trim();
    if tail.is_empty() {
        return None;
    }
    const TAIL_CHARS: usize = 300;
    let total = tail.chars().count();
    if total <= TAIL_CHARS {
        return Some(format!("：{}", tail.trim_start()));
    }
    let mut suffix: String = tail.chars().skip(total - TAIL_CHARS).collect();
    // 从首个空白之后取起，避免把截断的半个词当开头。
    if let Some(position) = suffix.find(char::is_whitespace) {
        suffix.drain(..position);
    }
    Some(format!("：…{}", suffix.trim_start()))
}

/// 组装媒体转录进程的环境变量（T-21）：组件目录内模型与运行库指针 + 与文档
/// 转换路径（[`markdown_document::apply_offline_environment`]）同口径的纯开关型
/// 离线变量（XB-04：两侧同为 xberg.exe worker 子命令，离线口径必须一致，防止
/// worker 内部 HF hub 回退联网）。不设 HF_HOME/HF_HUB_CACHE 等路径变量——媒体
/// 组件目录结构与文档转换组件不同，不引入额外路径假设。唯一例外是
/// `XBERG_PERF_LOG_DIR`（B'-8）：把组件 perf-tracing feature 的日志目录固定到
/// 系统临时目录（与媒体转录临时文件同口径），封死其向当前工作目录创建
/// `logs/perf.log.*` 的唯一主动写文件路径；该变量不是离线开关、不影响 XB-04
/// 口径，且仅当组件编入 perf feature 才生效，未编入时被无害忽略。
#[cfg(test)]
fn media_worker_environment(root: &Path) -> Vec<(String, String)> {
    let path_value = |sub: &str| root.join(sub).to_string_lossy().into_owned();
    [
        ("XBERG_SENSEVOICE_MODEL_DIR", path_value("models")),
        ("XBERG_SHERPA_DLL_DIR", path_value("sherpa-onnx")),
        ("XBERG_FFMPEG_DLL_DIR", path_value("ffmpeg")),
        (
            "XBERG_PERF_LOG_DIR",
            std::env::temp_dir()
                .join("JchTools-xberg-perf")
                .to_string_lossy()
                .into_owned(),
        ),
        ("HF_HUB_OFFLINE", "1".to_string()),
        ("HUGGINGFACE_HUB_OFFLINE", "1".to_string()),
        ("TRANSFORMERS_OFFLINE", "1".to_string()),
        ("HF_DATASETS_OFFLINE", "1".to_string()),
        ("NO_COLOR", "1".to_string()),
        ("XBERG_ORT_EP", "cpu".to_string()),
        ("XBERG_MAX_CONCURRENT_REQUESTS", "1".to_string()),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_string(), value))
    .collect()
}

#[cfg(test)]
impl Drop for MediaWorker {
    fn drop(&mut self) {
        // 批结束（含用户停止，T-23）的有界优雅关闭：关 stdin 触发 worker 在
        // EOF 后自行退出；超限按挂死终结整组进程（KILL_ON_JOB_CLOSE 再兜底）。
        self.stdin.take();
        let grace_end = Instant::now() + WORKER_EXIT_GRACE;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) if Instant::now() >= grace_end => break,
                Ok(None) => thread::sleep(Duration::from_millis(25)),
            }
        }
        #[cfg(windows)]
        if let Some(job) = &self.job {
            job.terminate();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.reap_readers();
    }
}

/// 组件解析类错误的统一包装（B'-7）：媒体转录组件未就绪的各类错误（未配置、
/// 不完整、版本与清单不一致、多版本无法确定——均产生自组件目录解析与在位校验）
/// 一律补「重新初始化」指引，口径一致；单文件转换本身的失败（格式不支持、解码
/// 失败等）产生自转录链路、不经本包装，不会被误加指引。
#[cfg(test)]
fn media_component_error(error: &str) -> String {
    // 部分解析错误（版本不一致等）自带重新初始化指引，避免双重指引。
    if error.contains("重新初始化") {
        return error.to_string();
    }
    format!("媒体转录组件未就绪，请重新初始化转 Markdown 功能：{error}")
}

/// 转换一个媒体文件：验证媒体资产后，经会话共享进程请求 transcribe。
fn convert_media(path: &Path, deadline: &markdown_document::Deadline) -> Result<String, String> {
    let component = markdown_assets::media_component_dir().map_err(|error| {
        format!("共享 Xberg 的媒体组件未就绪，请检查已保存目录的模型和运行库：{error}")
    })?;
    markdown_assets::validate_media()?;
    let response = crate::xberg_runtime::request(
        &component,
        serde_json::json!({"command":"transcribe","path":path}),
        deadline.remaining(),
        &AtomicBool::new(false),
    )?;
    let response = crate::xberg_runtime::checked(response)?;
    response["markdown"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "Xberg 转录响应缺少 markdown".into())
}

/// 转录一步（进程启动接缝可注入）；worker 缺失时即时启动。单测经此注入 mock
/// 进程覆盖协议与生命周期语义（成功透传/崩溃重启/超时/优雅关闭）。
#[cfg(test)]
fn transcribe_with_slot(
    worker_slot: &mut Option<MediaWorker>,
    path: &Path,
    deadline: &markdown_document::Deadline,
    mut spawn: impl FnMut() -> Result<MediaWorker, String>,
) -> Result<String, String> {
    if worker_slot.is_none() {
        *worker_slot = Some(spawn()?);
    }
    let Some(worker) = worker_slot.as_mut() else {
        // 不变量：上一分支已确保槽位在位；防御性分支仅在不变量被破坏时到达。
        return Err("媒体转录进程意外缺失".to_string());
    };
    let failure = match worker.transcribe(path, deadline) {
        Ok(markdown) => return Ok(markdown),
        Err(failure) => failure,
    };
    match failure {
        // 逐文件失败（解码失败等）：进程存活，下一个文件继续复用（T-24）。
        TranscribeFailure::PerFile(message) => Err(message),
        // 超时（T-29）或进程/协议异常：终结整组进程并丢弃；该文件记失败，
        // 下一个媒体文件自动重启新进程（T-24 隔离）。
        TranscribeFailure::Timeout(message) | TranscribeFailure::WorkerLost(message) => {
            if let Some(mut worker) = worker_slot.take() {
                worker.terminate();
            }
            Err(message)
        }
    }
}

/// E2E 专用接缝（#[doc(hidden)]）：单个媒体文件的真实转录路径（组件解析 →
/// 常驻进程 → 协议请求 → markdown）。批处理语义（T-22/T-23/T-24/T-29）由
/// [`run`] 承载；本接缝仅供自动化验收从真实实现驱动一次完整转录。
#[doc(hidden)]
pub fn e2e_convert_media(path: &Path, timeout_secs: u64) -> Result<String, String> {
    if timeout_secs == 0 {
        return Err("单文件超时必须为正整秒".to_string());
    }
    let deadline = markdown_document::Deadline::new(Duration::from_secs(timeout_secs));
    convert_media(path, &deadline)
}

#[cfg(all(test, windows))]
struct MediaProcessJob {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(all(test, windows))]
impl MediaProcessJob {
    fn attach(child: &std::process::Child) -> Result<Self, String> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        // SAFETY: 未命名 Job Object，不传入外部指针。
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(format!(
                "无法创建媒体进程组：{}",
                std::io::Error::last_os_error()
            ));
        }
        let job = Self { handle };
        // SAFETY: 纯 C 结构；置零后只设置 KILL_ON_JOB_CLOSE 标志。
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: Job 句柄和同步调用期间的结构体指针均有效。
        let configured = unsafe {
            SetInformationJobObject(
                job.handle,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                    .map_err(|_| "媒体进程组结构大小超出范围".to_string())?,
            )
        };
        if configured == 0 {
            return Err(format!(
                "无法配置媒体进程组：{}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: Child 保持有效且拥有该进程句柄，Job 句柄同样有效。
        let assigned = unsafe { AssignProcessToJobObject(job.handle, child.as_raw_handle()) };
        if assigned == 0 {
            return Err(format!(
                "无法把媒体工作进程加入进程组：{}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(job)
    }

    fn terminate(&self) {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        // SAFETY: Job 句柄在本对象销毁前有效；结束组内的 worker 和 FFmpeg。
        let _ = unsafe { TerminateJobObject(self.handle, 1) };
    }
}

#[cfg(all(test, windows))]
impl Drop for MediaProcessJob {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        // SAFETY: 本对象独占 Job 句柄；KILL_ON_JOB_CLOSE 会回收残留子进程。
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

#[cfg(test)]
mod tests {
    use super::{
        compare_names, compare_paths, compare_paths_insensitive, media_component_error,
        media_worker_environment, occupancy_key, parse_formats, read_line_capped,
        run_formats_probe, scan, selected_xberg_extension, stderr_tail_for_message,
        transcribe_with_slot, write_new_markdown, FormatGroup, MediaWorker, OccupiedIndex, Options,
    };
    use crate::markdown_document::Deadline;
    use std::thread;
    use std::time::Instant;
    use std::{
        cmp::Ordering,
        collections::BTreeSet,
        ffi::{OsStr, OsString},
        fs,
        path::{Path, PathBuf},
        process::Command,
        sync::atomic::{AtomicBool, Ordering as AtomicOrdering},
        time::Duration,
    };

    fn supported() -> BTreeSet<String> {
        ["pdf", "docx"].into_iter().map(str::to_string).collect()
    }

    // 覆盖 T-07、T-08：仅 MP4/M4A 进入媒体链路，Xberg 清单中的其他媒体格式不得误入文档转换。
    #[test]
    fn other_xberg_media_formats_are_not_selected() {
        let groups = [FormatGroup::Other, FormatGroup::Media];
        assert!(!selected_xberg_extension("wmv", "video/x-ms-wmv", &groups));
        assert!(!selected_xberg_extension("mp3", "audio/mpeg", &groups));
        assert!(selected_xberg_extension("mp4", "video/mp4", &groups));
        assert!(selected_xberg_extension("m4a", "audio/mp4", &groups));
    }

    fn options(input_dir: PathBuf, output_dir: PathBuf, flat: bool) -> Options {
        Options {
            input_dir,
            output_dir,
            flat,
            groups: vec![FormatGroup::Pdf, FormatGroup::Office],
            timeout_secs: 21_600,
        }
    }

    // 覆盖 T-09、T-10、T-11：只读纳入 Git 树，输出子树排除，保留层级。
    #[test]
    fn scan_includes_git_project_but_excludes_output_subtree() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("input");
        let output = input.join("results");
        fs::create_dir_all(input.join("repo/.git")).unwrap();
        fs::create_dir_all(&output).unwrap();
        fs::write(input.join("repo/a.PDF"), b"pdf").unwrap();
        fs::write(output.join("generated.pdf"), b"pdf").unwrap();

        let plan = scan(&options(input, output.clone(), false), &supported()).unwrap();
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].relative, PathBuf::from("repo/a.PDF"));
        assert_eq!(
            plan.items[0].target,
            fs::canonicalize(&output).unwrap().join("repo/a_pdf.md")
        );
    }

    // 覆盖 T-11：平铺同名按稳定路径排序只保留第一份。
    #[test]
    fn flat_collision_keeps_sorted_first_and_counts_skip() {
        assert_eq!(
            compare_paths(
                PathBuf::from("A/Z/x.PDF").as_path(),
                PathBuf::from("a/a/x.pdf").as_path(),
            ),
            Ordering::Greater
        );
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("input");
        let output = temp.path().join("output");
        fs::create_dir_all(input.join("A")).unwrap();
        fs::create_dir_all(input.join("b")).unwrap();
        fs::create_dir_all(&output).unwrap();
        fs::write(input.join("A/x.PDF"), b"first").unwrap();
        fs::write(input.join("b/x.pdf"), b"second").unwrap();

        let plan = scan(&options(input, output.clone(), true), &supported()).unwrap();
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].relative, PathBuf::from("A/x.PDF"));
        assert_eq!(
            plan.items[0].target,
            fs::canonicalize(&output).unwrap().join("x_pdf.md")
        );
        assert_eq!(plan.summary.skipped_duplicate, 1);
    }

    // 覆盖 T-11（A'-低危2）：平铺决胜段（逐段大小写不敏感全等价、同分段结构）
    // 里「逐段原始 UTF-16（首段分出胜负即停）」与合同「原始路径 UTF-16 单元
    // 序列」必须选出同序——等价性依据：决胜段内各段两两大小写不敏感相等，而
    // 大小写不敏感相等的两段不可能互为真前缀（逐单元折叠不消灭单元、段内无
    // 分隔符），故两种口径的第一差异单元必落在同一段内同一位置。本用例跨多段、
    // 各段大小写形态不同（A\b\x.pdf vs a\B\x.pdf），固定该性质防回归。
    #[test]
    fn flat_tiebreak_per_component_matches_whole_path_sequence() {
        for (left, right) in [
            ("A/b/x.PDF", "a/B/x.pdf"),
            ("a/A/x.pdf", "a/a/x.pdf"),
            ("Aa/b/x.pdf", "aA/B/x.pdf"),
            ("deep/A/b/x.PDF", "deep/a/B/x.pdf"),
        ] {
            let left = Path::new(left);
            let right = Path::new(right);
            assert_eq!(
                compare_paths_insensitive(left, right),
                Ordering::Equal,
                "用例必须处于决胜段（逐段大小写不敏感等价）：{left:?} vs {right:?}"
            );
            assert_eq!(
                compare_paths(left, right),
                whole_path_original_order(left, right),
                "逐段决胜必须与合同整路径序列决胜一致：{left:?} vs {right:?}"
            );
        }
        assert_eq!(
            compare_paths(Path::new("A/b/x.PDF"), Path::new("a/B/x.pdf")),
            Ordering::Less,
            "首段大写 A（U+0041）原始序数小于小写 a（U+0061），平铺跳过时保留前者"
        );
    }

    /// 合同 T-11 的决胜口径参照实现：整条相对路径的原始 UTF-16 单元序列比较。
    fn whole_path_original_order(left: &Path, right: &Path) -> Ordering {
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            left.as_os_str()
                .encode_wide()
                .cmp(right.as_os_str().encode_wide())
        }
        #[cfg(not(windows))]
        {
            // 非 Windows 仅作参照：ASCII 用例下按 Unicode 标量比较与 UTF-16
            // 单元序列同序。
            left.as_os_str()
                .to_string_lossy()
                .cmp(&right.as_os_str().to_string_lossy())
        }
    }

    // 覆盖 T-11、T-12、T-24（A'-1 回归，键语义单元级）：层级模式的占用键 =
    // 「相对父目录+结果名」且大小写不敏感比较——同父目录内大小写变体结果名
    // （对应输入 a.PDF 与 a.pdf 在大小写敏感目录共存时的等价目标）必须命中
    // 跳过，父目录大小写变体同样等价；不同父目录不得挤占（T-10）；平铺键
    // 不含父目录（既有平铺决胜序不变）。
    // 物理场景说明：默认 NTFS 大小写不敏感，同目录两个大小写变体文件无法
    // 共存（第二次写入覆盖同一文件；fsutil 启用目录级大小写敏感需要管理员
    // 权限、CreateFileW(FILE_FLAG_POSIX_SEMANTICS) 也无法绕过），故同键跳过
    // 以本单元断言 + 不同目录集成护栏（hierarchical_duplicate_in_different_
    // directories_both_convert）共同锁定；scan 接线由两者与既有平铺测试约束。
    #[test]
    fn hierarchical_occupancy_key_collides_only_within_same_parent() {
        let mut occupied = OccupiedIndex::default();
        occupied.insert(occupancy_key(
            false,
            Path::new("docs"),
            OsStr::new("a_pdf.md"),
        ));
        assert!(
            occupied.contains(
                occupancy_key(false, Path::new("docs"), OsStr::new("A_PDF.MD")).as_os_str()
            ),
            "同父目录的结果名大小写变体必须命中占用（A'-1：第二个计同名跳过而非提交失败）"
        );
        assert!(
            occupied.contains(
                occupancy_key(false, Path::new("Docs"), OsStr::new("a_pdf.md")).as_os_str()
            ),
            "父目录大小写变体属于同一输出父目录，同样必须命中"
        );
        assert!(
            !occupied.contains(
                occupancy_key(false, Path::new("other"), OsStr::new("a_pdf.md")).as_os_str()
            ),
            "不同父目录不得挤占（T-10 层级保留）"
        );
        assert!(
            !occupied.contains(
                occupancy_key(true, Path::new("docs"), OsStr::new("A_PDF.MD")).as_os_str()
            ),
            "平铺键不含父目录，不得被层级键误命中（平铺决胜序不受影响）"
        );
    }

    // 覆盖 T-10、T-11（A'-1 伴随护栏）：层级模式的占用登记只约束同一输出父
    // 目录；不同子目录的同名文件各自独立转换，不得因全局重名被误跳过。
    #[test]
    fn hierarchical_duplicate_in_different_directories_both_convert() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("input");
        let output = temp.path().join("output");
        fs::create_dir_all(input.join("docs")).unwrap();
        fs::create_dir_all(input.join("notes")).unwrap();
        fs::create_dir_all(&output).unwrap();
        fs::write(input.join("docs/a.PDF"), b"one").unwrap();
        fs::write(input.join("notes/a.pdf"), b"two").unwrap();

        let plan = scan(&options(input, output, false), &supported()).unwrap();
        assert_eq!(plan.items.len(), 2, "不同子目录的同名文件互不挤占");
        assert_eq!(plan.summary.skipped_duplicate, 0);
    }

    // 覆盖 T-07、T-09（输出目录是输入目录的祖先时：输入子树不被前缀判定排除，
    // 递归处理全部匹配文件——只有输出位于输入子树内时才排除输出子树）
    #[test]
    fn scan_keeps_input_subtree_when_output_is_ancestor() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("data").join("reports");
        let output = temp.path().join("data");
        fs::create_dir_all(input.join("sub")).unwrap();
        fs::write(input.join("top.PDF"), b"pdf").unwrap();
        fs::write(input.join("sub/inner.pdf"), b"pdf").unwrap();

        let plan = scan(&options(input, output.clone(), false), &supported()).unwrap();
        assert_eq!(
            plan.items.len(),
            2,
            "输出为输入祖先时子树全部纳入递归处理：{:?}",
            plan.items
        );
        assert_eq!(plan.summary.skipped_existing, 0);
        assert_eq!(plan.summary.skipped_duplicate, 0);
    }

    // 覆盖 T-09：输入输出重合时扫描前拒绝。
    #[test]
    fn same_input_and_output_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().to_path_buf();
        let error = scan(&options(directory.clone(), directory, false), &supported()).unwrap_err();
        assert!(error.contains("不能相同"));
    }

    // 覆盖 T-12、T-25：提交时已有结果不得被覆盖，临时文件须清除。
    #[test]
    fn atomic_commit_refuses_existing_result() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("a_pdf.md");
        fs::write(&target, b"old").unwrap();
        let error = write_new_markdown(temp.path(), &target, "new").unwrap_err();
        assert!(error.contains("已有结果不会覆盖"));
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    }

    // ── F21：格式清单探测必须有界、可取消，输出截断不得当完整清单 ──

    #[cfg(windows)]
    fn hanging_command() -> Command {
        let mut command = Command::new("powershell");
        command.args(["-NoProfile", "-Command", "Start-Sleep -Seconds 60"]);
        command
    }

    #[cfg(not(windows))]
    fn hanging_command() -> Command {
        let mut command = Command::new("sleep");
        command.arg("60");
        command
    }

    #[cfg(windows)]
    fn spewing_command(mib: usize) -> Command {
        let mut command = Command::new("powershell");
        command.args([
            "-NoProfile",
            "-Command",
            &format!("[Console]::Out.Write(('x' * {}))", mib * 1024 * 1024),
        ]);
        command
    }

    // 覆盖 T-22/T-24（F21）：探测进程挂起时必须受总超时约束，宿主在预算内恢复。
    #[test]
    fn formats_probe_times_out_on_hanging_process() {
        let mut command = hanging_command();
        let cancel = AtomicBool::new(false);
        let started = Instant::now();
        let result = run_formats_probe(&mut command, Duration::from_millis(400), &cancel);
        assert!(result.is_err(), "挂起进程必须在探测超时后失败");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "探测超时后应立即返回，实际 {:?}",
            started.elapsed()
        );
    }

    // 覆盖 T-23（F21）：停止请求必须立刻中断格式探测，不等满超时。
    #[test]
    fn formats_probe_stops_promptly_on_cancel() {
        use std::sync::Arc;
        let mut command = hanging_command();
        let cancel = Arc::new(AtomicBool::new(false));
        let setter = cancel.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            setter.store(true, AtomicOrdering::Relaxed);
        });
        let started = Instant::now();
        let result = run_formats_probe(&mut command, Duration::from_secs(60), &cancel);
        assert!(result.is_err(), "取消后探测应失败");
        assert!(cancel.load(AtomicOrdering::Relaxed));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "取消后应立即返回，实际 {:?}",
            started.elapsed()
        );
    }

    // 覆盖 T-24（F21）：探测输出超过捕获上限时报截断错误，不得当完整清单解析。
    #[cfg(windows)]
    #[test]
    fn formats_probe_reports_truncated_output() {
        let mut command = spewing_command(9);
        let cancel = AtomicBool::new(false);
        let result = run_formats_probe(&mut command, Duration::from_secs(60), &cancel);
        let error = result.expect_err("截断的输出不得当成功");
        assert!(
            error.contains("截断") || error.contains("不完整"),
            "应说明输出不完整：{error}"
        );
    }

    // 覆盖 T-07/T-08（F21 守护）：探测正常返回时捕获完整输出。
    #[cfg(windows)]
    #[test]
    fn formats_probe_captures_valid_output() {
        let mut command = Command::new("powershell");
        command.args([
            "-NoProfile",
            "-Command",
            "[Console]::Out.Write('[{\"extension\":\".pdf\",\"mime_type\":\"application/pdf\"}]')",
        ]);
        let cancel = AtomicBool::new(false);
        let output = run_formats_probe(&mut command, Duration::from_secs(60), &cancel)
            .expect("正常探测应成功");
        assert!(String::from_utf8_lossy(&output.stdout).contains("pdf"));
    }

    // 覆盖 T-07/T-08：清单行按分组与媒体例外筛入支持集（纯函数回归）。
    #[test]
    fn parse_formats_filters_rows_by_group() {
        let stdout: &[u8] = concat!(
            r#"[{"extension":".pdf","mime_type":"application/pdf"},"#,
            r#"{"extension":".MP4","mime_type":"video/mp4"},"#,
            r#"{"extension":"wmv","mime_type":"video/x-ms-wmv"},"#,
            r#"{"extension":"","mime_type":"application/pdf"},"#,
            r#"{"extension":"tar.gz","mime_type":"application/x-tar"},"#,
            r#"{"extension":".docx","mime_type":"application/vnd.wordprocessing"}]"#
        )
        .as_bytes();
        let groups = vec![FormatGroup::Pdf, FormatGroup::Office, FormatGroup::Media];
        let selected = parse_formats(stdout, &groups).expect("合法清单应解析");
        let expected: BTreeSet<String> = ["pdf", "mp4", "docx"]
            .into_iter()
            .map(str::to_string)
            .collect();
        assert_eq!(selected, expected);
    }

    // ── 常驻 Xberg 转录进程的协议与生命周期（mock 子进程，T-19/T-22/T-23/T-24/T-29）──

    /// 编译（带缓存）并返回 mock worker 子进程的可执行文件路径。
    fn mock_worker_exe() -> PathBuf {
        static EXE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        EXE.get_or_init(|| {
            let source = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("fixtures")
                .join("mock_xberg_worker.rs");
            let output = std::env::temp_dir().join("jch-mock-xberg-worker.exe");
            let status = std::process::Command::new("rustc")
                .arg("--edition")
                .arg("2021")
                .arg("-D")
                .arg("warnings")
                .arg(&source)
                .arg("-o")
                .arg(&output)
                .status()
                .expect("启动 rustc 编译 mock worker 失败");
            assert!(status.success(), "mock worker 编译失败");
            output
        })
        .clone()
    }

    /// 构造注入 [`transcribe_with_slot`] 的 spawn 闭包，并统计实际启动次数。
    fn mock_spawner(
        mode: &'static str,
        spawns: std::rc::Rc<std::cell::Cell<usize>>,
    ) -> impl FnMut() -> Result<MediaWorker, String> {
        let mut command = Command::new(mock_worker_exe());
        command.arg(mode);
        move || {
            spawns.set(spawns.get() + 1);
            MediaWorker::spawn(&mut command)
        }
    }

    // 覆盖 T-19/T-20/T-22：成功响应按行解析并透传 markdown；批内串行复用同一
    // 常驻进程（两个文件只启动一次），响应 id 逐请求关联。
    #[test]
    fn media_worker_success_passthrough_and_batch_reuse() {
        let mut slot = None;
        let spawns = std::rc::Rc::new(std::cell::Cell::new(0));
        let deadline = Deadline::new(Duration::from_secs(60));
        let first = transcribe_with_slot(
            &mut slot,
            Path::new("C:/m/a.mp4"),
            &deadline,
            mock_spawner("ok", spawns.clone()),
        )
        .expect("成功转录应返回 markdown");
        assert_eq!(first, "MOCK MARKDOWN 1");
        let second = transcribe_with_slot(&mut slot, Path::new("C:/m/b.mp4"), &deadline, || {
            unreachable!("slot 已在位，不得重启进程")
        })
        .expect("第二个文件应复用同一常驻进程");
        assert_eq!(second, "MOCK MARKDOWN 2");
        assert_eq!(spawns.get(), 1, "批内两个文件必须复用同一常驻进程");
    }

    // 覆盖 T-20：has_audio=false 的成功响应同样透传 markdown（内含说明文本，
    // 由 Xberg 生成；本测断言客户端不丢弃该结果）。
    #[test]
    fn media_worker_no_audio_markdown_passthrough() {
        let mut slot = None;
        let spawns = std::rc::Rc::new(std::cell::Cell::new(0));
        let deadline = Deadline::new(Duration::from_secs(60));
        let markdown = transcribe_with_slot(
            &mut slot,
            Path::new("C:/m/silent.mp4"),
            &deadline,
            mock_spawner("noaudio", spawns),
        )
        .expect("无音轨的成功响应应返回 markdown");
        assert_eq!(markdown, "MOCK NOAUDIO MARKDOWN");
    }

    // 覆盖 T-24：单文件失败（ok:false）只让该文件失败；worker 存活，下一文件
    // 继续复用同一进程，不重启。
    #[test]
    fn media_worker_per_file_failure_keeps_worker_alive() {
        let mut slot = None;
        let spawns = std::rc::Rc::new(std::cell::Cell::new(0));
        let deadline = Deadline::new(Duration::from_secs(60));
        let error = transcribe_with_slot(
            &mut slot,
            Path::new("C:/m/broken.mp4"),
            &deadline,
            mock_spawner("fail-then-ok", spawns.clone()),
        )
        .expect_err("解码失败必须让该文件失败");
        assert!(
            error.contains("媒体转换失败：mock decode failure"),
            "错误应携带响应中的原因：{error}"
        );
        assert!(slot.is_some(), "逐文件失败不得丢弃常驻进程");
        let markdown =
            transcribe_with_slot(&mut slot, Path::new("C:/m/next.mp4"), &deadline, || {
                unreachable!("逐文件失败后不得重启进程")
            })
            .expect("下一个文件应继续复用进程并成功");
        assert_eq!(markdown, "MOCK MARKDOWN 2");
        assert_eq!(spawns.get(), 1, "逐文件失败不得触发重启");
    }

    // 覆盖 T-24：worker 崩溃使该文件失败，下一个文件自动重启新进程。
    #[test]
    fn media_worker_crash_fails_file_and_restarts_for_next() {
        let mut slot = None;
        let spawns = std::rc::Rc::new(std::cell::Cell::new(0));
        let deadline = Deadline::new(Duration::from_secs(60));
        let first = transcribe_with_slot(
            &mut slot,
            Path::new("C:/m/a.mp4"),
            &deadline,
            mock_spawner("exit-after-first", spawns.clone()),
        )
        .expect("第一条请求应成功");
        assert_eq!(first, "MOCK MARKDOWN 1");
        let error = transcribe_with_slot(
            &mut slot,
            Path::new("C:/m/b.mp4"),
            &deadline,
            mock_spawner("exit-after-first", spawns.clone()),
        )
        .expect_err("进程崩溃必须让该文件失败");
        assert!(
            error.contains("意外退出") || error.contains("无法向媒体转录进程发送请求"),
            "错误应说明进程退出或断连：{error}"
        );
        assert!(slot.is_none(), "崩溃后必须丢弃旧进程槽位");
        let restarted = transcribe_with_slot(
            &mut slot,
            Path::new("C:/m/c.mp4"),
            &deadline,
            mock_spawner("exit-after-first", spawns.clone()),
        )
        .expect("下一个文件必须自动重启新进程并成功");
        assert_eq!(restarted, "MOCK MARKDOWN 1");
        // 三次请求只应启动两个进程：初始一个 + 崩溃后为下一文件重启一个。
        assert_eq!(spawns.get(), 2, "崩溃后应为下一文件重启一次进程");
    }

    // 覆盖 T-23/T-29（F21）：单文件预算耗尽必须终结整组进程并让该文件失败，
    // 下一文件自动重启，宿主不永久阻塞。
    #[test]
    fn media_worker_timeout_kills_worker_and_restarts() {
        let mut slot = None;
        let spawns = std::rc::Rc::new(std::cell::Cell::new(0));
        let started = Instant::now();
        let deadline = Deadline::new(Duration::from_millis(400));
        let error = transcribe_with_slot(
            &mut slot,
            Path::new("C:/m/hang.mp4"),
            &deadline,
            mock_spawner("slow", spawns.clone()),
        )
        .expect_err("挂死的转录必须按预算超时");
        assert!(error.contains("媒体转换超时"), "{error}");
        assert!(slot.is_none(), "超时后必须丢弃进程槽位");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "超时后应立即恢复，实际 {:?}",
            started.elapsed()
        );
        let deadline = Deadline::new(Duration::from_millis(400));
        let error = transcribe_with_slot(
            &mut slot,
            Path::new("C:/m/next.mp4"),
            &deadline,
            mock_spawner("slow", spawns.clone()),
        )
        .expect_err("下一文件应重启新进程并再次按预算超时");
        assert!(error.contains("媒体转换超时"), "{error}");
        assert_eq!(spawns.get(), 2, "超时后应为下一文件重启一次进程");
    }

    // 覆盖 T-24：协议破坏（响应行非法 JSON）按进程异常处理，该文件失败并弃用进程。
    #[test]
    fn media_worker_garbage_response_fails_file() {
        let mut slot = None;
        let spawns = std::rc::Rc::new(std::cell::Cell::new(0));
        let deadline = Deadline::new(Duration::from_secs(60));
        let error = transcribe_with_slot(
            &mut slot,
            Path::new("C:/m/a.mp4"),
            &deadline,
            mock_spawner("garbage", spawns),
        )
        .expect_err("非法响应不得当成功");
        assert!(
            error.contains("响应与请求不匹配") || error.contains("意外退出"),
            "错误应说明协议异常：{error}"
        );
        assert!(slot.is_none(), "协议破坏后必须弃用该进程");
    }

    // 覆盖 T-24：单次响应超过捕获上限按「该文件失败」处理——进程与协议行边界
    // 仍完好，不得按进程异常终结（批内继续复用，不重启、不重载模型）。
    #[test]
    fn media_worker_oversize_response_fails_file_keeps_worker() {
        let mut slot = None;
        let spawns = std::rc::Rc::new(std::cell::Cell::new(0));
        let deadline = Deadline::new(Duration::from_secs(120));
        let error = transcribe_with_slot(
            &mut slot,
            Path::new("C:/m/huge.mp4"),
            &deadline,
            mock_spawner("oversize", spawns.clone()),
        )
        .expect_err("超限响应不得当成功");
        assert!(
            error.contains("媒体转换失败") && error.contains("捕获上限"),
            "错误应按单文件失败并说明上限：{error}"
        );
        assert!(slot.is_some(), "超限响应不得终结健康进程");
        let next = transcribe_with_slot(
            &mut slot,
            Path::new("C:/m/next.mp4"),
            &deadline,
            mock_spawner("oversize", spawns.clone()),
        )
        .expect("下一文件应复用进程并成功");
        assert_eq!(next, "MOCK MARKDOWN 2");
        assert_eq!(spawns.get(), 1, "超限响应不得触发重启");
    }

    // 覆盖 T-22/T-23：批结束（或用户停止）后丢弃 slot 时必须优雅关闭——关闭
    // stdin 让子进程收到 EOF 并自行退出（由 mock 的 EOF 标记文件证明），而非强杀。
    #[test]
    fn media_worker_drop_closes_stdin_for_graceful_exit() {
        let marker_dir = tempfile::tempdir().expect("创建标记目录");
        let marker = marker_dir.path().join("eof-marker");
        let mut command = Command::new(mock_worker_exe());
        command
            .arg("ok")
            .env("MOCK_XBERG_WORKER_EOF_MARKER", &marker);
        let mut worker = MediaWorker::spawn(&mut command).expect("启动 mock worker");
        let deadline = Deadline::new(Duration::from_secs(60));
        let markdown = worker
            .transcribe(Path::new("C:/m/a.mp4"), &deadline)
            .expect("转录应成功");
        assert_eq!(markdown, "MOCK MARKDOWN 1");
        assert!(!marker.exists(), "EOF 前不得出现标记文件");
        drop(worker);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !marker.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            marker.exists(),
            "丢弃 slot 必须触发 stdin EOF 并让子进程自行退出"
        );
    }

    // ── B'-6/A'-1：stdout 读线程的增量有界行读取 ──

    // 覆盖 T-24（B'-6）：正常行（含未以 \n 结尾的最后一行与 EOF 空返回）与
    // read_line 行为一致，行内容含结尾换行原样返回（空行与 EOF 可区分）。
    #[test]
    fn read_line_capped_reads_normal_lines() {
        let mut reader = std::io::Cursor::new(b"first\nsecond".to_vec());
        assert_eq!(
            read_line_capped(&mut reader, 64).unwrap(),
            b"first\n".to_vec()
        );
        assert_eq!(
            read_line_capped(&mut reader, 64).unwrap(),
            b"second".to_vec()
        );
        assert_eq!(
            read_line_capped(&mut reader, 64).unwrap(),
            Vec::<u8>::new(),
            "EOF 返回空行"
        );
        assert_eq!(
            read_line_capped(&mut reader, 64).unwrap(),
            Vec::<u8>::new(),
            "EOF 之后继续读取仍返回空行"
        );
    }

    // 覆盖 T-24（B'-6）：行字节数（含结尾 \n）恰好等于 cap 不算超限，与旧
    // 「read > MAX_CAPTURE_BYTES 才失败」口径一致。
    #[test]
    fn read_line_capped_allows_line_exactly_at_cap() {
        let mut input = vec![b'a'; 15];
        input.push(b'\n');
        let mut reader = std::io::Cursor::new(input.clone());
        assert_eq!(
            read_line_capped(&mut reader, 16).unwrap(),
            input,
            "恰好 cap 的行必须照常返回"
        );
        assert_eq!(
            read_line_capped(&mut reader, 16).unwrap(),
            Vec::<u8>::new(),
            "随后到达 EOF"
        );
    }

    // 覆盖 T-24（B'-6）：超限行立即返回 InvalidData 错误，错误文案沿用旧口径
    // 并给出整行实际字节数；该行剩余字节被排空到行边界且不整行分配内存——
    // 输入的剩余部分不被吞掉，下一行照常解析（协议行边界对齐，批次可继续）。
    #[test]
    fn read_line_capped_errors_on_oversize_and_keeps_line_boundary() {
        let mut input = vec![b'x'; 20]; // 含结尾 \n 共 21 字节，超过 cap=16
        input.push(b'\n');
        input.extend_from_slice(b"next\n");
        let mut reader = std::io::Cursor::new(input);
        let error = read_line_capped(&mut reader, 16).expect_err("超限行必须报错");
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::InvalidData,
            "超限必须以 InvalidData 区别于一般读错误：{error}"
        );
        assert!(
            error.to_string().contains("21 字节"),
            "错误应沿用捕获上限文案口径并给出整行字节数：{error}"
        );
        assert_eq!(
            read_line_capped(&mut reader, 16).unwrap(),
            b"next\n".to_vec(),
            "超限后读位置必须停在行边界，剩余输入照常可读"
        );
    }

    // 覆盖 T-21（B'-7）：组件解析类错误（未配置/不完整/版本与清单不一致/多版本）
    // 同属「组件未就绪」，经 convert_media 的包装必须统一携带初始化指引；单文件
    // 转换本身的失败（格式不支持、解码失败等）产生自转录链路、不经本包装，不得
    // 被误加指引（透传语义由 media_worker_per_file_failure_keeps_worker_alive
    // 等用例锁定）。
    #[test]
    fn media_component_error_unifies_reinit_guidance() {
        for resolve_error in [
            "Xberg 推理组件未配置：媒体转录所需的模型与运行库尚未安装",
            "推理组件不完整：缺少 models/vad/silero_vad.onnx（组件目录 C:/x）",
            "推理组件版本与清单不一致（安装 C:/x/v1，清单要求 v2）；请在对应功能页重新初始化以更新组件",
            "推理组件目录存在多个版本且无清单要求的 v2；请重新初始化以更新组件",
        ] {
            let wrapped = media_component_error(resolve_error);
            let already_guided = resolve_error.contains("重新初始化");
            assert_eq!(
                wrapped.starts_with("媒体转录组件未就绪，请重新初始化转 Markdown 功能："),
                !already_guided,
                "自带指引的错误不得双重包装，缺指引的必须统一口径：{wrapped}"
            );
            assert!(
                wrapped.contains(resolve_error),
                "原始错误信息（含安装目录与清单 tag）必须保留：{wrapped}"
            );
        }
    }

    // ── F29：平铺占用名查询不随规模线性增长比较次数 ──

    // 覆盖 T-11（F29 性能修复，语义不变）：1 千/1 万互异目标的比较次数都近似为零，
    // 增长阶不再随占用名规模线性上升。
    #[test]
    fn occupied_index_compare_count_does_not_grow_with_size() {
        for size in [1_000usize, 10_000] {
            let mut index = OccupiedIndex::default();
            for i in 0..size {
                index.insert(OsString::from(format!("file{i}_pdf.md")));
            }
            let compares = {
                let mut compares = 0usize;
                let mut probe = |a: &OsStr, b: &OsStr| {
                    compares += 1;
                    compare_names(a, b) == Ordering::Equal
                };
                for i in 0..100 {
                    let name = OsString::from(format!("probe{i}_pdf.md"));
                    assert!(!index.contains_with(&name, &mut probe));
                }
                compares
            };
            assert_eq!(
                compares, 0,
                "互异目标应命中不同桶，规模 {size} 时不应发生逐一比较"
            );
        }
    }

    // 覆盖 T-11（F29）：大小写等价变体落入同桶，恰好一次精确比较即判重。
    #[test]
    fn occupied_index_case_variant_hits_single_bucket() {
        let mut index = OccupiedIndex::default();
        index.insert(OsString::from("Report_pdf.md"));
        let compares = {
            let mut compares = 0usize;
            let mut probe = |a: &OsStr, b: &OsStr| {
                compares += 1;
                compare_names(a, b) == Ordering::Equal
            };
            let duplicate = OsString::from("REPORT_pdf.md");
            assert!(index.contains_with(&duplicate, &mut probe));
            let distinct = OsString::from("Reports_pdf.md");
            assert!(!index.contains_with(&distinct, &mut probe));
            compares
        };
        assert_eq!(compares, 1, "大小写变体应在同桶内一次精确确认");
    }

    // ── A2：进程丢失诊断的 stderr 尾部截取必须字符安全 ──

    // 覆盖 T-24（A2）：lost_message 曾按字节偏移切片，多字节 stderr 起点非
    // UTF-8 边界时直接 panic，杀死 gui spawn 的转换线程（CONVERTER_DONE/FAIL
    // 永不发送，GUI 永久 busy）。本例 751 字节（「错」×200 +「a」+「错」×50）：
    // 旧实现的字节起点 451 相对中文区偏移 ≡1 (mod 3)，落在「错」内部，必然
    // panic。注：纯「ASCII 前缀 + 连续中文」不会触发（300 恰为 3 的倍数、始终
    // 对齐），混排内容才暴露缺陷。
    #[test]
    fn stderr_tail_multibyte_slice_does_not_panic() {
        let stderr = format!("{}a{}", "错".repeat(200), "错".repeat(50));
        let suffix = stderr_tail_for_message(&stderr).expect("非空 stderr 必须有诊断尾部");
        assert_eq!(
            suffix,
            format!("：{stderr}"),
            "251 字符未超 300 字符上限，应完整保留"
        );
        assert!(!suffix.contains('\u{FFFD}'), "不得出现半个字符：{suffix}");
    }

    // 覆盖 T-24（A2）：超长尾部按字符截取，最多 300 个字符，截断后从首个
    // 空白之后取起，并以「：…」衔接。
    #[test]
    fn stderr_tail_caps_at_300_chars_and_drops_partial_word() {
        let stderr = format!("模块加载失败 {}", "错".repeat(400));
        let suffix = stderr_tail_for_message(&stderr).expect("非空 stderr 必须有诊断尾部");
        assert!(suffix.starts_with("：…"), "截断时以省略号衔接：{suffix}");
        let body = suffix.strip_prefix("：…").unwrap();
        assert!(
            body.chars().count() <= 300,
            "尾部最多 300 个字符，实际 {}",
            body.chars().count()
        );
        assert!(!body.contains('\u{FFFD}'), "不得出现半个字符：{body}");
        assert!(
            body.chars().all(|character| character == '错'),
            "截断应从首个空白之后取起且只保留完整字符：{body}"
        );
    }

    // 覆盖 T-24（A2）：截断窗口内含空白时，从尾部首个空白之后的完整词开头。
    #[test]
    fn stderr_tail_truncated_starts_after_first_whitespace() {
        let stderr = format!("{} abc {}", "错".repeat(200), "错".repeat(200));
        let suffix = stderr_tail_for_message(&stderr).expect("非空 stderr 必须有诊断尾部");
        assert!(suffix.starts_with("：…"), "总 405 字符必然截断：{suffix}");
        let body = suffix.strip_prefix("：…").unwrap();
        assert!(
            body.starts_with("abc"),
            "截断后应从首个空白之后的完整词开头：{suffix}"
        );
        assert!(!body.contains('\u{FFFD}'), "不得出现半个字符：{body}");
    }

    // 覆盖 T-24（A2 守护）：短 ASCII 尾部完整保留，不截断、不加省略号。
    #[test]
    fn stderr_tail_short_ascii_is_kept_whole() {
        let suffix =
            stderr_tail_for_message("  decode failed  \n").expect("非空 stderr 必须有诊断尾部");
        assert_eq!(suffix, "：decode failed");
    }

    // ── B-3：媒体转录进程的离线环境口径与文档转换路径一致 ──

    // 覆盖 T-21/XB-04（B-3）：spawn_for_root 曾只注入三个 XBERG_* 目录指针，
    // 缺 HF/Transformers 离线开关；若 xberg worker 内部存在 HF hub 回退即违反
    // 离线要求。两侧同为 xberg.exe worker 子命令，口径必须与文档转换路径
    // （markdown_document::apply_offline_environment）一致；此处只补纯开关型
    // 变量，不引入 HF_HOME/HF_HUB_CACHE 等路径假设（媒体组件目录结构不同）。
    #[test]
    fn media_worker_environment_matches_document_offline_policy() {
        let root = Path::new("C:/xberg-component");
        let env = media_worker_environment(root);
        let find = |key: &str| -> Option<String> {
            env.iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        };
        for key in [
            "HF_HUB_OFFLINE",
            "HUGGINGFACE_HUB_OFFLINE",
            "TRANSFORMERS_OFFLINE",
            "HF_DATASETS_OFFLINE",
            "NO_COLOR",
        ] {
            assert_eq!(
                find(key).as_deref(),
                Some("1"),
                "媒体转录进程缺少离线开关 {key}"
            );
        }
        assert_eq!(
            find("XBERG_ORT_EP").as_deref(),
            Some("cpu"),
            "媒体转录进程必须固定 CPU 推理"
        );
        assert_eq!(
            find("XBERG_MAX_CONCURRENT_REQUESTS").as_deref(),
            Some("1"),
            "媒体转录进程必须串行处理请求"
        );
        // B'-8：perf 日志目录必须固定指向系统临时目录——组件若编入 perf-tracing
        // feature 会在当前工作目录创建 logs/perf.log.*（配置发现之外唯一主动向
        // CWD 写文件的路径），注入该变量封死；未编入时变量被无害忽略。
        let perf_log_dir =
            find("XBERG_PERF_LOG_DIR").unwrap_or_else(|| panic!("缺少 XBERG_PERF_LOG_DIR"));
        assert!(
            perf_log_dir.contains("JchTools-xberg-perf"),
            "XBERG_PERF_LOG_DIR 应指向专用临时目录：{perf_log_dir}"
        );
        // 既有组件目录指针保持不变：仍指向组件目录内的模型与运行库。
        for (key, sub) in [
            ("XBERG_SENSEVOICE_MODEL_DIR", "models"),
            ("XBERG_SHERPA_DLL_DIR", "sherpa-onnx"),
            ("XBERG_FFMPEG_DLL_DIR", "ffmpeg"),
        ] {
            let value = find(key).unwrap_or_else(|| panic!("缺少组件目录变量 {key}"));
            let root_name = root.file_name().unwrap().to_string_lossy();
            assert!(
                value.contains(root_name.as_ref()) && value.contains(sub),
                "{key} 应指向组件目录内的 {sub}：{value}"
            );
        }
    }
}
