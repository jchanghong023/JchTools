//! 转 Markdown 的只读扫描、任务编排与结果落盘（T-07～T-12、T-22～T-25）。

use std::{
    cmp::Ordering,
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering as AtomicOrdering},
    thread,
    time::Duration,
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
    let supported = match supported_formats(&runtime_dir, &options.groups, cancel) {
        Ok(formats) => formats,
        Err(message) => {
            if cancel.load(AtomicOrdering::Relaxed) {
                // T-23：主动停止不作为错误；返回已停止的空汇总，由界面按停止收尾。
                return Ok(Summary {
                    stopped: true,
                    ..Summary::default()
                });
            }
            return Err(message);
        }
    };
    let plan = scan(options, &supported)?;
    let total = plan.items.len() + plan.summary.skipped_existing + plan.summary.skipped_duplicate;
    events(Event::Started { total });
    let mut summary = plan.summary;
    let output_root = plan.output_root;
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

/// 平铺输出名占用索引（T-11/F29）：按大小写折叠键分桶，桶内候选再用
/// [`compare_names`]（Windows `CompareStringOrdinal` 忽略大小写）精确确认。
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
        if options.flat && occupied.contains(&file_name) {
            plan.summary.skipped_duplicate += 1;
            continue;
        }
        if options.flat {
            occupied.insert(file_name.clone());
        }
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

/// 格式清单探测的总超时：这是启动前的元数据查询，不受（也不占用）单文件预算，
/// 但必须有界（F21/T-22），并响应停止请求（T-23）。
const FORMATS_PROBE_TIMEOUT: Duration = Duration::from_secs(60);

fn supported_formats(
    runtime_dir: &Path,
    groups: &[FormatGroup],
    cancel: &AtomicBool,
) -> Result<BTreeSet<String>, String> {
    let mut command = Command::new(runtime_dir.join("xberg.exe"));
    command
        .args(["formats", "--format", "json"])
        .current_dir(runtime_dir);
    crate::markdown_document::apply_offline_environment(&mut command, runtime_dir);
    let output = run_formats_probe(&mut command, FORMATS_PROBE_TIMEOUT, cancel)?;
    let mut selected = parse_formats(&output.stdout, groups)?;
    if groups.contains(&FormatGroup::Media) {
        selected.extend(MEDIA.iter().map(|extension| (*extension).to_string()));
    }
    Ok(selected)
}

/// 有界且可取消地运行格式清单探测（F21）：超时或取消后 kill + 限时收尾，
/// 输出被截断时如实报错，不得把半截清单当完整结果。
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
    let mut left_parts = left.components();
    let mut right_parts = right.components();
    loop {
        match (left_parts.next(), right_parts.next()) {
            (Some(a), Some(b)) => {
                let order = compare_names(a.as_os_str(), b.as_os_str());
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

fn compare_paths_original(left: &Path, right: &Path) -> Ordering {
    let mut left_parts = left.components();
    let mut right_parts = right.components();
    loop {
        match (left_parts.next(), right_parts.next()) {
            (Some(a), Some(b)) => {
                let order = compare_original(a.as_os_str(), b.as_os_str());
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

/// 媒体工作进程一次运行的结果（F21：统一有界读 + 限时收尾）。
#[derive(Debug)]
struct MediaProcessOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// 运行媒体工作进程直到退出或单文件预算耗尽（F21/T-23/T-29）：读线程按
/// 8 MiB 上限捕获，所有收尾 join 都经 `join_with_deadline` 限时，超时先经
/// Job Object 终结整组进程（含 FFmpeg 子进程）再回收，孙进程持有管道写端时
/// 超限放弃读线程，不永久阻塞宿主。
fn run_media_process(
    command: &mut Command,
    deadline: &markdown_document::Deadline,
) -> Result<MediaProcessOutput, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("无法启动媒体工作进程：{e}"))?;
    #[cfg(windows)]
    let job = match MediaProcessJob::attach(&child) {
        Ok(job) => job,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| "无法读取媒体结果".to_string())?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| "无法读取媒体错误".to_string())?;
    let stdout_thread = thread::spawn(move || {
        crate::process::read_all_capped(stdout_pipe, crate::process::MAX_CAPTURE_BYTES)
    });
    let stderr_thread = thread::spawn(move || {
        crate::process::read_all_capped(stderr_pipe, crate::process::MAX_CAPTURE_BYTES)
    });
    let terminate_and_wait = |child: &mut std::process::Child| {
        #[cfg(windows)]
        job.terminate();
        #[cfg(not(windows))]
        let _ = child.kill();
        let _ = child.wait();
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if deadline.expired() => {
                terminate_and_wait(&mut child);
                // 读线程可能因孙进程持有管道写端而不 EOF：限时收尾，超限放弃。
                let _ = crate::process::join_with_deadline(
                    stdout_thread,
                    crate::process::PIPE_DRAIN_GRACE,
                );
                let _ = crate::process::join_with_deadline(
                    stderr_thread,
                    crate::process::PIPE_DRAIN_GRACE,
                );
                return Err(format!("媒体转换超时（{} 秒）", deadline.total().as_secs()));
            }
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                terminate_and_wait(&mut child);
                let _ = crate::process::join_with_deadline(
                    stdout_thread,
                    crate::process::PIPE_DRAIN_GRACE,
                );
                let _ = crate::process::join_with_deadline(
                    stderr_thread,
                    crate::process::PIPE_DRAIN_GRACE,
                );
                return Err(format!("无法查询媒体工作进程状态：{e}"));
            }
        }
    };
    // 正常退出后同样限时收尾：管道未能在宽限期内排空（孙进程持写端）时按
    // 不完整处理，不得把半截输出当完整 JSON。
    let stdout =
        crate::process::join_with_deadline(stdout_thread, crate::process::PIPE_DRAIN_GRACE)
            .ok_or_else(|| "媒体结果输出未能在收尾期内读满，结果可能不完整".to_string())?;
    let stderr =
        crate::process::join_with_deadline(stderr_thread, crate::process::PIPE_DRAIN_GRACE)
            .unwrap_or_default();
    if let Some(error) = stdout.error {
        return Err(format!("读取媒体结果失败：{error}"));
    }
    if stdout.truncated {
        return Err("媒体结果输出被截断（超过捕获上限），不能当完整结果".to_string());
    }
    Ok(MediaProcessOutput {
        status,
        stdout: stdout.data,
        stderr: stderr.data,
    })
}

fn convert_media(path: &Path, deadline: &markdown_document::Deadline) -> Result<String, String> {
    let worker = markdown_assets::media_worker_path();
    if !worker.is_file() {
        return Err("媒体工作进程未安装，请重新初始化转 Markdown 功能".to_string());
    }
    let mut command = Command::new(worker);
    command
        .arg("--input")
        .arg(path)
        .arg("--models")
        .arg(markdown_assets::media_models_dir())
        .env_remove("SHERPA_ONNX_DLL")
        .env_remove("ALL2MARKDOWN_FFMPEG");
    let output = run_media_process(&mut command, deadline)?;
    if !output.status.success() {
        return Err(format!(
            "媒体转换失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("媒体结果格式错误：{e}"))?;
    value
        .get("markdown")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "媒体结果缺少 Markdown".to_string())
}

#[cfg(windows)]
struct MediaProcessJob {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
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

#[cfg(windows)]
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
        compare_names, compare_paths, parse_formats, run_formats_probe, run_media_process, scan,
        selected_xberg_extension, write_new_markdown, FormatGroup, OccupiedIndex, Options,
    };
    use crate::markdown_document::Deadline;
    use std::{
        cmp::Ordering,
        collections::BTreeSet,
        ffi::{OsStr, OsString},
        fs,
        path::PathBuf,
        process::Command,
        sync::atomic::{AtomicBool, Ordering as AtomicOrdering},
        thread,
        time::{Duration, Instant},
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

    // ── F21：媒体工作进程收尾必须有界 ──

    // 覆盖 T-23/T-29（F21）：媒体进程挂死时按预算终结整组进程并恢复，不永久阻塞。
    #[test]
    fn media_process_deadline_recovers_from_hung_worker() {
        let mut command = hanging_command();
        let deadline = Deadline::new(Duration::from_millis(400));
        let started = Instant::now();
        let result = run_media_process(&mut command, &deadline);
        let error = result.expect_err("挂死的媒体进程必须超时失败");
        assert!(error.contains("媒体转换超时"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "超时后应立即返回，实际 {:?}",
            started.elapsed()
        );
    }

    // 覆盖 T-25（F21）：媒体输出被截断时必须失败，不得把半截 JSON 当结果。
    #[cfg(windows)]
    #[test]
    fn media_process_truncated_output_fails() {
        let mut command = spewing_command(9);
        let deadline = Deadline::new(Duration::from_secs(60));
        let result = run_media_process(&mut command, &deadline);
        let error = result.expect_err("截断的媒体输出必须失败");
        assert!(
            error.contains("截断") || error.contains("不完整"),
            "应说明输出不完整：{error}"
        );
    }

    // 覆盖 T-22（F21 守护）：正常退出进程的输出完整返回。
    #[cfg(windows)]
    #[test]
    fn media_process_returns_complete_output() {
        let mut command = Command::new("powershell");
        command.args([
            "-NoProfile",
            "-Command",
            "[Console]::Out.Write('{\"markdown\":\"ok\"}')",
        ]);
        let deadline = Deadline::new(Duration::from_secs(60));
        let output = run_media_process(&mut command, &deadline).expect("正常进程应成功返回");
        assert_eq!(output.stdout, b"{\"markdown\":\"ok\"}".to_vec());
        assert!(output.status.success());
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
}
