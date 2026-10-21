use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::Duration;

use super::naming::OccupiedIndex;
use crate::{markdown_assets, markdown_document};
use jchtools_file_core::fsutil;

/// T-14：媒体目录名 = 产物主干（空白折叠为下划线，去掉尾部点/空格）+ `_media`。
/// 与目标所在输出父目录内已有普通项同名时追加序号让路；链接/junction 直接拒绝。
/// 同一批次内已分配的媒体目录也计入占用，避免空白折叠后的名称碰撞。
pub(super) fn media_dir_name(output_root: &Path, target: &Path) -> Result<String, String> {
    let parent = target.parent().unwrap_or(output_root);
    let mut occupied = OccupiedIndex::default();
    media_dir_name_reserved(parent, target, &mut occupied)
}

pub(super) fn media_dir_name_reserved(
    parent: &Path,
    target: &Path,
    occupied: &mut OccupiedIndex,
) -> Result<String, String> {
    let stem = target
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("media");
    let mut base = stem.split_whitespace().collect::<Vec<_>>().join("_");
    base = base.trim_end_matches(['.', ' ']).to_string();
    if base.is_empty() {
        base = "media".to_string();
    }
    let mut candidate = format!("{base}_media");
    let mut sequence = 1u32;
    loop {
        if occupied.contains(OsStr::new(&candidate)) {
            sequence = sequence.saturating_add(1);
            candidate = format!("{base}_{sequence}_media");
            continue;
        }
        let path = parent.join(&candidate);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if fsutil::is_link(&metadata) => {
                return Err(format!("媒体目录含符号链接或 junction：{}", path.display()));
            }
            Ok(_) => {
                sequence = sequence.saturating_add(1);
                candidate = format!("{base}_{sequence}_media");
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                occupied.insert(OsString::from(&candidate));
                return Ok(candidate);
            }
            Err(error) => {
                return Err(format!("无法检查媒体目录 {}：{error}", path.display()));
            }
        }
    }
}

/// 转换一个媒体文件：无损验证请求路径编码，验证媒体资产后请求 transcribe。
pub(super) fn convert_media(
    path: &Path,
    deadline: &markdown_document::Deadline,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let span = jchtools_foundation::logging::operation_span("markdown", "transcribe");
    let _entered = span.enter();
    let started = std::time::Instant::now();
    let mut stage = "media_assets";
    tracing::debug!(event = "media_conversion_started", "媒体转换开始");
    let result = (|| {
        jchtools_assets::asset_util::ensure_not_cancelled(cancel)?;
        stage = "path_encoding";
        let request_path = path
            .to_str()
            .ok_or_else(|| "输入路径编码无效：Xberg 请求要求有效 UTF-8 路径".to_string())?;
        stage = "media_assets";
        markdown_assets::validate_media()?;
        let component = markdown_assets::media_component_dir().map_err(|error| {
            format!("缺少可选音频资产，请在设置页主动下载；不会自动联网：{error}")
        })?;
        stage = "transcribe_request_boundary";
        let response = jchtools_xberg::xberg_runtime::request(
            &component,
            serde_json::json!({"command":"transcribe","path":request_path}),
            deadline.remaining(),
            cancel,
        )?;
        let response = jchtools_xberg::xberg_runtime::checked(response)?;
        stage = "response_protocol";
        jchtools_assets::asset_util::ensure_not_cancelled(cancel)?;
        let markdown = response["markdown"]
            .as_str()
            .ok_or_else(|| "Xberg 转录响应缺少 markdown".to_string())?;
        if markdown.trim().is_empty() {
            return Err("Xberg 转录响应的 markdown 为空，未生成有效媒体结果".to_string());
        }
        Ok(markdown.to_owned())
    })();
    if let Ok(markdown) = &result {
        tracing::debug!(
            event = "media_conversion_completed",
            markdown_bytes = markdown.len(),
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "媒体转换完成"
        );
    } else {
        tracing::error!(
            event = "media_conversion_failed",
            stage,
            error_type = if stage == "response_protocol" {
                "protocol"
            } else {
                "media_boundary"
            },
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            cancel_requested = cancel.load(AtomicOrdering::Acquire),
            "媒体转换失败，详情由上游结构化事件诊断"
        );
    }
    result
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
    convert_media(path, &deadline, &AtomicBool::new(false))
}

#[doc(hidden)]
pub fn test_media_dir_name(output_root: &Path, target: &Path) -> Result<String, String> {
    media_dir_name(output_root, target)
}
