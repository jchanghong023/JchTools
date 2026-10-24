use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::Duration;

use crate::markdown_document;
use jchtools_file_core::fsutil;

fn log_output_io(error: &std::io::Error, stage: &'static str) {
    tracing::error!(event = "markdown_output_io_failed", stage, error_type = "io",
        error_code = error.raw_os_error(), error_kind = ?error.kind(), "Markdown输出文件系统调用失败");
}

fn log_output_cleanup(result: std::io::Result<()>, stage: &'static str) {
    if let Err(error) = result {
        if error.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                event = "markdown_output_cleanup_failed",
                stage,
                error_type = "io",
                error_code = error.raw_os_error(),
                "Markdown未提交输出清理失败"
            );
        }
    }
}

fn prepare_media_destination(
    parent: &Path,
    relative: &str,
    created_dirs: &mut Vec<PathBuf>,
) -> Result<PathBuf, String> {
    let components: Vec<_> = Path::new(relative).components().collect();
    if components.is_empty() {
        return Err("媒体图片路径为空".to_string());
    }
    let mut current = parent.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err("媒体图片路径含非法目录段".to_string());
        };
        current.push(name);
        let is_file = index + 1 == components.len();
        match fs::symlink_metadata(&current) {
            Ok(metadata) if fsutil::is_link(&metadata) => {
                return Err(format!(
                    "媒体输出路径含符号链接或 junction：{}",
                    current.display()
                ));
            }
            Ok(_) if is_file => {
                return Err(format!(
                    "媒体图片目标已存在，不会覆盖：{}",
                    current.display()
                ));
            }
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Err(format!("媒体输出路径不是目录：{}", current.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !is_file => {
                fs::create_dir(&current)
                    .map_err(|error| format!("无法创建媒体目录 {}：{error}", current.display()))?;
                created_dirs.push(current.clone());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "无法检查媒体输出路径 {}：{error}",
                    current.display()
                ));
            }
        }
    }
    Ok(current)
}

/// T-29（S4-02）：单文件预算是否已耗尽——`remaining` 为零即耗尽；无界预算
/// （`Deadline` 内部 `None`）的 `remaining` 为 `Duration::MAX`，恒不耗尽。
fn deadline_exhausted(deadline: &markdown_document::Deadline) -> bool {
    deadline.remaining() == Duration::ZERO
}

pub(super) fn write_new_markdown(
    output_root: &Path,
    target: &Path,
    content: &str,
    media: &[markdown_document::MediaFile],
    deadline: &markdown_document::Deadline,
) -> Result<(), String> {
    write_new_markdown_cancelable(
        output_root,
        target,
        content,
        media,
        deadline,
        &AtomicBool::new(false),
    )
}

pub(super) fn write_new_markdown_cancelable(
    output_root: &Path,
    target: &Path,
    content: &str,
    media: &[markdown_document::MediaFile],
    deadline: &markdown_document::Deadline,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let span = jchtools_foundation::logging::operation_span("markdown", "write_output");
    let _entered = span.enter();
    let started = std::time::Instant::now();
    let mut stage = "output_directory";
    tracing::debug!(
        event = "markdown_output_started",
        markdown_bytes = content.len(),
        media_count = media.len(),
        "Markdown结果落盘开始"
    );
    let result = (|| {
        let parent = ensure_output_tree(output_root, target, cancel)?;
        // T-14：先落图片（本次任务的未提交材料），md 最后以不覆盖改名提交——
        // md 在场即代表 media 已完整（T-25：不留半成品）；中途失败回滚本次已写
        // 图片并删除空媒体目录，不触碰既有文件。
        let mut created: Vec<std::path::PathBuf> = Vec::new();
        let mut created_dirs: Vec<std::path::PathBuf> = Vec::new();
        stage = "media_write";
        if let Err(error) = write_media_files(
            &parent,
            media,
            deadline,
            cancel,
            &mut created,
            &mut created_dirs,
        ) {
            rollback_uncommitted(&created, &created_dirs);
            return Err(error);
        }
        let temp = parent.join(format!(".jch-markdown-{}.tmp", uuid::Uuid::new_v4()));
        stage = "markdown_write";
        let write_result = commit_markdown(target, &temp, content, deadline, cancel, &mut stage);
        log_output_cleanup(fs::remove_file(&temp), "temporary_output");
        if write_result.is_err() {
            // 提交失败（含目标被外部占用的保险丝）：回滚本次已写图片并删除空
            // 媒体目录，不把无主媒体留给旧结果（T-25）。
            rollback_uncommitted(&created, &created_dirs);
        }
        write_result
    })();
    if let Ok(()) = &result {
        tracing::debug!(
            event = "markdown_output_completed",
            component = "markdown",
            markdown_bytes = content.len(),
            media_count = media.len(),
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "Markdown结果落盘完成"
        );
    } else {
        tracing::error!(
            event = "markdown_output_failed",
            stage,
            error_type = "output_boundary",
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            cancel_requested = cancel.load(AtomicOrdering::Acquire),
            "Markdown结果落盘失败"
        );
    }
    result
}

/// T-12 保险丝 + 输出目录准备：目标已存在直接拒绝（不覆盖既有结果），逐段
/// 创建输出父目录并核对每一段都是普通目录（防符号链接/junction）。返回结果
/// 文件的父目录。
fn ensure_output_tree(
    output_root: &Path,
    target: &Path,
    cancel: &AtomicBool,
) -> Result<PathBuf, String> {
    jchtools_assets::asset_util::ensure_not_cancelled(cancel)?;
    let parent = target.parent().ok_or_else(|| "结果目录无效".to_string())?;
    // T-12 保险丝：扫描已跳过既有结果，这里目标再出现属并发/外部改动——
    // 在动任何 media 文件之前直接拒绝，避免「md 旧、图新」的错位组合。
    if fs::symlink_metadata(target).is_ok() {
        return Err("无法提交新结果（已有结果不会覆盖）：目标已存在".to_string());
    }
    let relative_parent = parent
        .strip_prefix(output_root)
        .map_err(|e| format!("结果不在输出目录内：{e}"))?;
    let mut current = output_root.to_path_buf();
    for component in relative_parent.components() {
        jchtools_assets::asset_util::ensure_not_cancelled(cancel)?;
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
    Ok(parent.to_path_buf())
}

/// T-14/T-29：把本次转换产出的媒体图片写入目标目录；每张图写入前检查取消
/// 与单文件预算（失败由调用方统一回滚），已写文件与新建目录分别记入
/// created / created_dirs 供回滚使用。
fn write_media_files(
    parent: &Path,
    media: &[markdown_document::MediaFile],
    deadline: &markdown_document::Deadline,
    cancel: &AtomicBool,
    created: &mut Vec<PathBuf>,
    created_dirs: &mut Vec<PathBuf>,
) -> Result<(), String> {
    for file in media {
        jchtools_assets::asset_util::ensure_not_cancelled(cancel)?;
        // T-29（S4-02）：预算耗尽后不再开始写下一张图（每张图写入前检查，
        // 等价于上一张图后的边界）；失败走既有回滚，不留半成品。
        if deadline_exhausted(deadline) {
            tracing::error!(
                event = "markdown_output_timeout",
                stage = "media_write",
                error_type = "timeout",
                "图片写入前预算耗尽"
            );
            return Err("单文件处理超时：写入图片前预算已耗尽，本次结果未提交".to_string());
        }
        let destination = prepare_media_destination(parent, &file.relative, created_dirs)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)
            .map_err(|e| {
                log_output_io(&e, "media_create");
                format!("无法创建图片 {}：{e}", destination.display())
            })?;
        created.push(destination.clone());
        output.write_all(&file.bytes).map_err(|e| {
            log_output_io(&e, "media_write");
            format!("无法写入图片 {}：{e}", destination.display())
        })?;
        output.sync_all().map_err(|e| {
            log_output_io(&e, "media_sync");
            format!("同步图片失败 {}：{e}", destination.display())
        })?;
    }
    Ok(())
}

/// T-25/T-12：temp 写入正文、预算检查后以不覆盖改名提交。temp 路径与残留
/// 清理留在调用方；`stage` 在紧贴改名的检查点推进为 "output_commit"，保证
/// 调用方失败日志的阶段字段与重构前一致。
fn commit_markdown(
    target: &Path,
    temp: &Path,
    content: &str,
    deadline: &markdown_document::Deadline,
    cancel: &AtomicBool,
    stage: &mut &'static str,
) -> Result<(), String> {
    let write_result = (|| -> Result<(), String> {
        jchtools_assets::asset_util::ensure_not_cancelled(cancel)?;
        // T-29（S4-02）：最终提交前同样受预算约束；检查点在 temp 写入与改名
        // 之前，超时走失败与回滚，不把慢盘上的半成品计成功。
        if deadline_exhausted(deadline) {
            tracing::error!(
                event = "markdown_output_timeout",
                stage = "markdown_write",
                error_type = "timeout",
                "正文写入前预算耗尽"
            );
            return Err("单文件处理超时：提交结果前预算已耗尽，本次结果未提交".to_string());
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(temp)
            .map_err(|e| {
                log_output_io(&e, "markdown_create");
                format!("无法创建临时结果：{e}")
            })?;
        file.write_all(content.as_bytes()).map_err(|e| {
            log_output_io(&e, "markdown_write");
            format!("写入 Markdown 失败：{e}")
        })?;
        file.sync_all().map_err(|e| {
            log_output_io(&e, "markdown_sync");
            format!("同步 Markdown 失败：{e}")
        })?;
        drop(file);
        jchtools_assets::asset_util::ensure_not_cancelled(cancel)?;
        // T-29（S4-02）：检查点紧贴改名提交——慢盘写正文+sync 耗尽预算时不得
        // 把结果改名为可见产物（检查点放在 rename 之后无法回滚对外可见状态）。
        if deadline_exhausted(deadline) {
            tracing::error!(
                event = "markdown_output_timeout",
                stage = "output_commit",
                error_type = "timeout",
                "结果提交前预算耗尽"
            );
            return Err("单文件处理超时：提交前预算已耗尽，本次结果未提交".to_string());
        }
        // T-25/T-12：完整成功后以不覆盖改名提交（temp 与目标同目录同卷）。
        // 不用 fs::rename——Windows 上它会替换已存在目标；不用硬链接——
        // exFAT/FAT 等文件系统不支持，会把输出在这些卷上的结果全部判失败。
        *stage = "output_commit";
        fsutil::rename_noreplace(temp, target)
            .map_err(|e| format!("无法提交新结果（已有结果不会覆盖）：{e:#}"))?;
        Ok(())
    })();
    write_result
}

/// 回滚本次未提交的输出（T-25 不留半成品）：先倒序删除本次已写的图片，再
/// 倒序移除本次创建的媒体目录；NotFound 一律静默，不触碰既有文件。
fn rollback_uncommitted(created: &[PathBuf], created_dirs: &[PathBuf]) {
    for path in created.iter().rev() {
        log_output_cleanup(fs::remove_file(path), "media_rollback");
    }
    for path in created_dirs.iter().rev() {
        log_output_cleanup(fs::remove_dir(path), "media_directory_rollback");
    }
}

#[doc(hidden)]
pub fn test_write_new_markdown(
    output_root: &Path,
    target: &Path,
    content: &str,
    media: &[markdown_document::MediaFile],
) -> Result<(), String> {
    // 既有接缝签名不变：内部按无界预算驱动（Duration::MAX 经 checked_add 溢出
    // 退化为不限时），专注 T-12/T-14/T-25 语义；超时约束由带 deadline 的新
    // 用例经 `write_new_markdown` 直接覆盖。
    write_new_markdown(
        output_root,
        target,
        content,
        media,
        &markdown_document::Deadline::new(Duration::MAX),
    )
}
