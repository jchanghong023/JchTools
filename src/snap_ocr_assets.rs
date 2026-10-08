//! 截图 OCR（O 分区）的可选资产初始化与就绪检查。
//!
//! 资产清单（resources/snap-ocr-assets.json）编译进主程序：结果窗专用字体为
//! 固定版本、固定来源、固定 SHA-256（O-05/O-09）；识别用的模型、推理运行库与
//! `xberg.exe` 从应用级 SQLite 保存的共享目录读取，按截图场景固定清单校验。
//! 只在用户于图形界面主动初始化时联网下载（O-06/O-10），主程序包不携带这些重资产。
//! 初始化遵循 staging → 校验 → 原子落位：取消或失败删除本轮 staging，不覆盖
//! 已经校验通过的完整资产；重试时已验证资产直接复用，不重复下载。
//! 下载/校验/原子落位与推理组件包安装核心与转 Markdown 共用
//! [`crate::asset_util`]，两侧行为同源。

use crate::asset_util::{
    atomic_replace_dir, atomic_replace_file, cleanup_stale_staging_dirs, ensure_not_cancelled,
    extract_zip_safely, finalize_staging, require_component_members,
    require_inference_members_for_scenario, resolve_xberg_component, state_dir_asset_root,
    valid_component_tag, validate_relative_path, verify_file, verify_file_with_cancel,
    AssetDownloader, InferenceManifest,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fmt::Write as FmtWrite;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;
use uuid::Uuid;

const MANIFEST: &str = include_str!(concat!(env!("OUT_DIR"), "/snap-ocr-assets.json"));
const DATA_DIRECTORY: &str = "snap-ocr";
/// worker 固定版本目录：资产清单 worker 条目的 install_path 必须落在该目录下
/// （服务进程从这条固定路径启动，见 gui.rs 的服务监督线程）。
const WORKER_VERSION: &str = "v0.1.2";
const WORKER_EXE_NAME: &str = "snap-ocr-worker.exe";
/// 构建期占位标记：打包阶段回填 worker 的真实 size/sha256 后删除该状态。
const WORKER_STATUS_PENDING: &str = "pending-build";

#[derive(Debug, Deserialize)]
struct SnapAssetManifest {
    schema_version: u32,
    assets: Vec<SnapAsset>,
    workers: Vec<SnapWorker>,
    /// Xberg 推理组件包（XB-10：xberg.exe、O-07 截图模型集与 onnxruntime，
    /// 来源为固定发布 zip）。清单未接入该条目时为 None，组件缺失时如实报告。
    #[serde(default)]
    xberg_inference: Option<SnapInferencePack>,
}

/// 推理组件包条目：tag 决定安装目录 `xberg-inference/<tag>/`，其余字段与
/// 普通归档资产同构，复用同一安装与校验路径。
#[derive(Debug, Deserialize)]
struct SnapInferencePack {
    tag: String,
    #[serde(flatten)]
    asset: SnapAsset,
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

impl SnapAsset {
    /// O-05/XB-25：字体和许可证编入随包 worker，不再依赖用户字体缓存。
    fn is_embedded(&self) -> bool {
        matches!(
            self.id.as_str(),
            "noto-sans-mono-cjk-sc" | "noto-cjk-ofl-license"
        )
    }
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
    #[allow(dead_code)]
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

/// 测试支持构建的资产根覆盖：`JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT`
/// 指向绝对路径时，资产根与截图服务管道名（见 [`pipe_name`]）一并脱离生产
/// 位置——无头测试因此既不会 spawn 真实 worker（readiness 对隔离根必然
/// 失败），也不会向真实用户会话的服务管道发送请求（生产名可被真实服务应答，
/// attach-main-exe 会改写其 launcher.json）。
fn test_asset_root_override() -> Option<PathBuf> {
    if !(cfg!(test) || cfg!(feature = "test-hooks")) {
        return None;
    }
    std::env::var_os("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// 本功能资产根目录：用户状态目录下的 snap-ocr/（模型、字体与 worker 的缓存落盘
/// 属于 O-06 明确允许的资产写入，与截图/识别内容无关）。回退链共用
/// `asset_util::state_dir_asset_root`（包内私有助手）。
pub fn asset_root() -> PathBuf {
    if let Some(path) = test_asset_root_override() {
        return path;
    }
    state_dir_asset_root(DATA_DIRECTORY)
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
/// 测试资产根覆盖生效时改用派生的测试专用名（C'-1，见 `test_asset_root_override`）。
pub fn pipe_name() -> String {
    // C'-1：同一覆盖变量驱动管道名隔离——无头 GUI 测试的监督线程只会 ping
    // 测试专用名（无服务监听，连接必然失败），不会触碰真实用户会话的服务。
    if let Some(root) = test_asset_root_override() {
        let mut identity = b"test:".to_vec();
        identity.extend_from_slice(root.as_os_str().as_encoded_bytes());
        let digest = Sha256::digest(&identity);
        let mut hash = String::with_capacity(16);
        for byte in digest.iter().take(8) {
            // 两位十六进制，无格式化失败路径。
            let _ = write!(&mut hash, "{byte:02x}");
        }
        return format!(r"\\.\pipe\jchtools-snap-ocr-test-{hash}");
    }
    let identity = pipe_identity();
    let digest = Sha256::digest(identity.as_bytes());
    let mut hash = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        // 两位十六进制，无格式化失败路径。
        let _ = write!(&mut hash, "{byte:02x}");
    }
    format!(r"\\.\pipe\jchtools-snap-ocr-{hash}")
}

/// 只读检查所有已安装资产：不联网、不创建目录、不修改文件（O-09 加载前离线
/// 检查所需资产存在）。worker 条目仍为构建期占位时按未就绪报告，并说明原因
/// （不冒称就绪，O-11）。Xberg 推理组件按「在位校验 + 清单成员存在性」检查；
/// 缺失时如实报告，不冒称就绪。
/// 后台工作进程的安装路径（取清单条目的 install_path；升版只改清单）。
/// XB-25：主包同目录的后台程序；测试资产隔离时不误用生产程序。
fn bundled_worker() -> Option<PathBuf> {
    if test_asset_root_override().is_some() {
        return std::env::var_os("JCHTOOLS_TEST_BUNDLED_WORKER")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute() && p.is_file());
    }
    let path = std::env::current_exe()
        .ok()?
        .parent()?
        .join(WORKER_EXE_NAME);
    path.is_file().then_some(path)
}

pub fn worker_install_path() -> Result<PathBuf, String> {
    if let Some(path) = bundled_worker() {
        let manifest = load_manifest()?;
        let worker = manifest.workers.first().ok_or("后台服务清单缺失")?;
        verify_bundled_worker(&path, worker)?;
        return Ok(path);
    }
    let manifest = load_manifest()?;
    let worker = manifest
        .workers
        .first()
        .ok_or_else(|| "截图 OCR 工作进程清单缺失".to_string())?;
    let root = asset_root();
    worker_ready(worker, &root).map_err(|error| format!("截图 OCR 工作进程校验失败：{error}"))?;
    Ok(root.join(&worker.install_path))
}

/// bundled worker 的启动链口径完整性校验：开发版同目录二进制由 cargo build
/// 产生（target/debug），免摘要；发布目录必须匹配打包时回填的 size/SHA-256。
/// 就绪检查与启动链（`worker_install_path`）共用本判据（S7-02 同口径要求）。
fn verify_bundled_worker(path: &Path, worker: &SnapWorker) -> Result<(), String> {
    let dev_output =
        cfg!(debug_assertions) && path.parent().is_some_and(|p| p.ends_with("target/debug"));
    if !dev_output {
        verify_file(path, worker.size_bytes, &worker.sha256)?;
    }
    Ok(())
}

/// 就绪检查的工作进程段：bundled 在场时走启动链同口径校验（S7-02：损坏的
/// worker 必须在 readiness 阶段即报未就绪并携带原因，不得拖到启动链才失败，
/// O-09「自有可选资产不完整必须阻止使用并明确提示」）；缓存安装按清单校验。
fn worker_readiness(worker: &SnapWorker, root: &Path) -> Result<(), String> {
    if let Some(path) = bundled_worker() {
        return verify_bundled_worker(&path, worker)
            .map_err(|error| format!("截图 OCR 工作进程校验失败：{error}"));
    }
    if worker_ready(worker, root).is_err() {
        return Err("截图 OCR 工作进程未安装或校验失败".to_string());
    }
    Ok(())
}

pub fn readiness() -> Result<(), String> {
    let manifest = load_manifest()?;
    let root = asset_root();
    local_asset_readiness(&manifest.assets, &root)?;
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
    worker_readiness(worker, &root)?;
    readiness_inference_pack(&manifest, &root)
}

fn local_asset_readiness(assets: &[SnapAsset], root: &Path) -> Result<(), String> {
    for asset in assets {
        if asset.is_embedded() {
            continue;
        }
        // S7-05：透传具体失败原因（缺失/大小/摘要），GUI 侧直接展示该文案；
        // 压缩成统一「未安装或校验失败」会丢失失败种类（O-09/O-30）。
        if let Err(error) = asset_ready(asset, root) {
            return Err(format!("资产 {}：{}", asset.id, error));
        }
    }
    Ok(())
}

/// 就绪检查的推理组件段：在位校验 + 清单成员存在性检查。
///
/// 成员检查基于共享解析规则（C-2）：组件目录只来自应用 SQLite 保存的共享
/// Xberg 目录（设置页保存或产品内下载；2026-10-04 起不再有环境变量覆盖），
/// 成员检查必须与在位校验使用同一目录——此前成员校验直接按资产根相对
/// install_path 进行，绕过共享解析，导致配置目录在位校验通过后必报
/// 「推理组件包校验失败」、永远无法就绪。成员
/// 过滤与存在性检查经 [`crate::asset_util::require_inference_members_for_scenario`]
/// 与 markdown 侧共用同一实现（XB-09 2026-10-02 修订：不比对摘要）。
fn readiness_inference_pack(manifest: &SnapAssetManifest, root: &Path) -> Result<(), String> {
    let component = xberg_inference_ready(root)?;
    // 清单接入推理组件包后做成员级存在性检查（XB-09 修订后口径；未接入时
    // 在位校验已覆盖）。
    if let Some(pack) = &manifest.xberg_inference {
        let inference = inference_manifest_from_pack(pack)?;
        require_inference_members_for_scenario(&component, "snapshot", &inference.members)?;
    }
    Ok(())
}

/// Xberg 推理组件安装根：`<资产根>/xberg-inference/`（每个发布版本一个 tag 子目录）。
#[must_use]
pub fn xberg_inference_root() -> PathBuf {
    asset_root().join("xberg-inference")
}

/// Xberg 推理组件的在位校验（存在性；XB-09 2026-10-02 修订后运行时不比对
/// 摘要）：
/// `xberg.exe` + `models/snapshot-ocr/{det.onnx,rec.onnx,dict/dict.txt}` + `onnxruntime.dll`。
/// `_root` 形参保留调用点形状；组件目录一律由共享解析规则提供（其删除属
/// 基线再生成事项，另行确认）。
pub fn xberg_inference_ready(_root: &Path) -> Result<PathBuf, String> {
    let component = resolve_xberg_component()?;
    xberg_layout_ready(&component).map(|()| component)
}

/// 单个组件目录的在位校验（存在性，不做摘要）。
fn xberg_layout_ready(component: &Path) -> Result<(), String> {
    let required = [
        component.join("xberg.exe"),
        component
            .join("models")
            .join("snapshot-ocr")
            .join("det.onnx"),
        component
            .join("models")
            .join("snapshot-ocr")
            .join("rec.onnx"),
        component
            .join("models")
            .join("snapshot-ocr")
            .join("dict")
            .join("dict.txt"),
        component.join("onnxruntime.dll"),
    ];
    require_component_members(component, &required)
}

/// 下载、校验并原子安装全部可选资产（O-06）。
///
/// 取消会终止当前下载或解包阶段并删除本轮 staging 目录；最终位置已校验的资产
/// 跨重试复用、不重下；失败不覆盖已经校验的完整资产。下载接缝与生产下载器
/// 共用 `crate::asset_util::AssetDownloader` / `crate::asset_util::NetworkDownloader`，
/// 测试可注入本地供给验证「补缺下载」与「失败后重试不重下」。
pub fn initialize(cancel: &AtomicBool, mut progress: impl FnMut(String)) -> Result<(), String> {
    // P-10：初始化是关键功能任务，开始/结束统计必须落盘（下载内部另有
    // 逐次重试与回退日志）。
    tracing::info!("截图 OCR 组件初始化开始");
    let started = std::time::Instant::now();
    let result = initialize_task(cancel, &mut progress);
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    match &result {
        Ok(()) => tracing::info!(elapsed_ms, "截图 OCR 组件初始化完成"),
        Err(reason) if reason.contains("取消") => {
            tracing::info!(elapsed_ms, reason = %reason, "截图 OCR 组件初始化已取消");
        }
        Err(reason) => tracing::error!(elapsed_ms, reason = %reason, "截图 OCR 组件初始化失败"),
    }
    result
}

fn initialize_task(cancel: &AtomicBool, progress: &mut dyn FnMut(String)) -> Result<(), String> {
    let root = asset_root();
    // B-2：先兜底清理历史残留的 staging（readiness 提前返回、取消后清理
    // 失败或进程崩溃都会残留 .staging-<uuid>，download.zip 残留可达约 291MB）。
    cleanup_stale_staging(&root);
    if readiness().is_ok() {
        progress("截图 OCR 组件已就绪".to_string());
        return Ok(());
    }
    ensure_not_cancelled(cancel)?;
    let manifest = load_manifest()?;
    fs::create_dir_all(&root).map_err(|error| format!("创建资产目录失败：{error}"))?;
    let staging = root.join(format!(".staging-{}", Uuid::new_v4().simple()));
    fs::create_dir_all(&staging).map_err(|error| format!("创建临时目录失败：{error}"))?;
    let mut downloader = NetworkDownloader;
    let result = initialize_staged(
        &manifest,
        cancel,
        progress,
        &staging,
        &root,
        &mut downloader,
    );
    finalize_staging(result, &staging, progress)
}

/// 兜底清理资产根下历史残留的 `.staging-*` 目录（B-2，`.staging-*` 清扫与
/// markdown 侧共用 [`crate::asset_util::cleanup_stale_staging_dirs`]），
/// 并清扫 write_expected_tag 在 `xberg-inference/` 下残留的 `.expected-tag-*`
/// 临时文件；单项失败跳过继续。
///
/// 不扫 `.old-*` 备份：那是原子替换路径的暂存（asset_util），其中
/// xberg-inference/<tag> 目标的残留按设计由 prune_old_inference_tags 收集
///（该安装链当前仅在测试中启用，见 asset_util；生产初始化只校验共享目录，
/// XB-10），入口一概删除会把替换失败后仍可恢复的备份提前清掉。
fn cleanup_stale_staging(root: &Path) {
    cleanup_stale_staging_dirs(root);
    // write_expected_tag 的临时标记（xberg-inference/.expected-tag-<uuid>）在
    // 写入或原子就位失败/进程崩溃时残留（XB-09），与 .staging-* 同属入口兜底
    // 清扫。
    cleanup_expected_tag_residue(&root.join("xberg-inference"));
}

/// 清扫 `xberg-inference/` 下顶层的 `.expected-tag-*` 残留临时文件（文件非
/// 目录，按类型删除）；目录不存在（组件从未安装）时无需清理，单项失败跳过。
fn cleanup_expected_tag_residue(base: &Path) {
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_temp = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".expected-tag-"));
        if is_temp && path.is_file() {
            // 尽力而为：残留被防护软件短暂锁定时跳过，下次初始化再试。
            let _ = fs::remove_file(&path);
        }
    }
}

fn initialize_staged(
    manifest: &SnapAssetManifest,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(String),
    staging: &Path,
    root: &Path,
    downloader: &mut dyn AssetDownloader,
) -> Result<(), String> {
    let total = manifest.assets.len();
    for (index, asset) in manifest.assets.iter().enumerate() {
        ensure_not_cancelled(cancel)?;
        if asset.is_embedded() {
            progress(format!("使用随包内置资产：{}", asset.id));
            continue;
        }
        // O-06 跨重试复用：最终位置已校验的资产不重下（保留已验证下载）。
        if asset_ready(asset, root).is_ok() {
            progress(format!("复用已校验资产：{}", asset.id));
            continue;
        }
        progress(format!("下载资产 {}/{}：{}", index + 1, total, asset.id));
        install_asset(asset, staging, root, cancel, downloader, progress)?;
    }
    // XB-10：只校验共享目录，不下载、复制或改写用户的 Xberg。
    readiness_inference_pack(manifest, root)?;
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
    let bundled_present = bundled_worker().is_some();
    let bundled_ready = bundled_present && worker_install_path().is_ok();
    let cached_ready = !bundled_present && worker_ready(worker, root).is_ok();
    if bundled_ready || cached_ready {
        progress("复用已校验的工作进程".to_string());
    } else {
        return Err(worker_repair_message(worker));
    }
    let notice_stage = staging.join("licenses");
    fs::create_dir_all(&notice_stage).map_err(|error| format!("创建许可证目录失败：{error}"))?;
    write_notice(&notice_stage.join("THIRD_PARTY_NOTICES.md"), manifest)?;
    atomic_replace_dir(&notice_stage, &root.join("licenses"))?;
    readiness()?;
    progress("截图 OCR 组件初始化完成".to_string());
    Ok(())
}

fn worker_repair_message(worker: &SnapWorker) -> String {
    format!(
        "截图 OCR 工作进程未安装或与当前清单摘要不匹配（{}）；请重新安装当前 JchTools 包修复 worker，不会下载其他版本",
        worker.install_path
    )
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
    verify_file_with_cancel(&archive, asset.size_bytes, &asset.sha256, cancel)
        .map_err(|error| format!("资产 {} 下载内容校验失败：{error}", asset.id))?;
    match asset.archive_type.as_str() {
        "file" => {
            let install_path = asset
                .install_path
                .as_ref()
                .ok_or_else(|| format!("文件资产 {} 缺少安装路径", asset.id))?;
            let target = root.join(install_path);
            if verify_file_with_cancel(&target, asset.size_bytes, &asset.sha256, cancel).is_err() {
                ensure_not_cancelled(cancel)?;
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
                verify_file_with_cancel(&source, member.size_bytes, &member.sha256, cancel)
                    .map_err(|error| {
                        format!("资产 {} 成员 {} 校验失败：{error}", asset.id, member.path)
                    })?;
                let target = root.join(&member.install_path);
                if verify_file_with_cancel(&target, member.size_bytes, &member.sha256, cancel)
                    .is_err()
                {
                    ensure_not_cancelled(cancel)?;
                    atomic_replace_file(&source, &target)?;
                }
            }
            Ok(())
        }
        other => Err(format!("不支持的资产归档格式：{other}")),
    }
}

/// snap 组件包条目 → 共享安装路径的推理组件清单（XB-10）。
///
/// snap 清单成员的 install_path 相对资产根（serde 强制 `xberg-inference/<tag>/`
/// 前缀），而共享的安装与校验按组件目录相对路径进行；转换时剥离该前缀，
/// 否则组件会嵌套安装到 `xberg-inference/<tag>/xberg-inference/<tag>/`。
/// 前缀由清单校验保证存在；剥离失败按清单损坏报错（此前静默回退原路径，
/// 在组件目录校验口径下会嵌套出错误目录，掩盖真实问题）。
fn inference_manifest_from_pack(pack: &SnapInferencePack) -> Result<InferenceManifest, String> {
    let prefix = format!("xberg-inference/{}/", pack.tag);
    let mut members = Vec::with_capacity(pack.asset.members.len());
    for member in &pack.asset.members {
        let install_path = member.install_path.replace('\\', "/");
        let stripped = install_path.strip_prefix(&prefix).ok_or_else(|| {
            format!(
                "推理组件清单损坏：成员 {} 未落在 xberg-inference/{}/ 之下",
                member.install_path, pack.tag
            )
        })?;
        members.push(crate::asset_util::InferenceMember {
            path: member.path.clone(),
            install_path: stripped.to_string(),
            size_bytes: member.size_bytes,
            sha256: member.sha256.clone(),
        });
    }
    Ok(InferenceManifest {
        tag: pack.tag.clone(),
        url: pack.asset.url.clone(),
        size_bytes: pack.asset.size_bytes,
        sha256: pack.asset.sha256.clone(),
        members,
    })
}

/// 写入推理组件的清单 tag 标记（`xberg-inference/expected-tag.txt`），供截图
/// 服务进程做与主程序同口径的版本校验（XB-09）。
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
    if let Some(pack) = &manifest.xberg_inference {
        if !valid_component_tag(&pack.tag) {
            return Err("推理组件包 tag 不合法".to_string());
        }
        validate_asset(&pack.asset)?;
        if pack.asset.archive_type != "zip" || pack.asset.members.is_empty() {
            return Err(format!(
                "推理组件包 {} 必须是带成员清单的 zip 归档",
                pack.asset.id
            ));
        }
        for member in &pack.asset.members {
            let under_tag = member.install_path.replace('\\', "/");
            if !under_tag.starts_with(&format!("xberg-inference/{}/", pack.tag)) {
                return Err(format!(
                    "推理组件包 {} 成员 {} 必须安装在 xberg-inference/{}/ 之下",
                    pack.asset.id, member.install_path, pack.tag
                ));
            }
        }
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

/// 下载接缝共用 [`crate::asset_util::AssetDownloader`]；本文件持有唯一的
/// HTTP 下载原语（P-03 联网边界测试按文件白名单执法：`ureq` 只允许出现在
/// `snap_ocr_assets.rs` 与 `markdown_assets.rs`），测试可注入本地供给验证
/// 「补缺下载」与「失败后重试不重下」。
///
/// 生产下载器：只访问清单固定地址（O-06/O-10：仅用户主动初始化联网）。
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

/// 流式下载到 `.part` 并按清单摘要校验；下载中断与校验失败均最多重试 3 次
///（C-3：弱网一次中断不作废整包），成功后原子改名落位。
/// markdown 侧的可选组件初始化复用同一原语（T-05/T-21 同口径）。
pub(crate) fn download_asset(
    url: &str,
    destination: &Path,
    expected_size: u64,
    expected_sha256: &str,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(String),
) -> Result<(), String> {
    // P-10：网络出口调用先记开始——请求若永久阻塞或进程崩溃，只有完成日志
    // 无法指认卡在哪一步。
    tracing::info!(url, size_bytes = expected_size, "资产下载开始");
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("创建下载目录失败：{error}"))?;
    }
    let partial = destination.with_extension("part");
    let _ = fs::remove_file(&partial);
    let mut fetch = |partial: &Path, progress: &mut dyn FnMut(String)| {
        download_stream(url, partial, expected_size, cancel, progress)
    };
    download_asset_with(
        destination,
        &partial,
        expected_size,
        expected_sha256,
        cancel,
        progress,
        &mut fetch,
    )
}

/// 下载 + 校验重试核心：单次流式拉取经 `fetch` 注入（生产为 HTTP 流式下载
/// [`download_stream`]，测试注入中断脚本），`.part` 命名与目录准备由调用方
/// 承担。重试语义（C-3）：拉取中断与校验失败均最多重试 3 次；第 3 次仍失败
/// 时返回含最后一次错误信息的 Err，`.part` 不残留。
fn download_asset_with<F>(
    destination: &Path,
    partial: &Path,
    expected_size: u64,
    expected_sha256: &str,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(String),
    fetch: &mut F,
) -> Result<(), String>
where
    F: FnMut(&Path, &mut dyn FnMut(String)) -> Result<(), String>,
{
    let started = std::time::Instant::now();
    for attempt in 1..=3 {
        ensure_not_cancelled(cancel)?;
        if let Err(error) = fetch(partial, progress) {
            // 取消优先于重试语义：用户取消时不发重试提示，直接收场。
            ensure_not_cancelled(cancel)?;
            if attempt < 3 {
                tracing::warn!(attempt, reason = %error, "资产下载中断，准备重试");
                progress(format!("下载中断，准备重试（{attempt}/3）：{error}"));
                let _ = fs::remove_file(partial);
                continue;
            }
            tracing::error!(
                reason = %error,
                elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                "资产下载失败（重试耗尽）"
            );
            let _ = fs::remove_file(partial);
            return Err(format!("下载资产失败：{error}"));
        }
        match verify_file_with_cancel(partial, expected_size, expected_sha256, cancel) {
            Ok(()) => {
                if let Err(error) = ensure_not_cancelled(cancel) {
                    let _ = fs::remove_file(partial);
                    return Err(error);
                }
                fs::rename(partial, destination)
                    .map_err(|error| format!("写入下载资产失败：{error}"))?;
                tracing::info!(
                    elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    bytes = expected_size,
                    "资产下载完成"
                );
                return Ok(());
            }
            Err(_) if cancel.load(std::sync::atomic::Ordering::Acquire) => {
                let _ = fs::remove_file(partial);
                return Err("用户已取消初始化".to_string());
            }
            Err(error) if attempt < 3 => {
                tracing::warn!(attempt, reason = %error, "资产校验失败，准备重试");
                progress(format!("资产校验失败，准备重试（{attempt}/3）：{error}"));
                let _ = fs::remove_file(partial);
            }
            Err(error) => {
                tracing::error!(
                    reason = %error,
                    elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    "资产校验失败（重试耗尽）"
                );
                let _ = fs::remove_file(partial);
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
    progress: &mut dyn FnMut(String),
) -> Result<(), String> {
    // P-09 的 ProxyOverride 按每个实际请求目标匹配，不能沿用初始 URL 的代理
    // 策略处理重定向；每跳失败时仅对该目标执行一次代理→直连回退。
    let proxy = crate::system_proxy::read();
    download_attempt(url, partial, expected_size, cancel, progress, &proxy).map_err(
        |(message, _)| {
            tracing::error!(url, reason = message, "资产下载失败");
            message
        },
    )
}

/// 单次下载尝试：逐跳重新选择系统代理；仅代理传输失败时对当前目标直连重试一次。
/// 重定向上限与锁定的 ureq 2.12.1 `.redirects(3)` 行为一致（最多跟随两跳）。
fn download_attempt(
    url: &str,
    partial: &Path,
    expected_size: u64,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(String),
    proxy: &crate::system_proxy::SystemProxy,
) -> Result<(), (String, bool)> {
    let mut current_url = url.to_string();
    let mut redirects_followed = 0_u32;
    let mut retry_direct = false;
    let mut proxy_failure = None;
    loop {
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            let _ = fs::remove_file(partial);
            return Err(("用户已取消初始化".to_string(), false));
        }
        let endpoint = if retry_direct {
            None
        } else {
            proxy.endpoint_for_url(&current_url)
        };
        let response = match download_request(&current_url, endpoint.as_deref(), progress) {
            Ok(response) => response,
            Err((proxy_message, true)) if endpoint.is_some() => {
                tracing::warn!(
                    proxy = proxy_message,
                    url = current_url,
                    "系统代理连接失败，按 P-09 自动回退直连重试"
                );
                progress("系统代理连接失败，自动回退直连重试".to_string());
                proxy_failure = Some(proxy_message);
                retry_direct = true;
                continue;
            }
            Err((direct_message, _)) => {
                return Err(download_attempt_error(
                    &current_url,
                    &mut proxy_failure,
                    direct_message,
                ));
            }
        };

        let status = response.status();
        // ureq checks the redirect limit before looking up Location, including
        // a third 3xx response without a Location header.
        if (300..400).contains(&status) && redirects_followed + 1 >= 3 {
            return Err(download_attempt_error(
                &current_url,
                &mut proxy_failure,
                format!("下载请求失败：达到重定向次数上限（3）：{current_url}"),
            ));
        }
        let location = if matches!(status, 301 | 302 | 303 | 307 | 308) {
            response.header("location").map(str::to_owned)
        } else {
            None
        };
        if let Some(location) = location {
            let request_url = match ureq::get(&current_url).request_url() {
                Ok(url) => url,
                Err(error) => {
                    return Err(download_attempt_error(
                        &current_url,
                        &mut proxy_failure,
                        format!("下载请求失败：{error}"),
                    ));
                }
            };
            let next_url = match request_url.as_url().join(&location) {
                Ok(url) => url,
                Err(error) => {
                    return Err(download_attempt_error(
                        &current_url,
                        &mut proxy_failure,
                        format!("下载重定向地址无效：{error}"),
                    ));
                }
            };
            drop(response);
            current_url = next_url.to_string();
            redirects_followed += 1;
            retry_direct = false;
            proxy_failure = None;
            continue;
        }
        if !(200..300).contains(&status) {
            return Err(download_attempt_error(
                &current_url,
                &mut proxy_failure,
                format!("下载请求返回 HTTP {status}"),
            ));
        }

        match download_response_body(response, partial, expected_size, cancel, progress) {
            Ok(()) => return Ok(()),
            Err((proxy_message, true)) if endpoint.is_some() => {
                tracing::warn!(
                    proxy = proxy_message,
                    url = current_url,
                    "系统代理连接失败，按 P-09 自动回退直连重试"
                );
                progress("系统代理连接失败，自动回退直连重试".to_string());
                let _ = fs::remove_file(partial);
                proxy_failure = Some(proxy_message);
                retry_direct = true;
            }
            Err((direct_message, _)) => {
                return Err(download_attempt_error(
                    &current_url,
                    &mut proxy_failure,
                    direct_message,
                ));
            }
        }
    }
}

fn download_attempt_error(
    url: &str,
    proxy_failure: &mut Option<String>,
    direct_message: String,
) -> (String, bool) {
    if let Some(proxy_message) = proxy_failure.take() {
        proxy_direct_failure(url, &proxy_message, &direct_message)
    } else {
        (direct_message, false)
    }
}

fn proxy_direct_failure(url: &str, proxy_message: &str, direct_message: &str) -> (String, bool) {
    tracing::error!(
        proxy = proxy_message,
        direct = direct_message,
        url,
        "系统代理与直连均失败"
    );
    (
        format!("系统代理与直连均失败——系统代理：{proxy_message}；直连：{direct_message}"),
        false,
    )
}

fn download_response_body(
    response: ureq::Response,
    partial: &Path,
    expected_size: u64,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(String),
) -> Result<(), (String, bool)> {
    let mut reader = response.into_reader();
    let mut output =
        File::create(partial).map_err(|error| (format!("创建下载文件失败：{error}"), false))?;
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut current = 0_u64;
    loop {
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            drop(output);
            let _ = fs::remove_file(partial);
            return Err(("用户已取消初始化".to_string(), false));
        }
        let read = reader
            .read(&mut buffer)
            .map_err(|error| (format!("读取下载数据失败：{error}"), true))?;
        if read == 0 {
            break;
        }
        let next = match checked_download_total(
            current,
            u64::try_from(read).unwrap_or(u64::MAX),
            expected_size,
        ) {
            Ok(next) => next,
            Err(error) => {
                drop(output);
                let _ = fs::remove_file(partial);
                return Err((error, false));
            }
        };
        output
            .write_all(&buffer[..read])
            .map_err(|error| (format!("写入下载数据失败：{error}"), false))?;
        current = next;
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
        .map_err(|error| (format!("同步下载文件失败：{error}"), false))?;
    Ok(())
}

/// 发起一个不自动跟随重定向的 GET；调用方按新 URL 重新应用系统代理策略。
/// XB-10：查询固定发布源的元数据，沿用 P-09 系统代理及一次直连回退。
pub(crate) fn fetch_release_json(
    url: &str,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(String),
) -> Result<serde_json::Value, String> {
    const MAX_METADATA_BYTES: usize = 2 * 1024 * 1024;
    let proxy = crate::system_proxy::read();
    let endpoint = proxy.endpoint_for_url(url);
    let mut direct = false;
    loop {
        let attempt = (|| -> Result<serde_json::Value, (String, bool)> {
            ensure_not_cancelled(cancel).map_err(|error| (error, false))?;
            let response = download_request(
                url,
                if direct { None } else { endpoint.as_deref() },
                progress,
            )?;
            let mut reader = response.into_reader();
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 8192];
            loop {
                ensure_not_cancelled(cancel).map_err(|error| (error, false))?;
                let read = reader
                    .read(&mut buffer)
                    .map_err(|error| (format!("读取发布元数据失败：{error}"), true))?;
                if read == 0 {
                    break;
                }
                if bytes.len().saturating_add(read) > MAX_METADATA_BYTES {
                    return Err(("发布元数据超过大小上限".into(), false));
                }
                bytes.extend_from_slice(&buffer[..read]);
            }
            ensure_not_cancelled(cancel).map_err(|error| (error, false))?;
            serde_json::from_slice(&bytes)
                .map_err(|error| (format!("发布元数据不是有效 JSON：{error}"), false))
        })();
        match attempt {
            Ok(value) => return Ok(value),
            Err((message, true)) if endpoint.is_some() && !direct => {
                progress(format!("系统代理查询失败，直连重试一次：{message}"));
                direct = true;
            }
            Err((message, _)) => return Err(message),
        }
    }
}

/// 发起一个不自动跟随重定向的 GET；调用方按新 URL 重新应用系统代理策略。
fn download_request(
    url: &str,
    proxy: Option<&str>,
    progress: &mut dyn FnMut(String),
) -> Result<ureq::Response, (String, bool)> {
    let mut builder = ureq::builder()
        .redirects(0)
        // 连接超时 + 单次读超时，整体时长由用户取消控制。
        .timeout_connect(Duration::from_secs(30))
        .timeout_read(Duration::from_secs(60))
        .user_agent("JchTools-snap-ocr-assets/1");
    if let Some(proxy_url) = proxy {
        match ureq::Proxy::new(proxy_url) {
            Ok(parsed) => builder = builder.proxy(parsed),
            // 端点字符串由本仓库解析生成，正常不可能非法；异常时按直连
            // 继续，不让代理问题阻塞下载（P-09 可用性优先）。
            Err(error) => progress(format!("系统代理地址无法解析（{error}），本次直连")),
        }
    }
    let agent = builder.build();
    agent.get(url).call().map_err(|error| match error {
        error @ ureq::Error::Transport(_) => {
            let retryable = matches!(
                error.kind(),
                ureq::ErrorKind::Dns
                    | ureq::ErrorKind::ConnectionFailed
                    | ureq::ErrorKind::ProxyConnect
                    | ureq::ErrorKind::Io
            );
            (format!("下载请求失败：{error}"), retryable)
        }
        status @ ureq::Error::Status(..) => (format!("下载请求失败：{status}"), false),
    })
}

fn checked_download_total(current: u64, chunk: u64, expected_size: u64) -> Result<u64, String> {
    let next = current.saturating_add(chunk);
    if expected_size > 0 && next > expected_size {
        Err(format!("下载数据超过清单大小（预期 {expected_size} 字节）"))
    } else {
        Ok(next)
    }
}

fn write_notice(path: &Path, manifest: &SnapAssetManifest) -> Result<(), String> {
    let mut text = String::from("# JchTools 截图 OCR 可选组件许可证\n\n");
    text.push_str(
        "截图字体和许可证内置于随包后台程序；Xberg 引擎、模型与推理运行库使用设置页配置的共享目录。\n\n",
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
    // XB-10/O-11：推理组件包的许可条目与 markdown 侧 notice 口径对齐——许可
    // 文件本身随组件树安装（合规不依赖 notice），此处列出条目并指向安装目录，
    // 与只遍历 assets + workers 的旧写法区分（修复前推理组件在 notice 中缺席）。
    if let Some(pack) = &manifest.xberg_inference {
        let _ = writeln!(
            &mut text,
            "- {}：{}，{}，来源 {}（随组件树安装于 xberg-inference/{}/，许可文件随附）",
            pack.asset.license.component,
            pack.asset.license.license,
            pack.asset.archive_type,
            pack.asset.license.source,
            pack.tag
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn checked_download_total_rejects_payload_over_manifest_size() {
        assert_eq!(
            super::checked_download_total(4, 6, 10).expect("边界大小应允许"),
            10
        );
        let error =
            super::checked_download_total(4, 7, 10).expect_err("超过清单大小必须在写盘前拒绝");
        assert!(error.contains("超过清单大小"));
    }

    #[test]
    fn missing_worker_reports_repair_without_downloading_another_version() {
        let worker = super::SnapWorker {
            id: "snap-ocr-worker".to_string(),
            status: "ok".to_string(),
            url: "https://example.invalid/snap-ocr-worker.exe".to_string(),
            archive_type: "file".to_string(),
            install_path: "worker/v0.1.2/snap-ocr-worker.exe".to_string(),
            size_bytes: 1,
            sha256: "00".repeat(32),
            members: vec![],
            license: super::SnapLicense {
                component: "worker".to_string(),
                license: "MIT".to_string(),
                source: "https://example.invalid".to_string(),
            },
        };

        let message = super::worker_repair_message(&worker);
        assert!(message.contains("重新安装当前 JchTools 包"));
        assert!(message.contains("不会下载其他版本"));
    }

    // 覆盖 O-11/O-13：推理组件的在位校验——缺失、不完整与齐备分别得到明确的
    // 结论，不冒称就绪；存在性检查不要求摘要（摘要校验随清单条目接入）。
    #[test]
    fn xberg_component_layout_reports_missing_incomplete_and_ready() {
        let temp = tempfile::tempdir().expect("临时目录应可创建");
        let component = temp.path().join("v2026.9.27-test");

        // 什么都没有：未配置。
        let error = super::xberg_layout_ready(&component).expect_err("空目录应报不完整");
        assert!(error.contains("推理组件不完整"), "unexpected: {error}");

        // 只放可执行文件：仍缺模型与运行库，且错误点名缺失项。
        fs::create_dir_all(&component).expect("组件目录应可创建");
        fs::write(component.join("xberg.exe"), b"stub").expect("stub 写入");
        let error = super::xberg_layout_ready(&component).expect_err("缺模型应报不完整");
        assert!(
            error.contains("det.onnx"),
            "错误应点名缺失的模型文件：{error}"
        );

        // 三件模型齐了但缺 onnxruntime.dll：仍不就绪。
        let models = component.join("models").join("snapshot-ocr");
        fs::create_dir_all(models.join("dict")).expect("模型目录应可创建");
        fs::write(models.join("det.onnx"), b"det").expect("stub 写入");
        fs::write(models.join("rec.onnx"), b"rec").expect("stub 写入");
        fs::write(models.join("dict").join("dict.txt"), b"dict").expect("stub 写入");
        let error = super::xberg_layout_ready(&component).expect_err("缺运行库应报不完整");
        assert!(
            error.contains("onnxruntime.dll"),
            "错误应点名缺失的运行库：{error}"
        );

        // 全部在位：就绪。
        fs::write(component.join("onnxruntime.dll"), b"ort").expect("stub 写入");
        assert!(super::xberg_layout_ready(&component).is_ok());
    }

    // 覆盖 XB-10：snap 清单的组件包成员 install_path（资产根相对，带
    // xberg-inference/<tag>/ 前缀）转换为 markdown 侧安装所需的组件目录相对
    // 路径。回归反证：修复前转换原样复制前缀，组件嵌套落位到
    // xberg-inference/<tag>/xberg-inference/<tag>/，初始化报「缺少 xberg.exe」。
    #[test]
    fn inference_manifest_from_pack_strips_tag_prefix() {
        let pack = super::SnapInferencePack {
            tag: "vtest-tag".to_string(),
            asset: super::SnapAsset {
                id: "xberg-inference".to_string(),
                url: "https://fixtures.invalid/pack.zip".to_string(),
                archive_type: "zip".to_string(),
                install_path: None,
                size_bytes: 42,
                sha256: "ab".repeat(32),
                members: vec![super::SnapMember {
                    path: "pkg/xberg.exe".to_string(),
                    install_path: "xberg-inference/vtest-tag/xberg.exe".to_string(),
                    size_bytes: 7,
                    sha256: "cd".repeat(32),
                }],
                license: super::SnapLicense {
                    component: "Xberg".to_string(),
                    license: "MIT".to_string(),
                    source: "https://fixtures.invalid".to_string(),
                },
            },
        };
        let inference = super::inference_manifest_from_pack(&pack).expect("合法成员前缀必须可转换");
        assert_eq!(inference.tag, "vtest-tag");
        assert_eq!(inference.members.len(), 1);
        let member = &inference.members[0];
        assert_eq!(member.path, "pkg/xberg.exe");
        assert_eq!(
            member.install_path, "xberg.exe",
            "install_path 必须剥离 xberg-inference/<tag>/ 前缀（组件目录相对）"
        );
    }

    // 覆盖 C-2 配套：成员 install_path 未落在 xberg-inference/<tag>/ 之下时，
    // 转换必须报「推理组件清单损坏」错误，不得静默回退原路径（组件目录校验
    // 口径下回退会嵌套出错误目录）。前缀存在性由 load_manifest 校验保证，
    // 此处为防伪造清单的守卫路径。
    #[test]
    fn inference_manifest_from_pack_rejects_member_outside_tag_prefix() {
        let mut pack = super::SnapInferencePack {
            tag: "vtest-tag".to_string(),
            asset: super::SnapAsset {
                id: "xberg-inference".to_string(),
                url: "https://fixtures.invalid/pack.zip".to_string(),
                archive_type: "zip".to_string(),
                install_path: None,
                size_bytes: 42,
                sha256: "ab".repeat(32),
                members: vec![super::SnapMember {
                    path: "pkg/xberg.exe".to_string(),
                    install_path: "xberg-inference/vtest-tag/xberg.exe".to_string(),
                    size_bytes: 7,
                    sha256: "cd".repeat(32),
                }],
                license: super::SnapLicense {
                    component: "Xberg".to_string(),
                    license: "MIT".to_string(),
                    source: "https://fixtures.invalid".to_string(),
                },
            },
        };
        pack.asset.members[0].install_path = "elsewhere/xberg.exe".to_string();
        let error =
            super::inference_manifest_from_pack(&pack).expect_err("越界成员必须报清单损坏错误");
        assert!(
            error.contains("推理组件清单损坏") && error.contains("elsewhere/xberg.exe"),
            "错误应说明清单损坏并点名成员：{error}"
        );
    }

    // ── 测试环境守卫：snap 侧覆盖变量与 markdown_assets 共用同一把进程环境锁 ──

    /// 测试共享进程环境变量（cargo test 并行线程），组件根相关用例必须串行；
    /// 锁实例与 markdown_assets 的测试共用（见 asset_util::test_env），避免
    /// 两模块同时改写各自资产根覆盖变量相互覆盖。
    struct ComponentGuard {
        root: tempfile::TempDir,
        /// 进入用例前的覆盖变量旧值：drop 时恢复而不是无条件删除——GUI 测试
        /// 装配设置的隔离根必须在本用例结束后仍然生效（C'-1，监督线程常驻，
        /// 任意时刻可能读资产根与派生管道名）。
        previous: Option<std::ffi::OsString>,
        /// bundled worker 覆盖旧值（S7-02 用例设置）：drop 时同样恢复，避免
        /// 泄漏到同进程其他用例的 bundled_worker() 读取。
        previous_worker: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    fn redirect_component_env() -> ComponentGuard {
        let lock = crate::asset_util::test_env::env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::env::var_os("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT");
        let previous_worker = std::env::var_os("JCHTOOLS_TEST_BUNDLED_WORKER");
        let root = tempfile::tempdir().expect("创建资产根目录");
        std::env::set_var("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT", root.path());
        ComponentGuard {
            root,
            previous,
            previous_worker,
            _lock: lock,
        }
    }

    impl Drop for ComponentGuard {
        fn drop(&mut self) {
            match self.previous.clone() {
                Some(value) => {
                    std::env::set_var("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT", value);
                }
                None => std::env::remove_var("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT"),
            }
            match self.previous_worker.clone() {
                Some(value) => {
                    std::env::set_var("JCHTOOLS_TEST_BUNDLED_WORKER", value);
                }
                None => std::env::remove_var("JCHTOOLS_TEST_BUNDLED_WORKER"),
            }
        }
    }

    // 覆盖 C-2（XB-09 2026-10-02 修订后的存在性口径）：设置页保存的组件目录
    // 下，推理组件成员检查必须基于 resolve_xberg_component 解析出的组件目录，
    // 而非资产根相对 install_path——否则配置目录永远无法就绪（在位校验通过后
    // 必报「推理组件包校验失败」）。
    // 覆盖树内全部 snapshot 成员以桩字节（与清单摘要不同）在场：存在性口径下
    // 必须通过；删除成员后错误基于保存目录点名缺失项。
    #[test]
    fn readiness_inference_pack_honors_configured_component_dir() {
        let guard = redirect_component_env();
        let external = guard.root.path().join("external-component");
        let manifest = super::load_manifest().expect("内置清单必须可解析");
        let inference = super::inference_manifest_from_pack(
            manifest
                .xberg_inference
                .as_ref()
                .expect("内置清单已接入推理组件段"),
        )
        .expect("推理组件段必须可转换");
        for member in &inference.members {
            if !crate::xberg_runtime::asset_for_scenario(&member.install_path, "snapshot") {
                continue;
            }
            let target = external.join(&member.install_path);
            fs::create_dir_all(target.parent().expect("成员路径有父目录"))
                .expect("创建覆盖树成员目录");
            fs::write(&target, b"replaced-engine").expect("预置桩成员（字节与清单不同）");
        }
        fs::write(external.join("xberg.exe"), b"engine").expect("预置引擎文件供设置链校验");
        crate::xberg_settings::save(&external).expect("经设置链保存组件目录");

        super::readiness_inference_pack(&manifest, guard.root.path())
            .expect("桩字节成员应通过存在性检查（替换引擎免摘要校验）");

        fs::remove_file(
            external
                .join("models")
                .join("snapshot-ocr")
                .join("rec.onnx"),
        )
        .expect("删除一个截图模型");
        let error = super::readiness_inference_pack(&manifest, guard.root.path())
            .expect_err("缺失成员必须失败");
        assert!(
            error.contains("rec.onnx") && (error.contains("缺失") || error.contains("缺少")),
            "错误应基于保存目录点名缺失成员（在位校验与清单成员检查均为存在性口径）：{error}"
        );
        assert!(
            !error.contains("推理组件包校验失败"),
            "成员检查不得再走资产根相对路径（C-2）：{error}"
        );
    }

    fn sha256_bytes(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }

    // ── S7-02/S7-05：就绪检查的完整性校验与失败原因透传 ──
    // 覆盖 O-09：自动启动直接取得 worker 路径，缓存副本必须先通过完整性校验。
    #[test]
    fn worker_install_path_rejects_unverified_cached_worker() {
        let guard = redirect_component_env();
        std::env::remove_var("JCHTOOLS_TEST_BUNDLED_WORKER");
        let manifest = super::load_manifest().expect("内置清单必须可解析");
        let target = guard.root.path().join(&manifest.workers[0].install_path);

        let missing = super::worker_install_path().expect_err("缺失缓存 worker 不得交给启动链");
        assert!(missing.contains("工作进程"), "必须点名工作进程：{missing}");

        fs::create_dir_all(target.parent().expect("worker 路径有父目录"))
            .expect("创建缓存 worker 目录");
        fs::write(&target, b"unverified-worker").expect("预置未校验缓存 worker");
        let corrupted = super::worker_install_path().expect_err("损坏缓存 worker 不得交给启动链");
        assert!(
            corrupted.contains("校验失败"),
            "必须说明缓存 worker 校验失败：{corrupted}"
        );
    }

    // 覆盖 O-09（S7-02）：bundled worker 文件在场但损坏（与清单大小/摘要不符）时，
    // 就绪检查的工作进程段必须与启动链同口径报「未就绪」并携带具体原因——修复前
    // 该分支只检查 path.is_file()，损坏 worker 在 readiness 报已就绪、启动链才失败。
    // 内置清单当前为 pending-build，readiness() 会在更早的占位检查处短路，无法端到
    // 端触达 bundled 分支；本用例以合成 worker 直接驱动工作进程段，与启动链共用
    // verify_bundled_worker 判据保证同口径。
    #[test]
    fn worker_readiness_rejects_corrupted_bundled_worker() {
        let guard = redirect_component_env();
        let worker_dir = tempfile::tempdir().expect("创建 bundled worker 目录");
        let worker_path = worker_dir.path().join("snap-ocr-worker.exe");
        fs::write(&worker_path, b"corrupted-worker").expect("写入损坏的 worker");
        std::env::set_var("JCHTOOLS_TEST_BUNDLED_WORKER", &worker_path);

        let worker = super::SnapWorker {
            id: "snap-ocr-worker".to_string(),
            status: "ok".to_string(),
            url: "https://example.invalid/snap-ocr-worker.exe".to_string(),
            archive_type: "file".to_string(),
            install_path: "worker/v0.1.2/snap-ocr-worker.exe".to_string(),
            // 大小与摘要均为「正确内容」的期望值：实际文件是另一种字节（损坏）。
            size_bytes: b"healthy-worker".len() as u64,
            sha256: sha256_bytes(b"healthy-worker"),
            members: vec![],
            license: super::SnapLicense {
                component: "worker".to_string(),
                license: "MIT".to_string(),
                source: "https://example.invalid".to_string(),
            },
        };

        // 大小不符：必须报未就绪，且错误点名大小差异。
        let error = super::worker_readiness(&worker, guard.root.path())
            .expect_err("损坏的 bundled worker 必须在就绪检查报未就绪");
        assert!(
            error.contains("校验失败") || error.contains("未就绪"),
            "提示必须明确指出工作进程未通过校验：{error}"
        );
        assert!(
            error.contains("大小") || error.contains("SHA256"),
            "提示必须携带具体失败原因（大小或摘要）：{error}"
        );

        // 大小正确、摘要不符：错误改为点名摘要差异（两种失败可区分）。
        fs::write(&worker_path, b"healthy-workXr").expect("写入摘要不符的 worker");
        let error = super::worker_readiness(&worker, guard.root.path())
            .expect_err("摘要不符的 bundled worker 必须报未就绪");
        assert!(
            error.contains("SHA256") && error.contains("预期"),
            "摘要不符必须点名摘要差异：{error}"
        );

        // 内容正确的 bundled worker：通过（就绪判定不得误伤完好文件）。
        fs::write(&worker_path, b"healthy-worker").expect("写入完好的 worker");
        super::worker_readiness(&worker, guard.root.path())
            .expect("完好的 bundled worker 必须通过就绪检查");
    }

    // 覆盖 O-09/O-30（S7-05）：readiness 对资产的具体失败原因（缺失、大小不符、
    // 摘要不符）必须透传到返回文案，不得压成统一的「未安装或校验失败」——修复前
    // GUI 只能显示压缩文案，失败种类丢失。以首个资产（字体 zip 成员）驱动三类。
    #[test]
    fn readiness_reports_distinct_asset_failure_kinds() {
        let guard = redirect_component_env();
        let mut manifest = super::load_manifest().expect("内置清单必须可解析");
        // 内置字体不再检查缓存；用非内置资产继续锁定失败原因透传。
        manifest.assets[0].id = "external-test-asset".into();
        let font = &manifest.assets[0];
        let member = &font.members[0];
        let target = guard.root.path().join(&member.install_path);

        // 缺失：点名资产 id，且不得出现压缩文案或大小/摘要标记。
        let missing = super::local_asset_readiness(&manifest.assets, guard.root.path())
            .expect_err("非内置资产缺失必须报未就绪");
        assert!(missing.contains(&font.id), "必须点名失败资产：{missing}");
        assert!(
            !missing.contains("未安装或校验失败"),
            "压缩文案必须被具体原因取代：{missing}"
        );
        assert!(
            !missing.contains("大小") && !missing.contains("SHA256"),
            "缺失类不得混入大小/摘要标记：{missing}"
        );

        // 大小不符：透传「大小 X，预期 Y」。
        fs::create_dir_all(target.parent().expect("成员路径有父目录")).expect("创建字体目录");
        fs::write(&target, b"too-short").expect("写入过短的字体成员");
        let sized = super::local_asset_readiness(&manifest.assets, guard.root.path())
            .expect_err("大小不符必须报未就绪");
        assert!(
            sized.contains("大小") && sized.contains(&member.size_bytes.to_string()),
            "必须透传大小原因：{sized}"
        );
        assert!(sized.contains(&font.id), "必须点名失败资产：{sized}");

        // 摘要不符（大小正确、内容错误）：透传「SHA256 ...，预期 ...」。
        {
            use std::io::Write as _;
            let size = usize::try_from(member.size_bytes).expect("清单大小必须可转换");
            let mut file = fs::File::create(&target).expect("创建字体成员文件");
            file.write_all(&vec![0_u8; size])
                .expect("写入内容错误的字体成员");
        }
        let digest = super::local_asset_readiness(&manifest.assets, guard.root.path())
            .expect_err("摘要不符必须报未就绪");
        assert!(
            digest.contains("SHA256") && digest.contains(&member.sha256),
            "必须透传摘要原因：{digest}"
        );
    }

    // ── C-3：下载中断重试 ──

    // 覆盖 O-05/XB-25：主程序的新装检查不要求外部字体缓存。
    #[test]
    fn local_readiness_needs_no_external_font_cache() {
        let root = tempfile::tempdir().expect("创建隔离缓存");
        let manifest = super::load_manifest().expect("解析清单");
        assert!(super::local_asset_readiness(&manifest.assets, root.path()).is_ok());
        assert!(root
            .path()
            .read_dir()
            .expect("读取隔离缓存")
            .next()
            .is_none());
    }

    // 覆盖 C-3：拉取中断（网络错误）必须与校验失败一样进入最多 3 次重试，
    // 不得首次中断即整体失败（约 291MB 组件包弱网一次中断即作废、.part 报废）。
    #[test]
    fn download_asset_retries_after_stream_interruption() {
        let root = tempfile::tempdir().expect("创建测试目录");
        let destination = root.path().join("asset.bin");
        let body = b"download-body".to_vec();
        let cancel = AtomicBool::new(false);
        let mut messages = Vec::new();
        let mut progress = |message: String| messages.push(message);
        let mut calls = 0_usize;
        let mut fetch = |partial: &Path, _progress: &mut dyn FnMut(String)| -> Result<(), String> {
            calls += 1;
            if calls < 3 {
                return Err("模拟网络中断".to_string());
            }
            fs::write(partial, &body).map_err(|error| error.to_string())
        };

        super::download_asset_with(
            &destination,
            &destination.with_extension("part"),
            body.len() as u64,
            &sha256_bytes(&body),
            &cancel,
            &mut progress,
            &mut fetch,
        )
        .expect("两次中断后第三次拉取应成功落位");

        assert_eq!(calls, 3, "中断后必须重试");
        assert_eq!(fs::read(&destination).expect("成功后必须原子落位"), body);
        assert_eq!(
            messages
                .iter()
                .filter(|message| message.contains("下载中断，准备重试"))
                .count(),
            2,
            "应恰好发出两条中断重试提示：{messages:?}"
        );
    }

    // 覆盖 C-3：三次拉取全部中断时返回包含最后一次错误的失败，产物与 .part
    // 均不残留。
    #[test]
    fn download_asset_fails_after_three_interruptions() {
        let root = tempfile::tempdir().expect("创建测试目录");
        let destination = root.path().join("asset.bin");
        let cancel = AtomicBool::new(false);
        let mut progress = |_message: String| {};
        let mut calls = 0_usize;
        let mut fetch =
            |_partial: &Path, _progress: &mut dyn FnMut(String)| -> Result<(), String> {
                calls += 1;
                Err("模拟网络中断".to_string())
            };

        let error = super::download_asset_with(
            &destination,
            &destination.with_extension("part"),
            8,
            &"ab".repeat(32),
            &cancel,
            &mut progress,
            &mut fetch,
        )
        .expect_err("三次中断必须失败");

        assert_eq!(calls, 3, "重试次数上限为 3");
        assert!(
            error.contains("模拟网络中断"),
            "失败信息应包含最后一次错误：{error}"
        );
        assert!(!destination.exists(), "失败不得落位产物");
        assert!(
            !destination.with_extension("part").exists(),
            "失败后 .part 必须清理"
        );
    }

    // ── B-1/B-2：staging 清理降级与残留兜底清理（与 markdown 侧同口径）──

    // 覆盖 B-1：staging 清理失败时，已成功的关键变更不得被误报为失败（生产
    // 触发：防护软件/索引器短暂持有 staging 内文件句柄，如推理组件包
    // download.zip 约 291MB 长下载后解包阶段的句柄）。注入方式：staging
    // 路径本身是普通文件时 remove_dir_all 确定性失败，无需平台句柄。
    #[test]
    fn finalize_staging_cleanup_failure_downgrades_to_warning() {
        let root = tempfile::tempdir().expect("创建测试目录");
        let staging = root.path().join(".staging-fake");
        fs::write(&staging, b"not-a-dir").expect("把 staging 预置为文件");
        let mut warnings = Vec::new();
        let mut progress = |message: String| warnings.push(message);

        let result = super::finalize_staging(Ok(()), &staging, &mut progress);

        assert!(result.is_ok(), "清理失败不得吞掉成功结果：{result:?}");
        assert_eq!(
            warnings
                .iter()
                .filter(|message| message.contains("警告：清理初始化临时目录失败"))
                .count(),
            1,
            "应恰好发出一条清理失败警告：{warnings:?}"
        );
    }

    // 覆盖 B-1 配套不变量：主结果失败时，清理失败不得覆盖原始错误
    //（本用例为守护性断言，修复前后都应通过）。
    #[test]
    fn finalize_staging_keeps_original_error_over_cleanup_failure() {
        let root = tempfile::tempdir().expect("创建测试目录");
        let staging = root.path().join(".staging-fake");
        fs::write(&staging, b"not-a-dir").expect("把 staging 预置为文件");
        let mut progress = |_message: String| {};

        let result = super::finalize_staging(Err("原始失败".to_string()), &staging, &mut progress);

        assert_eq!(
            result.expect_err("原始错误必须保留"),
            "原始失败",
            "清理失败不得覆盖原始错误"
        );
    }

    // 覆盖 B-2：历史残留的 .staging-* 目录必须在 initialize 入口被兜底清理。
    // 残留来源：readiness 提前返回、取消后清理失败、进程崩溃——修复前仅当次
    // 收尾清理，入口不清理任何残留（download.zip 残留可达约 291MB）。
    // 注入方式：cancel 预置为 true，initialize 在入口清理后即被取消返回，
    // 不触网、不建新 staging，本用例只断言入口清理。
    #[test]
    fn initialize_cleans_stale_staging_residue_at_entry() {
        let guard = redirect_component_env();
        let root = guard.root.path();
        let residue = root.join(".staging-fake");
        fs::create_dir_all(residue.join("xberg-inference")).expect("预置残留目录");
        fs::write(
            residue.join("xberg-inference").join("download.zip"),
            b"residue",
        )
        .expect("预置残留文件");

        let cancel = AtomicBool::new(true);
        let _ = super::initialize(&cancel, |_message: String| {});

        assert!(
            !residue.exists(),
            "残留 staging 目录必须在 initialize 入口被兜底清理（B-2）"
        );
    }

    // 覆盖 B-2：单个残留目录清理失败（如被防护软件锁定）必须跳过继续，不得
    // 影响其余残留清理，也不得让初始化整体失败。注入方式：对残留内文件持有
    // 无 FILE_SHARE_DELETE 的句柄，remove_dir_all 稳定失败（目录不涉及改名，
    // 句柄方案可行；对照 asset_util 的目录原子替换用例）。cancel 预置为 true
    // 使初始化在入口清理后立即取消，避免真实联网。
    // 平台门禁原因：共享模式句柄为 Windows 独有注入；无门禁的入口清理用例
    // 覆盖主路径（本项目按 P-07 仅支持 Windows，门禁不影响生产平台覆盖）。
    #[cfg(windows)]
    #[test]
    fn initialize_skips_undeletable_stale_staging_and_continues() {
        let guard = redirect_component_env();
        let root = guard.root.path();
        let locked = root.join(".staging-locked");
        fs::create_dir_all(&locked).expect("预置被锁残留目录");
        fs::write(locked.join("inner.txt"), b"x").expect("预置锁定文件");
        let normal = root.join(".staging-normal");
        fs::create_dir_all(&normal).expect("预置普通残留目录");
        fs::write(normal.join("inner.txt"), b"x").expect("预置普通残留文件");
        {
            use std::os::windows::fs::OpenOptionsExt;
            let _handle = fs::OpenOptions::new()
                .read(true)
                .share_mode(3)
                .open(locked.join("inner.txt"))
                .expect("持有无共享删除句柄");

            let cancel = AtomicBool::new(true);
            let _ = super::initialize(&cancel, |_message: String| {});
        }
        assert!(!normal.exists(), "普通残留必须被清理");
        assert!(
            locked.exists(),
            "被锁残留必须跳过（清理尽力而为，不阻塞初始化）"
        );
        let remaining: Vec<String> = fs::read_dir(root)
            .expect("枚举资产根")
            .flatten()
            .filter_map(|entry| {
                entry
                    .file_name()
                    .into_string()
                    .ok()
                    .filter(|name| name.starts_with(".staging-"))
            })
            .collect();
        assert_eq!(
            remaining,
            vec![".staging-locked".to_string()],
            "除被锁残留外不得留下其他 .staging-* 条目"
        );
    }

    // 覆盖 B-2 扩展：write_expected_tag 写入失败/崩溃残留的
    // xberg-inference/.expected-tag-<uuid> 临时文件必须在 initialize 入口被
    // 兜底清扫——修复前入口只清 .staging-*，该残留永久滞留；.old-* 备份不在
    // 清扫范围（由原子替换/prune 路径管理），必须保留。
    #[test]
    fn initialize_cleans_stale_expected_tag_residue_at_entry() {
        let guard = redirect_component_env();
        let root = guard.root.path();
        let inference = root.join("xberg-inference");
        fs::create_dir_all(&inference).expect("预置推理组件根目录");
        let residue = inference.join(".expected-tag-deadbeef");
        fs::write(&residue, b"v1\n").expect("预置 expected-tag 残留临时文件");
        let keep_backup = inference.join(".old-deadbeef");
        fs::create_dir_all(&keep_backup).expect("预置旧备份残留");

        let cancel = AtomicBool::new(true);
        let _ = super::initialize(&cancel, |_message: String| {});

        assert!(
            !residue.exists(),
            "残留的 .expected-tag-* 临时文件必须在 initialize 入口被兜底清理（B-2）"
        );
        assert!(
            keep_backup.exists(),
            ".old-* 备份不在初始化入口清扫范围（由原子替换路径管理），必须保留"
        );
    }

    // 覆盖 O-11 许可口径：notice 必须列出推理组件包的许可条目并指向安装目录
    //（与 markdown 侧「许可随组件树自带」口径对称）——修复前只遍历
    // assets + workers，不含 xberg_inference。
    #[test]
    fn notice_lists_xberg_inference_license() {
        let manifest = super::SnapAssetManifest {
            schema_version: 1,
            assets: vec![],
            workers: vec![],
            xberg_inference: Some(super::SnapInferencePack {
                tag: "vtest-tag".to_string(),
                asset: super::SnapAsset {
                    id: "xberg-inference".to_string(),
                    url: "https://fixtures.invalid/pack.zip".to_string(),
                    archive_type: "zip".to_string(),
                    install_path: None,
                    size_bytes: 42,
                    sha256: "ab".repeat(32),
                    members: vec![],
                    license: super::SnapLicense {
                        component: "Xberg 推理组件".to_string(),
                        license: "MIT".to_string(),
                        source: "https://fixtures.invalid".to_string(),
                    },
                },
            }),
        };
        let root = tempfile::tempdir().expect("创建测试目录");
        let notice = root.path().join("THIRD_PARTY_NOTICES.md");
        super::write_notice(&notice, &manifest).expect("写入 notice");
        let text = fs::read_to_string(&notice).expect("读取 notice");
        assert!(
            text.contains("Xberg 推理组件") && text.contains("MIT"),
            "notice 必须列出推理组件包的许可条目：{text}"
        );
        assert!(
            text.contains("xberg-inference"),
            "推理组件条目应指向安装目录 xberg-inference/：{text}"
        );
    }
    // P-09: 重定向到 ProxyOverride 命中目标时必须直连，不能继承首跳代理。
    #[test]
    fn redirect_rechecks_proxy_override_for_each_target() {
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};
        use std::time::Instant;

        fn accept_with_timeout(listener: &TcpListener) -> TcpStream {
            listener
                .set_nonblocking(true)
                .expect("设置本地测试监听器非阻塞");
            let deadline = Instant::now() + std::time::Duration::from_secs(3);
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        // Windows 接收的连接可能继承监听器非阻塞模式；HTTP 读取
                        // 使用下方 read_timeout，不以请求恰好已到达来掩盖调度竞态。
                        stream.set_nonblocking(false).expect("设置测试连接阻塞读取");
                        return stream;
                    }
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(error) => panic!("等待本地 HTTP 请求失败：{error}"),
                }
            }
        }

        fn read_request(stream: &mut TcpStream) -> String {
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                .expect("设置本地 HTTP 读取超时");
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 512];
            loop {
                let count = stream.read(&mut buffer).expect("读取本地 HTTP 请求");
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..count]);
                if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            String::from_utf8_lossy(&bytes).into_owned()
        }

        let target_listener =
            TcpListener::bind(("127.0.0.1", 0)).expect("启动直连重定向目标监听器");
        let target_addr = target_listener.local_addr().expect("读取目标监听地址");
        let body = b"redirected-asset";
        let target = std::thread::spawn(move || {
            let mut stream = accept_with_timeout(&target_listener);
            let request = read_request(&mut stream);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("发送直连响应头");
            stream.write_all(body).expect("发送直连响应内容");
            request
        });

        let proxy_listener = TcpListener::bind(("127.0.0.1", 0)).expect("启动系统代理模拟监听器");
        let proxy_addr = proxy_listener.local_addr().expect("读取代理监听地址");
        let redirect_url = format!("http://127.0.0.1:{}/asset", target_addr.port());
        let proxy = std::thread::spawn(move || {
            let mut stream = accept_with_timeout(&proxy_listener);
            let request = read_request(&mut stream);
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: {redirect_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream
                .write_all(response.as_bytes())
                .expect("发送代理重定向响应");
            request
        });

        let proxy_setting = format!("http=127.0.0.1:{}", proxy_addr.port());
        let system_proxy = crate::system_proxy::SystemProxy::from_registry_values(
            1,
            Some(&proxy_setting),
            Some("127.0.0.1"),
        );
        let source_url = "http://origin.example/asset";
        let target_url = format!("http://127.0.0.1:{}/asset", target_addr.port());
        assert!(system_proxy.endpoint_for_url(source_url).is_some());
        assert_eq!(system_proxy.endpoint_for_url(&target_url), None);

        let temp = tempfile::tempdir().expect("创建临时下载目录");
        let partial = temp.path().join("redirect.part");
        let cancel = AtomicBool::new(false);
        let mut progress = |_message: String| {};
        let result = super::download_attempt(
            source_url,
            &partial,
            u64::try_from(body.len()).expect("测试资产大小可转换"),
            &cancel,
            &mut progress,
            &system_proxy,
        );

        let proxy_request = proxy.join().expect("代理线程完成");
        let target_request = target.join().expect("目标线程完成");
        assert!(
            proxy_request.starts_with("GET http://origin.example/asset HTTP/1.1"),
            "首跳应经系统代理：{proxy_request}"
        );
        assert!(
            target_request.starts_with("GET /asset HTTP/1.1"),
            "重定向目标应直接请求：{target_request}"
        );
        result.expect("逐跳按 ProxyOverride 路由后下载成功");
        assert_eq!(fs::read(partial).expect("读取下载结果"), body);
    }
}
