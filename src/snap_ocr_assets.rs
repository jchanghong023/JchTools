//! 截图 OCR（O 分区）的可选资产初始化与就绪检查。
//!
//! 资产清单（resources/snap-ocr-assets.json）编译进主程序：det/rec ONNX、派生字典、
//! 结果窗专用字体与推理运行库均为固定版本、固定来源、固定 SHA-256（O-05/O-09）；
//! 只在用户于图形界面主动初始化时联网下载（O-06/O-10），安装到用户状态目录的
//! snap-ocr/ 子树（models/、fonts/、worker/），主程序包不携带这些重资产。
//! 初始化遵循 staging → 校验 → 原子落位：取消或失败删除本轮 staging，不覆盖
//! 已经校验通过的完整资产；重试时已验证资产直接复用，不重复下载。

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt::Write as FmtWrite;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use uuid::Uuid;
use zip::ZipArchive;

const MANIFEST: &str = include_str!(concat!(env!("OUT_DIR"), "/snap-ocr-assets.json"));
const DATA_DIRECTORY: &str = "snap-ocr";
/// worker 固定版本目录：资产清单 worker 条目的 install_path 必须落在该目录下
/// （服务进程从这条固定路径启动，见 gui.rs 的服务监督线程）。
const WORKER_VERSION: &str = "v0.1.1";
const WORKER_EXE_NAME: &str = "snap-ocr-worker.exe";
/// 构建期占位标记：打包阶段回填 worker 的真实 size/sha256 后删除该状态。
const WORKER_STATUS_PENDING: &str = "pending-build";

#[derive(Debug, Deserialize)]
struct SnapAssetManifest {
    schema_version: u32,
    assets: Vec<SnapAsset>,
    workers: Vec<SnapWorker>,
}

#[derive(Debug, Deserialize)]
struct SnapAsset {
    id: String,
    url: String,
    archive_type: String,
    install_path: Option<String>,
    size_bytes: u64,
    sha256: String,
    #[serde(default)]
    members: Vec<SnapMember>,
    license: SnapLicense,
}

#[derive(Clone, Debug, Deserialize)]
struct SnapMember {
    path: String,
    install_path: String,
    size_bytes: u64,
    sha256: String,
}

#[derive(Debug, Deserialize)]
struct SnapLicense {
    component: String,
    license: String,
    source: String,
}

#[derive(Debug, Deserialize)]
struct SnapWorker {
    id: String,
    #[serde(default)]
    status: String,
    url: String,
    archive_type: String,
    install_path: String,
    size_bytes: u64,
    sha256: String,
    #[serde(default)]
    members: Vec<SnapMember>,
    license: SnapLicense,
}

impl SnapWorker {
    /// 构建期占位条目：摘要与大小尚未回填，不得尝试下载（O-09：未提供可获取的
    /// 正确字节前不得宣称初始化链路可用）。
    fn is_pending(&self) -> bool {
        self.status == WORKER_STATUS_PENDING
    }
}

/// 本功能资产根目录：用户状态目录下的 snap-ocr/（模型、字体与 worker 的缓存落盘
/// 属于 O-06 明确允许的资产写入，与截图/识别内容无关）。
pub fn asset_root() -> PathBuf {
    if cfg!(debug_assertions) {
        if let Some(path) = std::env::var_os("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT") {
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

/// 命名管道身份：当前用户名与 Windows 登录会话 ID；同一用户的不同登录会话
/// 不能连接到另一会话的托盘服务。服务侧 service::protocol 使用相同字节序列。
#[cfg(windows)]
fn session_id() -> u32 {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcessId() -> u32;
        fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> i32;
    }
    let mut session = u32::MAX;
    // SAFETY: GetCurrentProcessId 无参数，查询当前进程 ID。
    let pid = unsafe { GetCurrentProcessId() };
    // SAFETY: session 是可写的 u32，调用期间其地址有效；失败时保留 MAX 哨兵。
    unsafe { ProcessIdToSessionId(pid, &raw mut session) };
    session
}
#[cfg(not(windows))]
fn session_id() -> u32 {
    0
}

fn pipe_identity() -> String {
    let user = std::env::var("USERNAME")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            std::env::var("USERPROFILE")
                .ok()
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| "anonymous".to_string());
    format!("{user}:{}", session_id())
}

/// 截图服务命名管道名：`\\.\pipe\jchtools-snap-ocr-<hash>`，hash 为
/// 「用户名:登录会话ID」UTF-8 字节 SHA-256 的前 16 个十六进制小写字符。
pub fn pipe_name() -> String {
    let identity = pipe_identity();
    let digest = Sha256::digest(identity.as_bytes());
    let mut hash = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        // 两位十六进制，无格式化失败路径。
        let _ = write!(&mut hash, "{byte:02x}");
    }
    format!(r"\\.\pipe\jchtools-snap-ocr-{hash}")
}

/// 只读检查所有已安装资产：不联网、不创建目录、不修改文件（O-09 加载前离线验证）。
/// worker 条目仍为构建期占位时按未就绪报告，并说明原因（不冒称就绪，O-11）。
pub fn readiness() -> Result<(), String> {
    let manifest = load_manifest()?;
    let root = asset_root();
    for asset in &manifest.assets {
        if asset_ready(asset, &root).is_err() {
            return Err(format!("资产 {} 未安装或校验失败", asset.id));
        }
    }
    let worker = manifest
        .workers
        .first()
        .ok_or_else(|| "截图 OCR 工作进程清单缺失".to_string())?;
    if worker.is_pending() {
        return Err(format!(
            "截图 OCR 工作进程尚未发布（清单条目 {} 为构建期占位）；打包阶段回填后重试初始化即可，已下载资产会复用",
            worker.id
        ));
    }
    if worker_ready(worker, &root).is_err() {
        return Err("截图 OCR 工作进程未安装或校验失败".to_string());
    }
    Ok(())
}

/// 单个资产的下载接缝：生产实现走固定来源的 HTTP 下载；与 markdown_assets 的
/// AssetDownloader 同构，测试可注入本地供给验证「补缺下载」与「失败后重试不重下」。
trait SnapDownloader {
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

/// 生产下载器：只访问清单固定地址（O-06/O-10：仅用户主动初始化联网）。
struct NetworkDownloader;

impl SnapDownloader for NetworkDownloader {
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

/// 下载、校验并原子安装全部可选资产（O-06）。
///
/// 取消会终止当前下载或解包阶段并删除本轮 staging 目录；最终位置已校验的资产
/// 跨重试复用、不重下；失败不覆盖已经校验的完整资产。
pub fn initialize(cancel: &AtomicBool, mut progress: impl FnMut(String)) -> Result<(), String> {
    if readiness().is_ok() {
        progress("截图 OCR 组件已就绪".to_string());
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
    manifest: &SnapAssetManifest,
    cancel: &AtomicBool,
    progress: &mut impl FnMut(String),
    staging: &Path,
    root: &Path,
    downloader: &mut dyn SnapDownloader,
) -> Result<(), String> {
    let total = manifest.assets.len();
    for (index, asset) in manifest.assets.iter().enumerate() {
        ensure_not_cancelled(cancel)?;
        // O-06 跨重试复用：最终位置已校验的资产不重下（保留已验证下载）。
        if asset_ready(asset, root).is_ok() {
            progress(format!("复用已校验资产：{}", asset.id));
            continue;
        }
        progress(format!("下载资产 {}/{}：{}", index + 1, total, asset.id));
        install_asset(asset, staging, root, cancel, downloader, progress)?;
    }
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
    if worker_ready(worker, root).is_ok() {
        progress("复用已校验的工作进程".to_string());
    } else {
        progress(format!(
            "下载资产 {}/{}：{}",
            total + 1,
            total + 1,
            worker.id
        ));
        let asset = SnapAsset {
            id: worker.id.clone(),
            url: worker.url.clone(),
            archive_type: worker.archive_type.clone(),
            install_path: Some(worker.install_path.clone()),
            size_bytes: worker.size_bytes,
            sha256: worker.sha256.clone(),
            members: worker.members.clone(),
            license: SnapLicense {
                component: worker.license.component.clone(),
                license: worker.license.license.clone(),
                source: worker.license.source.clone(),
            },
        };
        install_asset(&asset, staging, root, cancel, downloader, progress)?;
    }
    let notice_stage = staging.join("licenses");
    fs::create_dir_all(&notice_stage).map_err(|error| format!("创建许可证目录失败：{error}"))?;
    write_notice(&notice_stage.join("THIRD_PARTY_NOTICES.md"), manifest)?;
    atomic_replace_dir(&notice_stage, &root.join("licenses"))?;
    readiness()?;
    progress("截图 OCR 组件初始化完成".to_string());
    Ok(())
}

/// 下载单个资产（文件或归档）并按清单落位：先在 staging 校验，再原子替换到最终
/// 位置；最终位置已有校验通过的副本时不覆盖（失败安装不得动已验证资产）。
fn install_asset(
    asset: &SnapAsset,
    staging: &Path,
    root: &Path,
    cancel: &AtomicBool,
    downloader: &mut dyn SnapDownloader,
    progress: &mut impl FnMut(String),
) -> Result<(), String> {
    let stage_dir = staging.join(&asset.id);
    fs::create_dir_all(&stage_dir).map_err(|error| format!("创建资产临时目录失败：{error}"))?;
    let archive = stage_dir.join("download.bin");
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
    verify_file(&archive, asset.size_bytes, &asset.sha256)
        .map_err(|error| format!("资产 {} 下载内容校验失败：{error}", asset.id))?;
    match asset.archive_type.as_str() {
        "file" => {
            let install_path = asset
                .install_path
                .as_ref()
                .ok_or_else(|| format!("文件资产 {} 缺少安装路径", asset.id))?;
            let target = root.join(install_path);
            if verify_file(&target, asset.size_bytes, &asset.sha256).is_err() {
                atomic_replace_file(&archive, &target)?;
            }
            Ok(())
        }
        "zip" => {
            let extracted = stage_dir.join("extracted");
            fs::create_dir_all(&extracted).map_err(|error| format!("创建解包目录失败：{error}"))?;
            extract_zip_safely(&archive, &extracted, cancel)?;
            if asset.members.is_empty() {
                return Err(format!("归档资产 {} 没有成员清单", asset.id));
            }
            for member in &asset.members {
                let source = extracted.join(&member.path);
                verify_file(&source, member.size_bytes, &member.sha256).map_err(|error| {
                    format!("资产 {} 成员 {} 校验失败：{error}", asset.id, member.path)
                })?;
                let target = root.join(&member.install_path);
                if verify_file(&target, member.size_bytes, &member.sha256).is_err() {
                    atomic_replace_file(&source, &target)?;
                }
            }
            Ok(())
        }
        other => Err(format!("不支持的资产归档格式：{other}")),
    }
}

fn load_manifest() -> Result<SnapAssetManifest, String> {
    let manifest: SnapAssetManifest = serde_json::from_str(MANIFEST)
        .map_err(|error| format!("截图 OCR 资产清单无效：{error}"))?;
    if manifest.schema_version != 1 {
        return Err(format!("不支持的资产清单版本：{}", manifest.schema_version));
    }
    if manifest.workers.len() != 1 {
        return Err("截图 OCR 工作进程清单必须且只能有一个条目".to_string());
    }
    for asset in &manifest.assets {
        validate_asset(asset)?;
    }
    let worker = &manifest.workers[0];
    if worker.id.is_empty() || worker.url.is_empty() || worker.install_path.is_empty() {
        return Err("截图 OCR 工作进程清单条目不完整".to_string());
    }
    if worker.is_pending() {
        // 构建期占位：允许加载（页面如实显示未就绪），但禁止通过结构校验混入真下载。
        if worker.size_bytes != 0 || worker.sha256 != "PENDING-BUILD" {
            return Err(format!(
                "工作进程 {} 的占位条目必须保持 size=0 且 sha256=PENDING-BUILD",
                worker.id
            ));
        }
    } else {
        if worker.status != "ok" {
            return Err(format!(
                "工作进程 {} 的状态无效：{}",
                worker.id, worker.status
            ));
        }
        if worker.size_bytes == 0 || worker.sha256.len() != 64 || worker.archive_type != "file" {
            return Err(format!("工作进程 {} 的清单不完整", worker.id));
        }
        let expected = format!("worker/{WORKER_VERSION}/{WORKER_EXE_NAME}").replace('\\', "/");
        let normalized = worker.install_path.replace('\\', "/");
        if normalized != expected {
            return Err(format!(
                "工作进程 {} 的安装路径必须是 {expected}，实际为 {normalized}",
                worker.id
            ));
        }
        validate_relative_path(&worker.install_path)?;
    }
    Ok(manifest)
}

fn validate_asset(asset: &SnapAsset) -> Result<(), String> {
    if asset.id.is_empty()
        || asset.url.is_empty()
        || asset.size_bytes == 0
        || asset.sha256.len() != 64
        || asset.archive_type.is_empty()
    {
        return Err(format!("资产 {} 的清单不完整", asset.id));
    }
    if let Some(path) = &asset.install_path {
        validate_relative_path(path)?;
    }
    if asset.archive_type == "file" && asset.install_path.is_none() {
        return Err(format!("文件资产 {} 缺少安装路径", asset.id));
    }
    if asset.archive_type != "file" && asset.members.is_empty() {
        return Err(format!("归档资产 {} 没有成员清单", asset.id));
    }
    for member in &asset.members {
        validate_relative_path(&member.path)?;
        validate_relative_path(&member.install_path)?;
        if member.size_bytes == 0 || member.sha256.len() != 64 {
            return Err(format!(
                "资产 {} 成员 {} 的清单不完整",
                asset.id, member.path
            ));
        }
    }
    Ok(())
}

/// 单个资产（含全部成员）是否已在最终位置校验通过。
fn asset_ready(asset: &SnapAsset, root: &Path) -> Result<(), String> {
    match asset.archive_type.as_str() {
        "file" => {
            let install_path = asset
                .install_path
                .as_ref()
                .ok_or_else(|| format!("文件资产 {} 缺少安装路径", asset.id))?;
            verify_file(&root.join(install_path), asset.size_bytes, &asset.sha256)
        }
        "zip" => {
            if asset.members.is_empty() {
                return Err(format!("归档资产 {} 没有成员清单", asset.id));
            }
            for member in &asset.members {
                verify_file(
                    &root.join(&member.install_path),
                    member.size_bytes,
                    &member.sha256,
                )?;
            }
            Ok(())
        }
        other => Err(format!("不支持的资产归档格式：{other}")),
    }
}

fn worker_ready(worker: &SnapWorker, root: &Path) -> Result<(), String> {
    if worker.archive_type != "file" {
        return Err(format!("工作进程 {} 必须是单文件资产", worker.id));
    }
    verify_file(
        &root.join(&worker.install_path),
        worker.size_bytes,
        &worker.sha256,
    )
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
        // 与 markdown_assets 同口径：连接超时 + 单次读超时，整体时长由用户取消控制。
        .timeout_connect(Duration::from_secs(30))
        .timeout_read(Duration::from_secs(60))
        .user_agent("JchTools-snap-ocr-assets/1")
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
    let file = File::open(archive).map_err(|error| format!("打开压缩包失败：{error}"))?;
    let mut zip = ZipArchive::new(file).map_err(|error| format!("读取压缩包失败：{error}"))?;
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
            if let Err(restore_error) = fs::rename(&backup, destination) {
                return Err(format!(
                    "{install_error}；恢复旧文件失败：{restore_error}；旧文件仍保留在：{}",
                    backup.display()
                ));
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
            if let Err(restore_error) = fs::rename(&backup, destination) {
                return Err(format!(
                    "{install_error}；恢复旧资产失败：{restore_error}；旧资产仍保留在：{}",
                    backup.display()
                ));
            }
        }
        return Err(install_error);
    }
    if had_existing {
        fs::remove_dir_all(&backup).map_err(|error| format!("清理旧资产失败：{error}"))?;
    }
    Ok(())
}

fn write_notice(path: &Path, manifest: &SnapAssetManifest) -> Result<(), String> {
    let mut text = String::from("# JchTools 截图 OCR 可选组件许可证\n\n");
    text.push_str(
        "模型、字体与推理运行库由用户主动初始化后按固定清单下载；主程序安装包不包含这些资产。\n\n",
    );
    for asset in &manifest.assets {
        let _ = writeln!(
            &mut text,
            "- {}：{}，{}，来源 {}",
            asset.license.component,
            asset.license.license,
            asset.archive_type,
            asset.license.source
        );
    }
    for worker in &manifest.workers {
        let state = if worker.is_pending() {
            "构建期占位（打包阶段回填）"
        } else {
            worker.status.as_str()
        };
        let _ = writeln!(
            &mut text,
            "\n## {}\n\n状态：{}\n\n- {}：{}，来源 {}",
            worker.id,
            state,
            worker.license.component,
            worker.license.license,
            worker.license.source
        );
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
