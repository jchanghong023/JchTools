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
    let result = initialize_staged(&manifest, cancel, &mut progress, &staging, &root);
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
) -> Result<(), String> {
    if media_models_ready(manifest) {
        progress("复用已校验的媒体模型".to_string());
    } else {
        let media_stage = staging.join("media-models");
        fs::create_dir_all(&media_stage)
            .map_err(|error| format!("创建媒体模型临时目录失败：{error}"))?;
        for (index, model) in manifest.media_models.iter().enumerate() {
            ensure_not_cancelled(cancel)?;
            progress(format!(
                "下载媒体模型 {} / {}：{}",
                index + 1,
                manifest.media_models.len(),
                model.id
            ));
            let model_relative = Path::new(&model.relative_path)
                .strip_prefix("models")
                .map_err(|_| format!("媒体模型 {} 必须安装在 models 子目录", model.id))?;
            let target = media_stage.join(model_relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("创建媒体模型目录失败：{error}"))?;
            }
            download_asset(
                &model.url,
                &target,
                model.size_bytes,
                &model.sha256,
                cancel,
                progress,
            )?;
        }
        atomic_replace_dir(&media_stage, &media_models_dir().join("models"))?;
    }

    let notice_stage = staging.join("licenses");
    fs::create_dir_all(&notice_stage).map_err(|error| format!("创建许可证目录失败：{error}"))?;
    write_notice(&notice_stage.join("THIRD_PARTY_NOTICES.md"), manifest)?;
    atomic_replace_dir(&notice_stage, &root.join("licenses"))?;
    initialize_worker_assets(manifest, cancel, progress, staging)?;
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
        let download_result = download_asset(
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
        .timeout(Duration::from_secs(60))
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
        let relative_string = relative.to_string_lossy().replace('\\', "/");
        validate_relative_path(&relative_string)?;
        let relative = PathBuf::from(relative_string);
        if !seen.insert(relative.clone()) {
            return Err(format!("sherpa tar 包含重复路径：{}", relative.display()));
        }
        let kind = entry.header().entry_type();
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
    use super::restore_backup;
    use std::fs;

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
