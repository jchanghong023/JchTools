use super::download::NetworkDownloader;
use super::manifest::{load_manifest, write_notice, SnapAsset, SnapAssetManifest};
use super::readiness::{
    asset_ready, asset_root, readiness, readiness_inference_pack, worker_readiness,
};
use crate::asset_util::{
    atomic_replace_dir, atomic_replace_file, cleanup_stale_staging_dirs, ensure_not_cancelled,
    extract_zip_safely, finalize_staging, verify_file_with_cancel, AssetDownloader,
};
use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use uuid::Uuid;

/// 下载、校验并原子安装全部可选资产（O-06）。
///
/// 取消会终止当前下载或解包阶段并删除本轮 staging 目录；最终位置已校验的资产
/// 跨重试复用、不重下；失败不覆盖已经校验的完整资产。下载接缝与生产下载器
/// 共用 `crate::asset_util::AssetDownloader` / `crate::asset_util::NetworkDownloader`，
/// 测试可注入本地供给验证「补缺下载」与「失败后重试不重下」。
pub fn initialize(cancel: &AtomicBool, mut progress: impl FnMut(String)) -> Result<(), String> {
    let span = jchtools_foundation::logging::operation_span("snap_ocr_assets", "initialize");
    let _entered = span.enter();
    let started = std::time::Instant::now();
    tracing::info!(
        event = "asset_initialize_started",
        "截图 OCR 组件初始化开始"
    );
    let result = initialize_task(cancel, &mut progress);
    let elapsed_ms = jchtools_foundation::logging::elapsed_ms(started);
    match &result {
        Ok(()) => tracing::info!(
            event = "asset_initialize_completed",
            elapsed_ms,
            result = "ready",
            "截图 OCR 组件初始化完成"
        ),
        Err(reason) if reason.contains("取消") => {
            tracing::info!(
                event = "asset_initialize_cancelled",
                elapsed_ms,
                result = "cancelled",
                "截图 OCR 组件初始化已取消"
            );
        }
        Err(_) => tracing::error!(
            event = "asset_initialize_failed",
            elapsed_ms,
            result = "failed",
            "截图 OCR 组件初始化失败"
        ),
    }
    result
}

fn initialize_task(cancel: &AtomicBool, progress: &mut dyn FnMut(String)) -> Result<(), String> {
    let mut stage = "prepare";
    let result = (|| {
        let root = asset_root();
        // B-2：先兜底清理历史残留的 staging（readiness 提前返回、取消后清理
        // 失败或进程崩溃都会残留 .staging-<uuid>，download.zip 残留可达约 291MB）。
        cleanup_stale_staging(&root);
        if readiness().is_ok() {
            tracing::debug!(
                event = "asset_initialize_reused",
                result = "already_ready",
                "截图 OCR 资产已就绪，无需初始化"
            );
            progress("截图 OCR 组件已就绪".to_string());
            return Ok(());
        }
        ensure_not_cancelled(cancel)?;
        stage = "manifest";
        let manifest = load_manifest()?;
        stage = "create_root";
        fs::create_dir_all(&root).map_err(|error| format!("创建资产目录失败：{error}"))?;
        let staging = root.join(format!(".staging-{}", Uuid::new_v4().simple()));
        stage = "create_staging";
        fs::create_dir_all(&staging).map_err(|error| format!("创建临时目录失败：{error}"))?;
        let mut downloader = NetworkDownloader;
        stage = "initialize_staged";
        let result = initialize_staged(
            &manifest,
            cancel,
            progress,
            &staging,
            &root,
            &mut downloader,
        );
        stage = "finalize_staging";
        finalize_staging(result, &staging, progress)
    })();
    if result.is_err() {
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            tracing::info!(
                event = "asset_initialize_stage_cancelled",
                stage,
                "截图 OCR 初始化阶段已取消"
            );
        } else {
            tracing::error!(
                event = "asset_initialize_stage_failed",
                stage,
                "截图 OCR 初始化阶段未完成"
            );
        }
    }
    result
}

/// 兜底清理资产根下历史残留的 `.staging-*` 目录（B-2，`.staging-*` 清扫与
/// markdown 侧共用 [`crate::asset_util::cleanup_stale_staging_dirs`]），
/// 并兼容清扫旧版 tag 标记写入流程在 `xberg-inference/` 下残留的
/// `.expected-tag-*` 临时文件；当前版本不再写入该标记，单项失败跳过继续。
///
/// 不扫 `.old-*` 备份：那是原子替换路径的暂存（asset_util），其中
/// xberg-inference/<tag> 目标的残留按设计由 prune_old_inference_tags 收集
///（该安装链当前仅在测试中启用，见 asset_util；生产初始化只校验共享目录，
/// XB-10），入口一概删除会把替换失败后仍可恢复的备份提前清掉。
fn cleanup_stale_staging(root: &Path) {
    cleanup_stale_staging_dirs(root);
    // 仅清扫旧版写入流程在失败或进程崩溃后留下的临时标记；当前版本没有
    // expected-tag writer，也不依据该标记拒绝共享组件版本。
    cleanup_expected_tag_residue(&root.join("xberg-inference"));
}

/// 兼容清扫旧版在 `xberg-inference/` 顶层遗留的 `.expected-tag-*` 临时文件
/// （非目录，按类型删除）；目录不存在时无需清理，单项失败跳过。
fn cleanup_expected_tag_residue(base: &Path) {
    let entries = match fs::read_dir(base) {
        Ok(entries) => entries,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(event = "asset_residue_cleanup_failed", stage = "read_directory", error_kind = ?error.kind(), error_code = error.raw_os_error(), "读取推理组件临时标记目录失败");
            }
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(event = "asset_residue_cleanup_failed", stage = "read_entry", error_kind = ?error.kind(), error_code = error.raw_os_error(), "读取推理组件临时标记条目失败，跳过此项");
                continue;
            }
        };
        let path = entry.path();
        let is_temp = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".expected-tag-"));
        if is_temp && path.is_file() {
            // 尽力而为：残留被防护软件短暂锁定时跳过，下次初始化再试。
            if let Err(error) = fs::remove_file(&path) {
                tracing::warn!(event = "asset_residue_cleanup_failed", stage = "remove_expected_tag", error_kind = ?error.kind(), error_code = error.raw_os_error(), "删除推理组件临时标记失败，留待下次重试");
            }
        }
    }
}

pub(super) fn initialize_staged(
    manifest: &SnapAssetManifest,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(String),
    staging: &Path,
    root: &Path,
    downloader: &mut dyn AssetDownloader,
) -> Result<(), String> {
    let started = std::time::Instant::now();
    let mut stage = "assets";
    let result = (|| {
        let total = manifest.assets.len();
        for (index, asset) in manifest.assets.iter().enumerate() {
            ensure_not_cancelled(cancel)?;
            if asset.is_embedded() {
                tracing::debug!(
                    event = "asset_reused",
                    asset_index = index,
                    result = "embedded",
                    "使用随包内置资产"
                );
                progress(format!("使用随包内置资产：{}", asset.id));
                continue;
            }
            // O-06 跨重试复用：最终位置已校验的资产不重下（保留已验证下载）。
            if asset_ready(asset, root).is_ok() {
                tracing::debug!(
                    event = "asset_reused",
                    asset_index = index,
                    result = "verified_existing",
                    "复用已校验资产"
                );
                progress(format!("复用已校验资产：{}", asset.id));
                continue;
            }
            progress(format!("下载资产 {}/{}：{}", index + 1, total, asset.id));
            install_asset(asset, staging, root, cancel, downloader, progress)?;
        }
        // XB-10：只校验共享目录，不下载、复制或改写用户的 Xberg。
        stage = "inference_readiness";
        readiness_inference_pack(root)?;
        stage = "worker_manifest";
        let worker = manifest
            .workers
            .first()
            .ok_or_else(|| "截图 OCR 工作进程清单缺失".to_string())?;
        if worker.is_pending() {
            // 诚实收尾：模型等真实资产已按清单安装，但整体不得标就绪（O-06 半成品
            // 不标就绪）；重试在打包回填后直接补 worker，其余资产全部复用。
            return Err(format!(
            "截图 OCR 工作进程尚未发布（清单条目 {} 为构建期占位）；模型、字体等资产已安装并复用，打包阶段回填后重试即可",
            worker.id
        ));
        }
        ensure_not_cancelled(cancel)?;
        stage = "worker_readiness";
        worker_readiness(worker, root)?;
        progress("复用已校验的工作进程".to_string());
        let notice_stage = staging.join("licenses");
        stage = "notice_create";
        fs::create_dir_all(&notice_stage)
            .map_err(|error| format!("创建许可证目录失败：{error}"))?;
        stage = "notice_write";
        write_notice(&notice_stage.join("THIRD_PARTY_NOTICES.md"), manifest)?;
        // O-06：许可证写入期间仍可取消；只在原子提交前检查，提交后不假报取消。
        ensure_not_cancelled(cancel)?;
        stage = "notice_install";
        atomic_replace_dir(&notice_stage, &root.join("licenses"))?;
        stage = "final_readiness";
        readiness()?;
        progress("截图 OCR 组件初始化完成".to_string());
        Ok(())
    })();
    match &result {
        Ok(()) => tracing::debug!(
            event = "asset_initialize_staged_completed",
            asset_count = manifest.assets.len(),
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "截图 OCR 暂存资产初始化完成"
        ),
        Err(_) if cancel.load(std::sync::atomic::Ordering::Acquire) => tracing::info!(
            event = "asset_initialize_staged_cancelled",
            stage,
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "截图 OCR 暂存资产初始化已取消"
        ),
        Err(_) => tracing::error!(
            event = "asset_initialize_staged_failed",
            stage,
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "截图 OCR 暂存资产初始化未完成"
        ),
    }
    result
}

/// 下载单个资产（文件或归档）并按清单落位：先在 staging 校验，再原子替换到最终
/// 位置；最终位置已有校验通过的副本时不覆盖（失败安装不得动已验证资产）。
fn install_asset(
    asset: &SnapAsset,
    staging: &Path,
    root: &Path,
    cancel: &AtomicBool,
    downloader: &mut dyn AssetDownloader,
    progress: &mut dyn FnMut(String),
) -> Result<(), String> {
    let span = jchtools_foundation::logging::operation_span("snap_ocr_assets", "install_asset");
    let _entered = span.enter();
    let started = std::time::Instant::now();
    tracing::debug!(
        event = "asset_install_started",
        expected_size = asset.size_bytes,
        member_count = asset.members.len(),
        "资产校验安装开始"
    );
    let mut stage = "create_staging";
    let result = (|| {
        let stage_dir = staging.join(&asset.id);
        fs::create_dir_all(&stage_dir).map_err(|error| format!("创建资产临时目录失败：{error}"))?;
        let archive = stage_dir.join("download.bin");
        stage = "download";
        downloader.download(
            &asset.url,
            &archive,
            asset.size_bytes,
            &asset.sha256,
            cancel,
            progress,
        )?;
        ensure_not_cancelled(cancel)?;
        // 下载器校验之外独立复核暂存内容，防伪造的“下载成功”。
        stage = "archive_verify";
        let verify_started = std::time::Instant::now();
        tracing::debug!(
            event = "asset_archive_verify_started",
            size_bytes = asset.size_bytes,
            "资产暂存内容独立复核开始"
        );
        verify_file_with_cancel(&archive, asset.size_bytes, &asset.sha256, cancel)
            .map_err(|error| format!("资产 {} 下载内容校验失败：{error}", asset.id))?;
        tracing::debug!(
            event = "asset_archive_verify_completed",
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(verify_started),
            result = "verified",
            "资产暂存内容独立复核通过"
        );
        match asset.archive_type.as_str() {
            "file" => {
                stage = "file_install_path";
                let install_path = asset
                    .install_path
                    .as_ref()
                    .ok_or_else(|| format!("文件资产 {} 缺少安装路径", asset.id))?;
                let target = root.join(install_path);
                stage = "target_verify";
                if verify_file_with_cancel(&target, asset.size_bytes, &asset.sha256, cancel)
                    .is_err()
                {
                    ensure_not_cancelled(cancel)?;
                    stage = "file_install";
                    let install_started = std::time::Instant::now();
                    tracing::debug!(
                        event = "asset_file_install_started",
                        stage,
                        "资产文件原子落位开始"
                    );
                    atomic_replace_file(&archive, &target)?;
                    tracing::debug!(
                        event = "asset_file_install_completed",
                        stage,
                        elapsed_ms = jchtools_foundation::logging::elapsed_ms(install_started),
                        "资产文件原子落位完成"
                    );
                }
                Ok(())
            }
            "zip" => {
                let extracted = stage_dir.join("extracted");
                stage = "extract_directory";
                fs::create_dir_all(&extracted)
                    .map_err(|error| format!("创建解包目录失败：{error}"))?;
                stage = "extract_archive";
                let extract_started = std::time::Instant::now();
                tracing::debug!(
                    event = "asset_extract_started",
                    member_count = asset.members.len(),
                    "资产归档解包开始"
                );
                extract_zip_safely(&archive, &extracted, cancel)?;
                tracing::debug!(
                    event = "asset_extract_completed",
                    elapsed_ms = jchtools_foundation::logging::elapsed_ms(extract_started),
                    "资产归档解包完成"
                );
                stage = "member_manifest";
                if asset.members.is_empty() {
                    return Err(format!("归档资产 {} 没有成员清单", asset.id));
                }
                for (member_index, member) in asset.members.iter().enumerate() {
                    let source = extracted.join(&member.path);
                    stage = "member_verify";
                    let member_started = std::time::Instant::now();
                    tracing::debug!(
                        event = "asset_member_verify_started",
                        member_index,
                        size_bytes = member.size_bytes,
                        "资产成员校验开始"
                    );
                    verify_file_with_cancel(&source, member.size_bytes, &member.sha256, cancel)
                        .map_err(|error| {
                            format!("资产 {} 成员 {} 校验失败：{error}", asset.id, member.path)
                        })?;
                    tracing::debug!(
                        event = "asset_member_verify_completed",
                        member_index,
                        elapsed_ms = jchtools_foundation::logging::elapsed_ms(member_started),
                        "资产成员校验通过"
                    );
                    let target = root.join(&member.install_path);
                    stage = "target_verify";
                    if verify_file_with_cancel(&target, member.size_bytes, &member.sha256, cancel)
                        .is_err()
                    {
                        ensure_not_cancelled(cancel)?;
                        stage = "member_install";
                        let install_started = std::time::Instant::now();
                        tracing::debug!(
                            event = "asset_file_install_started",
                            stage,
                            member_index,
                            "资产成员原子落位开始"
                        );
                        atomic_replace_file(&source, &target)?;
                        tracing::debug!(
                            event = "asset_file_install_completed",
                            stage,
                            member_index,
                            elapsed_ms = jchtools_foundation::logging::elapsed_ms(install_started),
                            "资产成员原子落位完成"
                        );
                    }
                }
                Ok(())
            }
            other => {
                stage = "archive_type";
                Err(format!("不支持的资产归档格式：{other}"))
            }
        }
    })();
    let elapsed_ms = jchtools_foundation::logging::elapsed_ms(started);
    match &result {
        Ok(()) => tracing::debug!(
            event = "asset_install_completed",
            elapsed_ms,
            result = "verified_installed",
            "资产校验安装完成"
        ),
        Err(_) if cancel.load(std::sync::atomic::Ordering::Acquire) => tracing::info!(
            event = "asset_install_cancelled",
            stage,
            elapsed_ms,
            "资产校验安装已取消"
        ),
        Err(_) => tracing::error!(
            event = "asset_install_failed",
            stage,
            elapsed_ms,
            "资产校验安装失败"
        ),
    }
    result
}
