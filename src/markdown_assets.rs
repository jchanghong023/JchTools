//! 转 Markdown 的可选本地资产初始化。
//!
//! 该模块校验用户选择的 Xberg 运行时，并管理用户主动初始化后才需要的媒体资产。
//! 资产清单编译进主程序，但二进制、模型和下载缓存始终落在独立的
//! JchTools 用户数据目录中，不依赖旧 all2markdown 目录。

use bzip2::read::BzDecoder;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt::Write as FmtWrite;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tar::Archive;
use uuid::Uuid;
use zip::ZipArchive;

const MANIFEST: &str = include_str!("../resources/markdown-assets.json");
const DATA_DIRECTORY: &str = "markdown-assets";
const XBERG_TAG: &str = "v2026.9.15-0746-run36.1";
const RUNTIME_SELECTION_FILE: &str = "xberg-runtime-path.txt";

#[derive(Debug, Deserialize)]
struct AssetManifest {
    schema_version: u32,
    xberg: XbergManifest,
    media_models: Vec<MediaModel>,
    future_workers: Vec<FutureWorker>,
}

#[derive(Debug, Deserialize)]
struct XbergManifest {
    tag: String,
    archive_url: String,
    archive_size_bytes: u64,
    archive_sha256: String,
    members: Vec<AssetFile>,
    licenses: Vec<LicenseEntry>,
}

#[derive(Debug, Deserialize)]
struct MediaModel {
    id: String,
    url: String,
    relative_path: String,
    size_bytes: u64,
    sha256: String,
    license: LicenseEntry,
}

#[derive(Debug, Deserialize)]
struct AssetFile {
    path: String,
    size_bytes: u64,
    sha256: String,
}

#[derive(Debug, Deserialize)]
struct LicenseEntry {
    component: String,
    license: String,
    source: String,
}

#[derive(Debug, Deserialize)]
struct FutureWorker {
    id: String,
    status: String,
    blocking_reason: String,
    assets: Vec<WorkerAsset>,
}

#[derive(Debug, Deserialize)]
struct WorkerAsset {
    id: String,
    url: String,
    size_bytes: u64,
    sha256: String,
    archive_type: String,
    target_root: String,
    install_path: Option<String>,
    members: Vec<WorkerMember>,
    license: LicenseEntry,
}

#[derive(Debug, Deserialize)]
struct WorkerMember {
    path: String,
    install_path: String,
    size_bytes: u64,
    sha256: String,
}

/// 读取本功能独立保存的 Xberg 运行目录。
pub fn load_saved_runtime_dir() -> Result<Option<PathBuf>, String> {
    let selection = asset_root().join(RUNTIME_SELECTION_FILE);
    match fs::read_to_string(&selection) {
        Ok(value) => {
            let value = value.trim();
            if value.is_empty() {
                return Err(format!("Xberg 运行目录状态为空：{}", selection.display()));
            }
            Ok(Some(PathBuf::from(value)))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("读取 Xberg 运行目录状态失败：{error}")),
    }
}

/// 校验用户选择的 Xberg 运行目录及其固定版本全部成员。
pub fn validate_runtime_dir(path: &Path) -> Result<(), String> {
    let manifest = load_manifest()?;
    if !path.is_dir() {
        return Err(format!(
            "Xberg 运行目录不存在或不是目录：{}",
            path.display()
        ));
    }
    let mut failures = Vec::new();
    for member in manifest
        .xberg
        .members
        .iter()
        .filter(|member| member.path == "xberg.exe")
        .chain(
            manifest
                .xberg
                .members
                .iter()
                .filter(|member| member.path != "xberg.exe"),
        )
    {
        if let Err(error) = verify_file(&path.join(&member.path), member.size_bytes, &member.sha256)
        {
            failures.push(format!("{}：{error}", member.path));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Xberg 运行目录校验失败（{} 项）：{}",
            failures.len(),
            failures.join("；")
        ))
    }
}

/// 完整校验后保存用户选择的 Xberg 运行目录。
pub fn save_runtime_dir(path: &Path) -> Result<(), String> {
    validate_runtime_dir(path)?;
    let canonical =
        fs::canonicalize(path).map_err(|error| format!("解析 Xberg 运行目录失败：{error}"))?;
    let root = asset_root();
    fs::create_dir_all(&root).map_err(|error| format!("创建资产状态目录失败：{error}"))?;
    let selection = root.join(RUNTIME_SELECTION_FILE);
    let temporary = root.join(format!(
        ".{RUNTIME_SELECTION_FILE}.{}",
        Uuid::new_v4().simple()
    ));
    fs::write(&temporary, format!("{}\n", canonical.display()))
        .map_err(|error| format!("保存 Xberg 运行目录失败：{error}"))?;
    atomic_replace_file(&temporary, &selection)
        .map_err(|error| format!("原子保存 Xberg 运行目录失败：{error}"))?;
    Ok(())
}

/// 返回已保存的 Xberg 运行目录；未选择时返回明确错误。
pub fn runtime_dir() -> Result<PathBuf, String> {
    load_saved_runtime_dir()?.ok_or_else(|| "尚未选择 Xberg 运行目录".to_string())
}

/// 返回可选媒体模型的独立安装目录。
pub fn media_models_dir() -> PathBuf {
    asset_root()
        .join("media")
        .join("sherpa-onnx")
        .join("v1.13.6")
}

/// 返回可选媒体工作进程的固定路径。
pub fn media_worker_path() -> PathBuf {
    asset_root()
        .join("worker")
        .join("v0.1.0")
        .join("markdown-media-worker.exe")
}

/// 只读检查所有已安装资产。该函数不会联网、创建目录或修改文件。
pub fn readiness() -> Result<(), String> {
    let manifest = load_manifest()?;
    let runtime = runtime_dir()?;
    validate_runtime_dir(&runtime)?;
    ensure_worker_ready(&manifest)?;
    ensure_media_models_ready(&manifest)?;
    let notice = asset_root().join("licenses").join("THIRD_PARTY_NOTICES.md");
    if !notice.is_file() {
        return Err(format!("许可证 notice 不存在：{}", notice.display()));
    }
    Ok(())
}

/// 单个资产的下载接缝：生产实现走固定来源的 HTTP 下载，测试注入本地供给或
/// 失败脚本，用于验证「补缺下载」与「失败后重试不重下」语义（T-05）。
trait AssetDownloader {
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

/// 生产下载器：只访问清单固定地址（T-21）。
struct NetworkDownloader;

impl AssetDownloader for NetworkDownloader {
    fn download(
        &mut self,
        url: &str,
        destination: &Path,
        expected_size: u64,
        expected_sha256: &str,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(String),
    ) -> Result<(), String> {
        let mut sink = |message: String| progress(message);
        download_asset(
            url,
            destination,
            expected_size,
            expected_sha256,
            cancel,
            &mut sink,
        )
    }
}

/// 下载、校验并原子安装全部可选资产。
///
/// 初始化期间只访问清单中的固定地址。取消会终止当前下载或解包阶段，
/// 并删除本轮 staging 目录；已经存在的可用安装不会被覆盖。
pub fn initialize(cancel: &AtomicBool, mut progress: impl FnMut(String)) -> Result<(), String> {
    if readiness().is_ok() {
        progress("转 Markdown 组件已就绪".to_string());
        return Ok(());
    }
    ensure_not_cancelled(cancel)?;
    let manifest = load_manifest()?;
    let root = asset_root();
    fs::create_dir_all(&root).map_err(|error| format!("创建资产目录失败：{error}"))?;
    let staging = root.join(format!(".staging-{}", Uuid::new_v4().simple()));
    fs::create_dir_all(&staging).map_err(|error| format!("创建临时目录失败：{error}"))?;
    let mut downloader = NetworkDownloader;
    let result = initialize_staged(
        &manifest,
        cancel,
        &mut progress,
        &staging,
        &root,
        &mut downloader,
    );
    if let Err(error) = fs::remove_dir_all(&staging) {
        if result.is_ok() {
            return Err(format!("清理初始化临时目录失败：{error}"));
        }
    }
    result
}

fn initialize_staged(
    manifest: &AssetManifest,
    cancel: &AtomicBool,
    progress: &mut impl FnMut(String),
    staging: &Path,
    root: &Path,
    downloader: &mut dyn AssetDownloader,
) -> Result<(), String> {
    if media_models_ready(manifest) {
        progress("复用已校验的媒体模型".to_string());
    } else {
        for (index, model) in manifest.media_models.iter().enumerate() {
            ensure_not_cancelled(cancel)?;
            // T-05 逐资产复用：最终位置已校验的模型不重下（跨重试保留已验证下载）。
            let final_path = media_models_dir().join(&model.relative_path);
            if verify_file(&final_path, model.size_bytes, &model.sha256).is_ok() {
                progress(format!("复用已校验的媒体模型：{}", model.id));
                continue;
            }
            progress(format!(
                "下载媒体模型 {} / {}：{}",
                index + 1,
                manifest.media_models.len(),
                model.id
            ));
            let model_relative = Path::new(&model.relative_path)
                .strip_prefix("models")
                .map_err(|_| format!("媒体模型 {} 必须安装在 models 子目录", model.id))?;
            let staged = staging.join("media-models").join(model_relative);
            if let Some(parent) = staged.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("创建媒体模型目录失败：{error}"))?;
            }
            downloader.download(
                &model.url,
                &staged,
                model.size_bytes,
                &model.sha256,
                cancel,
                progress,
            )?;
            // 先校验暂存内容再落位；下载器校验之外再独立复核，防伪造的"下载成功"。
            verify_file(&staged, model.size_bytes, &model.sha256)
                .map_err(|error| format!("媒体模型 {} 下载内容校验失败：{error}", model.id))?;
            // T-06：最终位置已有校验通过的副本时不覆盖（失败安装不得动已验证资产）。
            if verify_file(&final_path, model.size_bytes, &model.sha256).is_err() {
                atomic_replace_file(&staged, &final_path)?;
            }
        }
    }

    let notice_stage = staging.join("licenses");
    fs::create_dir_all(&notice_stage).map_err(|error| format!("创建许可证目录失败：{error}"))?;
    write_notice(&notice_stage.join("THIRD_PARTY_NOTICES.md"), manifest)?;
    atomic_replace_dir(&notice_stage, &root.join("licenses"))?;
    initialize_worker_assets(manifest, cancel, progress, staging, downloader)?;
    if let Err(error) = ensure_worker_ready(manifest) {
        progress("媒体资产已处理，但媒体工作进程仍未就绪".to_string());
        return Err(error);
    }
    progress("转 Markdown 组件初始化完成".to_string());
    Ok(())
}

fn load_manifest() -> Result<AssetManifest, String> {
    let manifest: AssetManifest = serde_json::from_str(MANIFEST)
        .map_err(|error| format!("转 Markdown 资产清单无效：{error}"))?;
    if manifest.schema_version != 1 {
        return Err(format!("不支持的资产清单版本：{}", manifest.schema_version));
    }
    if manifest.xberg.tag != XBERG_TAG {
        return Err(format!(
            "Xberg 版本与代码固定版本不一致：{}",
            manifest.xberg.tag
        ));
    }
    if manifest.xberg.archive_url.is_empty()
        || manifest.xberg.archive_size_bytes == 0
        || manifest.xberg.archive_sha256.len() != 64
    {
        return Err("Xberg 固定版本归档元数据不完整".to_string());
    }
    if manifest.media_models.len() != 3 {
        return Err("媒体模型清单必须包含三份固定资产".to_string());
    }
    for model in &manifest.media_models {
        if model.relative_path.is_empty() || model.url.is_empty() {
            return Err(format!("媒体模型 {} 的路径或来源为空", model.id));
        }
        validate_relative_path(&model.relative_path)?;
    }
    for member in &manifest.xberg.members {
        validate_relative_path(&member.path)?;
    }
    if manifest.future_workers.len() != 1 {
        return Err("可选媒体工作进程清单必须且只能有一个条目".to_string());
    }
    for worker in &manifest.future_workers {
        if worker.id.is_empty() || worker.status.is_empty() || worker.blocking_reason.is_empty() {
            return Err("媒体工作进程清单缺少状态或阻塞说明".to_string());
        }
        for asset in &worker.assets {
            if asset.id.is_empty()
                || asset.url.is_empty()
                || asset.size_bytes == 0
                || asset.sha256.len() != 64
                || asset.archive_type.is_empty()
                || asset.target_root.is_empty()
            {
                return Err(format!("媒体工作进程资产 {} 的清单不完整", asset.id));
            }
            if !matches!(asset.target_root.as_str(), "worker" | "media_models") {
                return Err(format!("媒体工作进程资产 {} 的安装根目录无效", asset.id));
            }
            if let Some(path) = &asset.install_path {
                validate_relative_path(path)?;
            }
            if asset.archive_type == "file" && asset.install_path.is_none() {
                return Err(format!("媒体工作进程文件 {} 缺少安装路径", asset.id));
            }
            if asset.archive_type != "file" && asset.members.is_empty() {
                return Err(format!("媒体工作进程归档 {} 没有成员清单", asset.id));
            }
            for member in &asset.members {
                validate_relative_path(&member.path)?;
                validate_relative_path(&member.install_path)?;
                if member.size_bytes == 0 || member.sha256.len() != 64 {
                    return Err(format!("媒体工作进程成员 {} 的清单不完整", member.path));
                }
            }
        }
    }
    Ok(manifest)
}

fn ensure_worker_ready(manifest: &AssetManifest) -> Result<(), String> {
    let worker = manifest
        .future_workers
        .first()
        .ok_or_else(|| "媒体工作进程清单缺失".to_string())?;
    for asset in &worker.assets {
        let root = worker_target_root(&asset.target_root);
        if asset.archive_type == "file" {
            let install_path = asset
                .install_path
                .as_ref()
                .ok_or_else(|| format!("媒体工作进程资产 {} 缺少安装路径", asset.id))?;
            verify_file(&root.join(install_path), asset.size_bytes, &asset.sha256).map_err(
                |error| {
                    format!(
                        "媒体工作进程资产 {} 校验失败：{error}；{}",
                        asset.id, worker.blocking_reason
                    )
                },
            )?;
        } else {
            for member in &asset.members {
                verify_file(
                    &root.join(&member.install_path),
                    member.size_bytes,
                    &member.sha256,
                )
                .map_err(|error| {
                    format!(
                        "媒体工作进程资产 {} 的 {} 校验失败：{error}；{}",
                        asset.id, member.install_path, worker.blocking_reason
                    )
                })?;
            }
        }
    }
    Ok(())
}

fn initialize_worker_assets(
    manifest: &AssetManifest,
    cancel: &AtomicBool,
    progress: &mut impl FnMut(String),
    staging: &Path,
    downloader: &mut dyn AssetDownloader,
) -> Result<(), String> {
    let worker = manifest
        .future_workers
        .first()
        .ok_or_else(|| "媒体工作进程清单缺失".to_string())?;
    if worker_assets_ready(worker) {
        progress("复用已校验的媒体工作进程及原生依赖".to_string());
        return Ok(());
    }
    let worker_stage = staging.join("media-worker");
    fs::create_dir_all(&worker_stage)
        .map_err(|error| format!("创建媒体工作进程临时目录失败：{error}"))?;
    let mut ordered_assets = Vec::with_capacity(worker.assets.len());
    ordered_assets.extend(
        worker
            .assets
            .iter()
            .filter(|asset| asset.archive_type != "file"),
    );
    ordered_assets.extend(
        worker
            .assets
            .iter()
            .filter(|asset| asset.archive_type == "file"),
    );
    for (index, asset) in ordered_assets.into_iter().enumerate() {
        ensure_not_cancelled(cancel)?;
        if worker_asset_ready(asset) {
            progress(format!("复用已校验的媒体工作进程资产：{}", asset.id));
            continue;
        }
        progress(format!(
            "下载媒体工作进程资产 {} / {}：{}",
            index + 1,
            worker.assets.len(),
            asset.id
        ));
        let archive = worker_stage.join(&asset.id);
        let download_result = downloader.download(
            &asset.url,
            &archive,
            asset.size_bytes,
            &asset.sha256,
            cancel,
            progress,
        );
        if let Err(error) = download_result {
            if asset.archive_type == "file" {
                return Err(format!("{error}；{}", worker.blocking_reason));
            }
            return Err(error);
        }
        ensure_not_cancelled(cancel)?;
        if asset.archive_type == "file" {
            let install_path = asset
                .install_path
                .as_ref()
                .ok_or_else(|| format!("媒体工作进程文件 {} 缺少安装路径", asset.id))?;
            let target = worker_target_root(&asset.target_root).join(install_path);
            atomic_replace_file(&archive, &target)?;
            continue;
        }
        let extracted = worker_stage.join(format!("{index}-extracted"));
        fs::create_dir_all(&extracted)
            .map_err(|error| format!("创建媒体依赖解包目录失败：{error}"))?;
        match asset.archive_type.as_str() {
            "zip" => extract_zip_safely(&archive, &extracted, cancel)?,
            "tar.bz2" => extract_tar_bz2_safely(&archive, &extracted, cancel)?,
            other => return Err(format!("不支持的媒体资产归档格式：{other}")),
        }
        for member in &asset.members {
            let source = extracted.join(&member.path);
            verify_file(&source, member.size_bytes, &member.sha256)
                .map_err(|error| format!("媒体依赖 {} 校验失败：{error}", member.path))?;
            let target = worker_target_root(&asset.target_root).join(&member.install_path);
            atomic_replace_file(&source, &target)?;
        }
    }
    Ok(())
}

fn worker_assets_ready(worker: &FutureWorker) -> bool {
    if worker.status.is_empty() {
        return false;
    }
    worker.assets.iter().all(worker_asset_ready)
}

fn worker_asset_ready(asset: &WorkerAsset) -> bool {
    let root = worker_target_root(&asset.target_root);
    if asset.archive_type == "file" {
        let Some(install_path) = &asset.install_path else {
            return false;
        };
        return verify_file(&root.join(install_path), asset.size_bytes, &asset.sha256).is_ok();
    }
    asset.members.iter().all(|member| {
        verify_file(
            &root.join(&member.install_path),
            member.size_bytes,
            &member.sha256,
        )
        .is_ok()
    })
}

fn worker_target_root(target_root: &str) -> PathBuf {
    match target_root {
        "worker" => asset_root().join("worker").join("v0.1.0"),
        "media_models" => media_models_dir(),
        _ => PathBuf::new(),
    }
}

fn ensure_media_models_ready(manifest: &AssetManifest) -> Result<(), String> {
    let root = media_models_dir();
    for model in &manifest.media_models {
        verify_file(
            &root.join(&model.relative_path),
            model.size_bytes,
            &model.sha256,
        )
        .map_err(|error| format!("媒体模型 {} 校验失败：{error}", model.relative_path))?;
    }
    Ok(())
}

fn media_models_ready(manifest: &AssetManifest) -> bool {
    let root = media_models_dir();
    manifest.media_models.iter().all(|model| {
        verify_file(
            &root.join(&model.relative_path),
            model.size_bytes,
            &model.sha256,
        )
        .is_ok()
    })
}

fn asset_root() -> PathBuf {
    if cfg!(debug_assertions) {
        if let Some(path) = std::env::var_os("JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT") {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                return path;
            }
        }
    }
    if let Ok(path) = crate::config::state_dir() {
        return path.join(DATA_DIRECTORY);
    }
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local_app_data)
            .join("JchTools")
            .join(DATA_DIRECTORY);
    }
    std::env::temp_dir().join("JchTools").join(DATA_DIRECTORY)
}

fn validate_relative_path(path: &str) -> Result<(), String> {
    let path = path.replace('\\', "/");
    let candidate = Path::new(&path);
    if path.is_empty()
        || candidate.is_absolute()
        || path.starts_with('/')
        || path.contains('\0')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || path.as_bytes().get(1) == Some(&b':')
    {
        return Err(format!("资产路径不安全：{path}"));
    }
    Ok(())
}

fn verify_file(path: &Path, expected_size: u64, expected_sha256: &str) -> Result<(), String> {
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err("不是普通文件".to_string());
    }
    if metadata.len() != expected_size {
        return Err(format!("大小 {}，预期 {expected_size}", metadata.len()));
    }
    let actual = sha256_file(path).map_err(|error| error.to_string())?;
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        return Err(format!("SHA256 {actual}，预期 {expected_sha256}"));
    }
    Ok(())
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn download_asset(
    url: &str,
    destination: &Path,
    expected_size: u64,
    expected_sha256: &str,
    cancel: &AtomicBool,
    progress: &mut impl FnMut(String),
) -> Result<(), String> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("创建下载目录失败：{error}"))?;
    }
    let partial = destination.with_extension("part");
    let _ = fs::remove_file(&partial);
    for attempt in 1..=3 {
        ensure_not_cancelled(cancel)?;
        download_stream(url, &partial, expected_size, cancel, progress)?;
        match verify_file(&partial, expected_size, expected_sha256) {
            Ok(()) => {
                fs::rename(&partial, destination)
                    .map_err(|error| format!("写入下载资产失败：{error}"))?;
                return Ok(());
            }
            Err(error) if attempt < 3 => {
                progress(format!("资产校验失败，准备重试（{attempt}/3）：{error}"));
                let _ = fs::remove_file(&partial);
            }
            Err(error) => {
                let _ = fs::remove_file(&partial);
                return Err(format!("下载资产校验失败：{error}"));
            }
        }
    }
    Err("下载资产失败".to_string())
}

fn download_stream(
    url: &str,
    partial: &Path,
    expected_size: u64,
    cancel: &AtomicBool,
    progress: &mut impl FnMut(String),
) -> Result<(), String> {
    let agent = ureq::builder()
        .redirects(3)
        // 大资产（媒体模型 ~239MB、ffmpeg ~171MB）在慢链路上的整体下载时长不可预估，
        // 整体超时会让初始化在这类网络下永远无法完成（T-05 的可重试语义被硬上限
        // 抵消）。改为连接超时 + 单次读超时：整体时长无上限、由用户取消控制；
        // 单次读取停滞 60 秒即失败上抛（流级失败不进入 download_asset 的内部
        // 重试循环，该循环只重试校验失败）。
        .timeout_connect(Duration::from_secs(30))
        .timeout_read(Duration::from_secs(60))
        .user_agent("JchTools-markdown-assets/1")
        .build();
    let response = agent
        .get(url)
        .call()
        .map_err(|error| format!("下载请求失败：{error}"))?;
    if !(200..300).contains(&response.status()) {
        return Err(format!("下载请求返回 HTTP {}", response.status()));
    }
    let mut reader = response.into_reader();
    let mut output = File::create(partial).map_err(|error| format!("创建下载文件失败：{error}"))?;
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut current = 0_u64;
    loop {
        if cancel.load(Ordering::Acquire) {
            let _ = fs::remove_file(partial);
            return Err("用户已取消初始化".to_string());
        }
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("读取下载数据失败：{error}"))?;
        if read == 0 {
            break;
        }
        output
            .write_all(&buffer[..read])
            .map_err(|error| format!("写入下载数据失败：{error}"))?;
        current = current.saturating_add(read as u64);
        if expected_size > 0 {
            let percent = current
                .saturating_mul(100)
                .checked_div(expected_size)
                .unwrap_or(100)
                .min(100);
            progress(format!("下载进度：{percent}%"));
        } else {
            progress(format!("下载进度：{current} 字节"));
        }
    }
    output
        .sync_all()
        .map_err(|error| format!("同步下载文件失败：{error}"))?;
    Ok(())
}

fn extract_zip_safely(
    archive: &Path,
    destination: &Path,
    cancel: &AtomicBool,
) -> Result<(), String> {
    ensure_not_cancelled(cancel)?;
    let file = File::open(archive).map_err(|error| format!("打开 Xberg 压缩包失败：{error}"))?;
    let mut zip =
        ZipArchive::new(file).map_err(|error| format!("读取 Xberg 压缩包失败：{error}"))?;
    let mut seen = HashSet::new();
    for index in 0..zip.len() {
        ensure_not_cancelled(cancel)?;
        let mut entry = zip
            .by_index(index)
            .map_err(|error| format!("读取压缩包条目失败：{error}"))?;
        let relative = entry
            .enclosed_name()
            .ok_or_else(|| format!("压缩包包含不安全路径：{}", entry.name()))?
            .clone();
        if !seen.insert(relative.clone()) {
            return Err(format!("压缩包包含重复路径：{}", relative.display()));
        }
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170_000 == 0o120_000)
        {
            return Err(format!("压缩包包含符号链接：{}", entry.name()));
        }
        let target = destination.join(&relative);
        if entry.is_dir() {
            fs::create_dir_all(&target).map_err(|error| format!("创建解包目录失败：{error}"))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| format!("创建解包目录失败：{error}"))?;
        }
        let mut output =
            File::create(&target).map_err(|error| format!("创建解包文件失败：{error}"))?;
        io::copy(&mut entry, &mut output).map_err(|error| format!("写入解包文件失败：{error}"))?;
        output
            .sync_all()
            .map_err(|error| format!("同步解包文件失败：{error}"))?;
    }
    Ok(())
}

fn extract_tar_bz2_safely(
    archive: &Path,
    destination: &Path,
    cancel: &AtomicBool,
) -> Result<(), String> {
    ensure_not_cancelled(cancel)?;
    let file = File::open(archive).map_err(|error| format!("打开 sherpa 压缩包失败：{error}"))?;
    let decoder = BzDecoder::new(file);
    let mut tar = Archive::new(decoder);
    let entries = tar
        .entries()
        .map_err(|error| format!("读取 sherpa tar 条目失败：{error}"))?;
    let mut seen = HashSet::new();
    for entry in entries {
        ensure_not_cancelled(cancel)?;
        let mut entry = entry.map_err(|error| format!("读取 sherpa tar 条目失败：{error}"))?;
        let relative = entry
            .path()
            .map_err(|error| format!("读取 sherpa tar 路径失败：{error}"))?
            .to_path_buf();
        let kind = entry.header().entry_type();
        let mut relative_string = relative.to_string_lossy().replace('\\', "/");
        if kind.is_dir() && relative_string.ends_with('/') {
            relative_string.pop();
        }
        validate_relative_path(&relative_string)?;
        let relative = PathBuf::from(relative_string);
        if !seen.insert(relative.clone()) {
            return Err(format!("sherpa tar 包含重复路径：{}", relative.display()));
        }
        if kind.is_symlink() || kind.is_hard_link() {
            return Err(format!("sherpa tar 包含链接：{}", relative.display()));
        }
        let target = destination.join(&relative);
        if kind.is_dir() {
            fs::create_dir_all(&target)
                .map_err(|error| format!("创建 sherpa 解包目录失败：{error}"))?;
            continue;
        }
        if !kind.is_file() {
            return Err(format!(
                "sherpa tar 包含不支持的条目：{}",
                relative.display()
            ));
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("创建 sherpa 解包目录失败：{error}"))?;
        }
        let mut output =
            File::create(&target).map_err(|error| format!("创建 sherpa 解包文件失败：{error}"))?;
        io::copy(&mut entry, &mut output)
            .map_err(|error| format!("写入 sherpa 解包文件失败：{error}"))?;
        output
            .sync_all()
            .map_err(|error| format!("同步 sherpa 解包文件失败：{error}"))?;
    }
    Ok(())
}

fn atomic_replace_file(staged: &Path, destination: &Path) -> Result<(), String> {
    let parent = destination
        .parent()
        .ok_or_else(|| format!("无法确定安装文件目录：{}", destination.display()))?;
    fs::create_dir_all(parent).map_err(|error| format!("创建安装文件目录失败：{error}"))?;
    let backup = parent.join(format!(".old-file-{}", Uuid::new_v4().simple()));
    let had_existing = destination.exists();
    if had_existing {
        fs::rename(destination, &backup).map_err(|error| format!("暂存旧文件失败：{error}"))?;
    }
    if let Err(error) = fs::rename(staged, destination) {
        let install_error = format!("原子就位文件失败：{error}");
        if had_existing {
            if let Err(restore_error) = restore_backup(&backup, destination, "文件") {
                return Err(format!("{install_error}；{restore_error}"));
            }
        }
        return Err(install_error);
    }
    if had_existing {
        fs::remove_file(&backup).map_err(|error| format!("清理旧文件失败：{error}"))?;
    }
    Ok(())
}

fn atomic_replace_dir(staged: &Path, destination: &Path) -> Result<(), String> {
    let parent = destination
        .parent()
        .ok_or_else(|| format!("无法确定安装目录：{}", destination.display()))?;
    fs::create_dir_all(parent).map_err(|error| format!("创建安装目录失败：{error}"))?;
    let backup = parent.join(format!(".old-{}", Uuid::new_v4().simple()));
    let had_existing = destination.exists();
    if had_existing {
        fs::rename(destination, &backup).map_err(|error| format!("暂存旧资产失败：{error}"))?;
    }
    if let Err(error) = fs::rename(staged, destination) {
        let install_error = format!("原子就位资产失败：{error}");
        if had_existing {
            if let Err(restore_error) = restore_backup(&backup, destination, "资产") {
                return Err(format!("{install_error}；{restore_error}"));
            }
        }
        return Err(install_error);
    }
    if had_existing {
        fs::remove_dir_all(&backup).map_err(|error| format!("清理旧资产失败：{error}"))?;
    }
    Ok(())
}

fn restore_backup(backup: &Path, destination: &Path, kind: &str) -> Result<(), String> {
    fs::rename(backup, destination).map_err(|error| {
        format!(
            "恢复旧{kind}失败：{error}；旧{kind}仍保留在：{}",
            backup.display()
        )
    })
}

fn write_notice(path: &Path, manifest: &AssetManifest) -> Result<(), String> {
    let mut text = String::from("# JchTools 转 Markdown 可选组件许可证\n\n");
    text.push_str("Xberg 运行目录由用户指定；媒体组件由用户主动初始化后下载。主程序安装包不包含这些资产。\n\n");
    for license in &manifest.xberg.licenses {
        let _ = writeln!(
            &mut text,
            "- {}：{}，{}",
            license.component, license.license, license.source
        );
    }
    for model in &manifest.media_models {
        let _ = writeln!(
            &mut text,
            "- {}：{}，{}",
            model.license.component, model.license.license, model.license.source
        );
    }
    for worker in &manifest.future_workers {
        let _ = writeln!(
            &mut text,
            "\n## {}\n\n状态：{}\n\n{}",
            worker.id, worker.status, worker.blocking_reason
        );
        for asset in &worker.assets {
            let _ = writeln!(
                &mut text,
                "- {}：{}，{}，归档大小 {} 字节，SHA256 {}，来源 {}",
                asset.license.component,
                asset.license.license,
                asset.archive_type,
                asset.size_bytes,
                asset.sha256,
                asset.license.source
            );
        }
    }
    fs::write(path, text).map_err(|error| format!("写入许可证 notice 失败：{error}"))
}

fn ensure_not_cancelled(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) {
        Err("用户已取消初始化".to_string())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{extract_tar_bz2_safely, load_manifest, restore_backup};
    use bzip2::write::BzEncoder;
    use bzip2::Compression;
    use std::fs::{self, File};
    use std::io;
    use std::sync::atomic::AtomicBool;
    use tar::{Builder, EntryType, Header};

    // 覆盖 T-19/T-27/附录 D「Rust 直接调用 FFmpeg 原生接口」与 T-06 固定资产：
    // FFmpeg 资产必须是官方 shared 构建的四个 DLL（avutil/swresample/avcodec/
    // avformat），worker 经 libloading 直调；不得回退为 ffmpeg.exe 子进程布局。
    #[test]
    fn ffmpeg_asset_is_shared_dll_set() {
        let manifest = load_manifest().expect("内置资产清单必须可解析");
        let worker = &manifest.future_workers[0];
        let ffmpeg = worker
            .assets
            .iter()
            .find(|asset| asset.id.to_ascii_lowercase().contains("ffmpeg"))
            .expect("清单必须包含 FFmpeg 资产");
        let installs: Vec<&str> = ffmpeg
            .members
            .iter()
            .map(|member| member.install_path.as_str())
            .collect();
        for dll in [
            "ffmpeg/avutil-61.dll",
            "ffmpeg/swresample-7.dll",
            "ffmpeg/avcodec-63.dll",
            "ffmpeg/avformat-63.dll",
        ] {
            assert!(
                installs.contains(&dll),
                "FFmpeg shared DLL 缺失：{dll}（实际成员：{installs:?}）"
            );
        }
        assert!(
            !installs.iter().any(|path| path.ends_with("ffmpeg.exe")),
            "FFmpeg 资产不得再携带 ffmpeg.exe 子进程布局（实际成员：{installs:?}）"
        );
    }

    // ===== F22：媒体模型逐资产就绪、补缺下载与失败保留（T-05/T-06）=====

    use super::{
        ensure_media_models_ready, initialize_staged, media_models_dir, worker_target_root,
        AssetDownloader, AssetManifest, FutureWorker, LicenseEntry, MediaModel, WorkerAsset,
        XbergManifest, XBERG_TAG,
    };
    use sha2::{Digest, Sha256};
    use std::path::{Path, PathBuf};
    use std::sync::MutexGuard;
    use std::sync::{Mutex, OnceLock};

    /// 测试与其它用例共享进程环境变量，必须串行访问资产根目录。
    fn asset_root_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// 把资产根目录重定向到临时目录；Drop 时恢复环境，避免污染其它测试。
    struct AssetRootGuard {
        root: tempfile::TempDir,
        _lock: MutexGuard<'static, ()>,
    }

    fn redirect_asset_root() -> AssetRootGuard {
        let lock = asset_root_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = tempfile::tempdir().expect("创建资产根目录");
        std::env::set_var("JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT", root.path());
        AssetRootGuard { root, _lock: lock }
    }

    impl Drop for AssetRootGuard {
        fn drop(&mut self) {
            std::env::remove_var("JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT");
        }
    }

    fn sha256_bytes(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    /// 可脚本化的假下载器：按 URL 供给固定字节；可指定第 N 次调用失败。
    struct FakeDownloader {
        blobs: Vec<(String, Vec<u8>)>,
        fail_on_call: Option<usize>,
        calls: Vec<String>,
    }

    impl FakeDownloader {
        fn new(blobs: Vec<(String, Vec<u8>)>) -> Self {
            Self {
                blobs,
                fail_on_call: None,
                calls: Vec::new(),
            }
        }

        fn urls(&self) -> Vec<String> {
            self.calls.clone()
        }
    }

    impl AssetDownloader for FakeDownloader {
        fn download(
            &mut self,
            url: &str,
            destination: &Path,
            _expected_size: u64,
            _expected_sha256: &str,
            _cancel: &AtomicBool,
            _progress: &mut dyn FnMut(String),
        ) -> Result<(), String> {
            self.calls.push(url.to_string());
            if Some(self.calls.len()) == self.fail_on_call {
                return Err("模拟下载失败".to_string());
            }
            let blob = self
                .blobs
                .iter()
                .find(|(candidate, _)| candidate == url)
                .map(|(_, bytes)| bytes.clone())
                .ok_or_else(|| format!("假下载器没有 URL 的内容：{url}"))?;
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            fs::write(destination, blob).map_err(|error| error.to_string())
        }
    }

    fn fixture_blobs() -> Vec<(String, Vec<u8>)> {
        b"abc"
            .iter()
            .map(|tag| {
                (
                    format!("https://fixtures.invalid/model-{}.bin", *tag as char),
                    vec![*tag; 64],
                )
            })
            .collect()
    }

    /// 构造三模型 + 一个已就绪 worker 资产的最小清单；模型内容与摘要互相匹配。
    fn fixture_manifest(blobs: &[(String, Vec<u8>)]) -> AssetManifest {
        let models = ["a", "b", "c"]
            .iter()
            .enumerate()
            .map(|(index, tag)| {
                let url = format!("https://fixtures.invalid/model-{tag}.bin");
                let blob = &blobs
                    .iter()
                    .find(|(candidate, _)| *candidate == url)
                    .expect("每个模型都要有供给字节")
                    .1;
                MediaModel {
                    id: format!("model-{tag}"),
                    url: url.clone(),
                    relative_path: format!("models/group{index}/model-{tag}.bin"),
                    size_bytes: blob.len() as u64,
                    sha256: sha256_bytes(blob),
                    license: LicenseEntry {
                        component: format!("model {tag}"),
                        license: "MIT".to_string(),
                        source: "https://fixtures.invalid".to_string(),
                    },
                }
            })
            .collect();
        AssetManifest {
            schema_version: 1,
            xberg: XbergManifest {
                tag: XBERG_TAG.to_string(),
                archive_url: "https://fixtures.invalid/xberg.zip".to_string(),
                archive_size_bytes: 1,
                archive_sha256: "a".repeat(64),
                members: Vec::new(),
                licenses: Vec::new(),
            },
            media_models: models,
            future_workers: vec![FutureWorker {
                id: "worker".to_string(),
                status: "ready".to_string(),
                blocking_reason: "测试阻塞说明".to_string(),
                assets: vec![WorkerAsset {
                    id: "worker.exe".to_string(),
                    url: "https://fixtures.invalid/worker.exe".to_string(),
                    size_bytes: 13,
                    sha256: sha256_bytes(b"worker-binary"),
                    archive_type: "file".to_string(),
                    target_root: "worker".to_string(),
                    install_path: Some("worker.exe".to_string()),
                    members: Vec::new(),
                    license: LicenseEntry {
                        component: "worker".to_string(),
                        license: "MIT".to_string(),
                        source: "https://fixtures.invalid".to_string(),
                    },
                }],
            }],
        }
    }

    /// 预置已通过校验的 worker 资产，跳过 worker 下载分支。
    fn preinstall_worker() {
        let target = worker_target_root("worker").join("worker.exe");
        fs::create_dir_all(target.parent().expect("worker 路径有父目录"))
            .expect("创建 worker 目录");
        fs::write(&target, b"worker-binary").expect("预置 worker 资产");
    }

    fn model_path(model: &MediaModel) -> PathBuf {
        media_models_dir().join(&model.relative_path)
    }

    fn install_model(model: &MediaModel, blob: &[u8]) {
        let target = model_path(model);
        fs::create_dir_all(target.parent().expect("模型路径有父目录")).expect("创建模型目录");
        fs::write(&target, blob).expect("预置模型文件");
    }

    fn staged_run(
        manifest: &AssetManifest,
        guard: &AssetRootGuard,
        downloader: &mut FakeDownloader,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        let staging = guard.root.path().join("staging");
        fs::create_dir_all(&staging).expect("创建 staging");
        let mut progress = |_message: String| {};
        initialize_staged(
            manifest,
            cancel,
            &mut progress,
            &staging,
            guard.root.path(),
            downloader,
        )
    }

    // 覆盖 T-05「保留已校验资产」：三缺一时只下载缺失的那一个模型。
    #[test]
    fn media_init_downloads_only_missing_models() {
        let guard = redirect_asset_root();
        let blobs = fixture_blobs();
        let manifest = fixture_manifest(&blobs);
        preinstall_worker();
        // 已就绪：模型 b、c；缺失：模型 a。
        install_model(&manifest.media_models[1], &blobs[1].1);
        install_model(&manifest.media_models[2], &blobs[2].1);
        let mut downloader = FakeDownloader::new(blobs.clone());
        let cancel = AtomicBool::new(false);

        staged_run(&manifest, &guard, &mut downloader, &cancel).expect("补缺初始化应成功");

        assert_eq!(
            downloader.urls(),
            vec![manifest.media_models[0].url.clone()],
            "只应下载缺失的模型 a，实际下载了：{:?}",
            downloader.urls()
        );
        ensure_media_models_ready(&manifest).expect("补缺后三模型都应就绪");
        assert_eq!(
            fs::read(model_path(&manifest.media_models[1])).expect("模型 b 仍在"),
            blobs[1].1,
            "已校验模型不得被改动"
        );
    }

    // 覆盖 T-05「初始化可重试」：第 2 个模型下载失败后重试，不得重下第 1 个。
    #[test]
    fn media_init_retry_after_failure_skips_verified_downloads() {
        let guard = redirect_asset_root();
        let blobs = fixture_blobs();
        let manifest = fixture_manifest(&blobs);
        preinstall_worker();
        let cancel = AtomicBool::new(false);

        // 第一次：模型 b（第 2 次调用）失败。
        let mut failing = FakeDownloader::new(blobs.clone());
        failing.fail_on_call = Some(2);
        let first = staged_run(&manifest, &guard, &mut failing, &cancel);
        assert!(first.is_err(), "第 2 个模型失败必须使初始化失败");

        // 第二次：全部成功。模型 a 已在失败前落位并通过校验，不得重下。
        let mut retry = FakeDownloader::new(blobs.clone());
        staged_run(&manifest, &guard, &mut retry, &cancel).expect("重试应成功");
        assert_eq!(
            retry.urls(),
            vec![
                manifest.media_models[1].url.clone(),
                manifest.media_models[2].url.clone(),
            ],
            "重试只应下载缺失的 b 和 c，不得重下已校验的 a：{:?}",
            retry.urls()
        );
        ensure_media_models_ready(&manifest).expect("重试后三模型都应就绪");
    }

    // 覆盖 T-05/T-06：取消初始化不得删除或覆盖最终目录中已校验的模型。
    #[test]
    fn media_init_cancel_preserves_existing_final_models() {
        let guard = redirect_asset_root();
        let blobs = fixture_blobs();
        let manifest = fixture_manifest(&blobs);
        preinstall_worker();
        install_model(&manifest.media_models[1], &blobs[1].1);
        let mut downloader = FakeDownloader::new(blobs.clone());
        let cancel = AtomicBool::new(true);

        let result = staged_run(&manifest, &guard, &mut downloader, &cancel);
        let error = result.expect_err("已取消的初始化必须失败");
        assert!(error.contains("取消"), "错误应说明是用户取消：{error}");
        assert!(downloader.urls().is_empty(), "取消后不得发起任何下载");
        assert_eq!(
            fs::read(model_path(&manifest.media_models[1])).expect("已就绪模型必须保留"),
            blobs[1].1,
            "取消不得删除或改动最终目录中已校验的模型"
        );
        assert!(
            !model_path(&manifest.media_models[0]).exists(),
            "取消后不得留下半成品"
        );
        // 取消也不得破坏最终目录里已存在的其它资产（worker）。
        assert!(
            worker_target_root("worker").join("worker.exe").is_file(),
            "取消不得删除已安装的 worker"
        );
    }

    // 覆盖 T-05、T-06：首次初始化应接受 sherpa 归档中的合法目录条目。
    #[test]
    fn sherpa_tar_accepts_directory_entry_with_trailing_slash() {
        let root = tempfile::tempdir().expect("创建测试目录");
        let archive = root.path().join("sherpa.tar.bz2");
        let encoder = BzEncoder::new(
            File::create(&archive).expect("创建归档"),
            Compression::default(),
        );
        let mut builder = Builder::new(encoder);

        let mut directory = Header::new_gnu();
        directory.set_entry_type(EntryType::Directory);
        directory.set_size(0);
        directory.set_mode(0o755);
        directory.set_cksum();
        builder
            .append_data(&mut directory, "sherpa-root/", io::empty())
            .expect("写入带尾斜杠的目录条目");

        let body = b"model data";
        let mut file = Header::new_gnu();
        file.set_entry_type(EntryType::Regular);
        file.set_size(body.len() as u64);
        file.set_mode(0o644);
        file.set_cksum();
        builder
            .append_data(&mut file, "sherpa-root/model.bin", &body[..])
            .expect("写入模型文件");
        builder
            .into_inner()
            .expect("结束 tar")
            .finish()
            .expect("结束 bzip2");

        let destination = root.path().join("out");
        let cancelled = AtomicBool::new(false);
        let result = extract_tar_bz2_safely(&archive, &destination, &cancelled);
        assert!(result.is_ok(), "目录条目末尾斜杠应合法：{result:?}");
        assert_eq!(
            fs::read(destination.join("sherpa-root/model.bin")).expect("读取解包文件"),
            body
        );
    }

    #[test]
    fn restore_backup_reports_failure_and_preserves_backup() {
        let root = tempfile::tempdir().expect("创建测试目录");
        let backup = root.path().join("backup");
        let destination = root.path().join("destination");
        fs::write(&backup, b"old-content").expect("写入备份");
        fs::create_dir(&destination).expect("创建冲突目录");

        let error =
            restore_backup(&backup, &destination, "文件").expect_err("目标冲突时恢复必须报告失败");

        assert!(error.contains("恢复旧文件失败"));
        assert!(error.contains("旧文件仍保留在"));
        assert!(backup.exists(), "恢复失败时不得丢弃备份");
        assert!(destination.is_dir(), "冲突目标必须保持不变");
    }
}
