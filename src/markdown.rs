//! 转 Markdown 的只读扫描、任务编排与结果落盘（T-07～T-12、T-22～T-25）。

use std::{
    cmp::Ordering,
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering as AtomicOrdering},
    thread,
    time::{Duration, Instant},
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
    let supported = supported_formats(&runtime_dir, &options.groups)?;
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
        let timeout = Duration::from_secs(options.timeout_secs);
        let outcome = if item.is_media {
            convert_media(&item.source, timeout).map(|markdown| markdown_document::DocumentOutput {
                markdown,
                warnings: Vec::new(),
            })
        } else {
            let pages = markdown_document::page_count(&item.source);
            let fast = pages.is_some_and(|count| count > 200);
            if fast {
                events(Event::Log(format!(
                    "{}：{} 页，快速模式关闭版面识别与图片 OCR",
                    item.relative.display(),
                    pages.unwrap_or_default()
                )));
            }
            markdown_document::convert(&item.source, &runtime_dir, fast, timeout)
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

fn scan(options: &Options, supported: &BTreeSet<String>) -> Result<Plan, String> {
    let input = checked_directory(&options.input_dir)?;
    let output = checked_directory(&options.output_dir)?;
    if path_equal(&input, &output) {
        return Err("输入与输出目录不能相同".to_string());
    }
    let mut paths = Vec::new();
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
                if !path_begins_with(&path, &output) {
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
    let mut occupied = Vec::<OsString>::new();
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
        if options.flat
            && occupied
                .iter()
                .any(|name| compare_names(name, &file_name) == Ordering::Equal)
        {
            plan.summary.skipped_duplicate += 1;
            continue;
        }
        if options.flat {
            occupied.push(file_name.clone());
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

fn supported_formats(
    runtime_dir: &Path,
    groups: &[FormatGroup],
) -> Result<BTreeSet<String>, String> {
    let mut command = Command::new(runtime_dir.join("xberg.exe"));
    command
        .args(["formats", "--format", "json"])
        .current_dir(runtime_dir)
        .stdin(Stdio::null());
    crate::markdown_document::apply_offline_environment(&mut command, runtime_dir);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let output = command
        .output()
        .map_err(|e| format!("无法读取 Xberg 格式清单：{e}"))?;
    if !output.status.success() {
        return Err(format!(
            "Xberg 格式清单失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let rows: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("Xberg 格式清单无效：{e}"))?;
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
        let group = classify_format(&extension, mime);
        if groups.contains(&group) {
            selected.insert(extension);
        }
    }
    if groups.contains(&FormatGroup::Media) {
        selected.extend(MEDIA.iter().map(|extension| (*extension).to_string()));
    }
    Ok(selected)
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
        fs::hard_link(&temp, target)
            .map_err(|e| format!("无法提交新结果（已有结果不会覆盖）：{e}"))?;
        Ok(())
    })();
    let _ = fs::remove_file(&temp);
    write_result
}

fn convert_media(path: &Path, timeout: Duration) -> Result<String, String> {
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
        .env_remove("ALL2MARKDOWN_FFMPEG")
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
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "无法读取媒体结果".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "无法读取媒体错误".to_string())?;
    let output_thread = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = std::io::BufReader::new(stdout).read_to_end(&mut bytes);
        bytes
    });
    let error_thread = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = std::io::BufReader::new(stderr).read_to_end(&mut bytes);
        bytes
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= timeout => {
                #[cfg(windows)]
                job.terminate();
                #[cfg(not(windows))]
                let _ = child.kill();
                let _ = child.wait();
                let _ = output_thread.join();
                let _ = error_thread.join();
                return Err(format!("媒体转换超过 {} 秒", timeout.as_secs()));
            }
            Ok(None) => thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(format!("无法查询媒体工作进程状态：{e}")),
        }
    };
    let stdout = output_thread
        .join()
        .map_err(|_| "读取媒体结果失败".to_string())?;
    let stderr = error_thread
        .join()
        .map_err(|_| "读取媒体错误失败".to_string())?;
    if !status.success() {
        return Err(format!(
            "媒体转换失败：{}",
            String::from_utf8_lossy(&stderr).trim()
        ));
    }
    let value: serde_json::Value =
        serde_json::from_slice(&stdout).map_err(|e| format!("媒体结果格式错误：{e}"))?;
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
    use super::{compare_paths, scan, write_new_markdown, FormatGroup, Options};
    use std::{cmp::Ordering, collections::BTreeSet, fs, path::PathBuf};

    fn supported() -> BTreeSet<String> {
        ["pdf", "docx"].into_iter().map(str::to_string).collect()
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
}
