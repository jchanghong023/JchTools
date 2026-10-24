//! Xberg 推理组件包安装核心。
//!
//! 推理组件的清单模型（InferenceManifest/InferenceMember）、下载接缝
//! （AssetDownloader）与「下载 → 校验 → 解包 → staging 内成员级复核 →
//! 整目录原子落位 → 清理旧版本」安装链（XB-09/XB-10）的唯一实现，截图 OCR
//! 与转 Markdown 共用同一语义。安装链当前仅在测试启用（XB-10：生产初始化
//! 只校验共享目录），相关函数与仅被其消费的导入均带
//! `#[cfg(any(test, feature = "test-hooks"))]` 门。

use serde::Deserialize;
use std::path::Path;
use std::sync::atomic::AtomicBool;

#[cfg(any(test, feature = "test-hooks"))]
use super::verify::verify_file;
#[cfg(any(test, feature = "test-hooks"))]
use super::{atomic_replace_dir, ensure_not_cancelled, extract_zip_safely};
#[cfg(any(test, feature = "test-hooks"))]
use std::fs;
#[cfg(any(test, feature = "test-hooks"))]
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub struct InferenceManifest {
    pub tag: String,
    pub url: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub members: Vec<InferenceMember>,
}

#[derive(Debug, Deserialize)]
pub struct InferenceMember {
    pub path: String,
    pub install_path: String,
    // 摘要与大小只被下载安装链（install_inference_pack / inference_layout_ready，
    // 按 XB-10 当前仅在测试启用）消费；运行时存在性检查（XB-09 2026-10-02
    // 修订）不读取，非测试构建因此允许未读。
    // [quality-baseline approved 2026-10-03] 测试门控消费字段，经用户裁定保留
    #[cfg_attr(not(test), allow(dead_code))]
    pub size_bytes: u64,
    // [quality-baseline approved 2026-10-03] 同上：测试门控消费字段
    #[cfg_attr(not(test), allow(dead_code))]
    pub sha256: String,
}

/// 单个资产的下载接缝：生产实现走固定来源的 HTTP 下载（实现位于
/// `snap_ocr_assets`，P-03 联网边界测试按文件白名单执法），测试注入本地
/// 供给或失败脚本（T-05/T-21、O-06 语义）。
pub trait AssetDownloader {
    fn download(
        &mut self,
        url: &str,
        destination: &Path,
        expected_size: u64,
        expected_sha256: &str,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(String),
    ) -> Result<(), String>;
}

/// 按可选的清单 tag 解析唯一组件目录（XB-09）：
/// - 无 tag（清单未接入）：唯一子目录即组件目录；
/// - 有 tag：目录名必须与清单一致，不一致明确报错并指引更新；
/// - 零个或多个候选都视为无法确定，不静默选择。
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
#[cfg(any(test, feature = "test-hooks"))]
pub fn resolve_component_with_tag(
    base: &Path,
    expected_tag: Option<&str>,
    missing_message: &str,
) -> Result<PathBuf, String> {
    let entries = fs::read_dir(base).map_err(|_| missing_message.to_string())?;
    let mut versions = Vec::new();
    for entry in entries.flatten() {
        if entry.path().is_dir() {
            versions.push(entry.path());
        }
    }
    match (versions.len(), expected_tag) {
        (0, _) => Err(missing_message.to_string()),
        (1, None) => Ok(versions.remove(0)),
        (1, Some(tag)) => {
            let component = versions.remove(0);
            if component.file_name().and_then(|name| name.to_str()) == Some(tag) {
                Ok(component)
            } else {
                Err(format!(
                    "推理组件版本与清单不一致（安装 {}，清单要求 {tag}）；请在对应功能页重新初始化以更新组件",
                    component.display()
                ))
            }
        }
        // 多目录 + 有清单 tag：优先选中清单 tag 目录（一次瞬时清理失败留下的
        // 旧版本目录不应让组件不可用；下次初始化会再尝试清理）。
        (_, Some(tag)) => match versions
            .iter()
            .position(|path| path.file_name().and_then(|name| name.to_str()) == Some(tag))
        {
            Some(index) => Ok(versions.swap_remove(index)),
            None => Err(format!(
                "推理组件目录存在多个版本且无清单要求的 {tag}；请重新初始化以更新组件"
            )),
        },
        (_, None) => {
            Err("推理组件目录存在多个版本，无法确定使用哪一个；请只保留一个版本目录".into())
        }
    }
}

/// 下载、校验并原子安装推理组件包（XB-09/XB-10）。
///
/// 最终位置成员全部校验通过时不重下（跨重试复用）；下载与解包在 staging 内
/// 完成并二次校验（归档摘要 + 每成员摘要），全部通过后整目录原子落位到
/// `xberg-inference/<tag>/`，随后移除其他版本目录（不混用版本）。取消或
/// 失败不会触碰已验证的安装。
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
#[cfg(any(test, feature = "test-hooks"))]
pub fn install_inference_pack(
    inference: &InferenceManifest,
    staging: &Path,
    root: &Path,
    cancel: &AtomicBool,
    downloader: &mut dyn AssetDownloader,
    progress: &mut impl FnMut(String),
) -> Result<(), String> {
    if inference_ready(inference, root).is_ok() {
        progress("复用已校验的推理组件".to_string());
        // 旧版本目录的清理是尽力而为：清单 tag 目录可正常解析与使用，清理失败
        // （如目录被占用）只提示，不让「已可用」的安装报失败。
        if let Err(error) = prune_old_inference_tags(root, &inference.tag) {
            progress(format!("警告：{error}"));
        }
        return Ok(());
    }
    ensure_not_cancelled(cancel)?;
    progress(format!(
        "下载 Xberg 推理组件包（{}，{} 字节）",
        inference.tag, inference.size_bytes
    ));
    let stage_dir = staging.join("xberg-inference");
    fs::create_dir_all(&stage_dir).map_err(|error| format!("创建推理组件临时目录失败：{error}"))?;
    let archive = stage_dir.join("download.zip");
    downloader.download(
        &inference.url,
        &archive,
        inference.size_bytes,
        &inference.sha256,
        cancel,
        progress,
    )?;
    ensure_not_cancelled(cancel)?;
    // 下载器校验之外独立复核暂存内容，防伪造的“下载成功”。
    verify_file(&archive, inference.size_bytes, &inference.sha256)
        .map_err(|error| format!("推理组件包下载内容校验失败：{error}"))?;
    let extracted = stage_dir.join("extracted");
    fs::create_dir_all(&extracted).map_err(|error| format!("创建推理组件解包目录失败：{error}"))?;
    extract_zip_safely(&archive, &extracted, cancel)?;
    let staged_component = stage_dir.join("component");
    for member in &inference.members {
        ensure_not_cancelled(cancel)?;
        let source = extracted.join(&member.path);
        verify_file(&source, member.size_bytes, &member.sha256)
            .map_err(|error| format!("推理组件成员 {} 校验失败：{error}", member.path))?;
        let target = staged_component.join(&member.install_path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("创建推理组件成员目录失败：{error}"))?;
        }
        fs::copy(&source, &target)
            .map_err(|error| format!("落位推理组件成员 {} 失败：{error}", member.install_path))?;
    }
    // staging 内自校验通过后整目录原子落位，失败恢复旧目录。
    inference_layout_ready(&staged_component, inference)?;
    atomic_replace_dir(
        &staged_component,
        &root.join("xberg-inference").join(&inference.tag),
    )?;
    // 落位成功后清理旧版本目录：尽力而为（清单 tag 目录已可解析使用，清理失败
    // 只提示，不把成功的安装报成失败）。
    if let Err(error) = prune_old_inference_tags(root, &inference.tag) {
        progress(format!("警告：{error}"));
    }
    Ok(())
}

/// 清单接入后推理组件的成员级摘要校验（XB-09）。
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
#[cfg(any(test, feature = "test-hooks"))]
pub fn inference_ready(inference: &InferenceManifest, root: &Path) -> Result<(), String> {
    inference_layout_ready(
        &root.join("xberg-inference").join(&inference.tag),
        inference,
    )
}

/// 落位前对 staging 组件树做成员级复核（存在 + 摘要），确保原子替换进来的
/// 目录就是清单声明的完整安装。就绪检查（C-2）对解析出的组件目录复用
/// 同一口径。
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
#[cfg(any(test, feature = "test-hooks"))]
pub(crate) fn inference_layout_ready(
    component: &Path,
    inference: &InferenceManifest,
) -> Result<(), String> {
    for member in &inference.members {
        verify_file(
            &component.join(&member.install_path),
            member.size_bytes,
            &member.sha256,
        )
        .map_err(|error| format!("推理组件成员 {}：{error}", member.install_path))?;
    }
    Ok(())
}

/// 成功安装清单 tag 后移除其他版本目录：同一安装只保留一个版本（XB-09）。
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
#[cfg(any(test, feature = "test-hooks"))]
pub(crate) fn prune_old_inference_tags(root: &Path, keep: &str) -> Result<(), String> {
    let base = root.join("xberg-inference");
    let entries = fs::read_dir(&base).map_err(|error| format!("枚举推理组件目录失败：{error}"))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && path.file_name().and_then(|name| name.to_str()) != Some(keep) {
            fs::remove_dir_all(&path).map_err(|error| {
                format!("移除旧版本推理组件失败（{}）：{error}", path.display())
            })?;
        }
    }
    Ok(())
}
