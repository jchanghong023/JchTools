use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

#[cfg(test)]
use std::process::Command;
#[cfg(test)]
use std::sync::atomic::Ordering as AtomicOrdering;

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
pub(super) const MEDIA: &[&str] = &["mp4", "m4a"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatGroup {
    Pdf,
    Office,
    Images,
    Media,
    Other,
}

/// T-08/XB-09/XB-26：格式能力取自当前选中的共享引擎，不用发布清单限制
/// 用户替换后的引擎。纯媒体任务仍只处理固定 MP4/M4A，不依赖文档格式能力。
pub(super) fn supported_formats(
    runtime_dir: &Path,
    groups: &[FormatGroup],
    cancel: &AtomicBool,
) -> Result<BTreeSet<String>, String> {
    let mut selected = BTreeSet::new();
    if groups.iter().any(|group| *group != FormatGroup::Media) {
        let response = jchtools_xberg::xberg_runtime::request(
            runtime_dir,
            serde_json::json!({"command": "formats"}),
            Duration::from_secs(15),
            cancel,
        )
        .and_then(jchtools_xberg::xberg_runtime::checked)?;
        let rows = response
            .get("formats")
            .and_then(serde_json::Value::as_array)
            .ok_or("Xberg 格式清单协议异常：formats 必须是数组")?;
        for required in ["pdf", "docx", "pptx", "xlsx"] {
            if !rows.iter().any(|row| {
                row.get("extension")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|extension| {
                        extension
                            .trim_start_matches('.')
                            .eq_ignore_ascii_case(required)
                    })
            }) {
                return Err(format!("当前 Xberg 不支持必需文档格式：{required}"));
            }
        }
        selected = parse_format_rows(rows, groups);
    }
    if groups.contains(&FormatGroup::Media) {
        selected.extend(MEDIA.iter().map(|extension| (*extension).to_string()));
    }
    Ok(selected)
}

/// 有界且可取消地运行格式清单探测（F21）：超时或取消后 kill + 限时收尾，
/// 输出被截断时如实报错，不得把半截清单当完整结果。
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
pub(super) fn run_formats_probe(
    command: &mut Command,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<jchtools_file_core::process::CapturedOutput, String> {
    let result = jchtools_file_core::process::run_with_timeout_cancel(command, timeout, cancel);
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
pub(super) fn parse_formats(stdout: &[u8], groups: &[FormatGroup]) -> Result<BTreeSet<String>, String> {
    let rows: Vec<serde_json::Value> =
        serde_json::from_slice(stdout).map_err(|e| format!("Xberg 格式清单无效：{e}"))?;
    Ok(parse_format_rows(&rows, groups))
}

/// 直接读取共享响应中的格式行，不序列化再解析同一份 JSON。
fn parse_format_rows(rows: &[serde_json::Value], groups: &[FormatGroup]) -> BTreeSet<String> {
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
    selected
}

pub(super) fn selected_xberg_extension(
    extension: &str,
    mime: &str,
    groups: &[FormatGroup],
) -> bool {
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
