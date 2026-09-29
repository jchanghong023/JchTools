//! 截图 OCR（O 分区）的可选资产初始化与就绪检查。
//!
//! 资产清单（resources/snap-ocr-assets.json）编译进主程序：结果窗专用字体为
//! 固定版本、固定来源、固定 SHA-256（O-05/O-09）；识别用的模型、推理运行库与
//! `xberg.exe` 由固定版本 Xberg 推理组件承接，安装在状态目录的
//! `snap-ocr/xberg-inference/<tag>/` 子树（摘要级清单条目待发布 tag 落定后接入）。
//! 只在用户于图形界面主动初始化时联网下载（O-06/O-10），主程序包不携带这些重资产。
//! 初始化遵循 staging → 校验 → 原子落位：取消或失败删除本轮 staging，不覆盖
//! 已经校验通过的完整资产；重试时已验证资产直接复用，不重复下载。
//! 下载/校验/原子落位与推理组件包安装核心与转 Markdown 共用
//! [`crate::asset_util`]，两侧行为同源。

use crate::asset_util::{
    atomic_replace_dir, atomic_replace_file, ensure_not_cancelled, extract_zip_safely,
    install_inference_pack, validate_relative_path, verify_file, AssetDownloader,
    InferenceManifest,
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

/// 测试资产根覆盖（按 `debug_assertions` 门禁；注意本仓 release profile 同样
/// 开启 debug-assertions，故发布构建中也生效——变量以测试命名、单用户本地
/// 工具，实际风险可控，C'-1）：`JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT`
/// 指向绝对路径时，资产根与截图服务管道名（见 [`pipe_name`]）一并脱离生产
/// 位置——无头测试因此既不会 spawn 真实 worker（readiness 对隔离根必然
/// 失败），也不会向真实用户会话的服务管道发送请求（生产名可被真实服务应答，
/// attach-main-exe 会改写其 launcher.json）。
fn test_asset_root_override() -> Option<PathBuf> {
    if !cfg!(debug_assertions) {
        return None;
    }
    std::env::var_os("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// 本功能资产根目录：用户状态目录下的 snap-ocr/（模型、字体与 worker 的缓存落盘
/// 属于 O-06 明确允许的资产写入，与截图/识别内容无关）。
pub fn asset_root() -> PathBuf {
    if let Some(path) = test_asset_root_override() {
        return path;
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
/// 测试资产根覆盖生效时改用派生的测试专用名（C'-1，见 [`test_asset_root_override`]）。
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

/// 只读检查所有已安装资产：不联网、不创建目录、不修改文件（O-09 加载前离线验证）。
/// worker 条目仍为构建期占位时按未就绪报告，并说明原因（不冒称就绪，O-11）。
/// Xberg 推理组件按「在位校验」检查（存在性）；其摘要清单接入前缺失时如实
/// 报告「推理组件未配置」，不冒称就绪。
/// 后台工作进程的安装路径（取清单条目的 install_path；升版只改清单）。
pub fn worker_install_path() -> Result<PathBuf, String> {
    let manifest = load_manifest()?;
    let worker = manifest
        .workers
        .first()
        .ok_or_else(|| "截图 OCR 工作进程清单缺失".to_string())?;
    Ok(asset_root().join(&worker.install_path))
}

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
    readiness_inference_pack(&manifest, &root)
}

/// 就绪检查的推理组件段：在位校验 + 清单成员校验。
///
/// 成员校验基于 [`resolve_xberg_component`] 解析出的组件目录（C-2）：该解析
/// 支持 debug 构建的 `JCHTOOLS_XBERG_INFERENCE_DIR` 覆盖，成员校验必须与在位
/// 校验使用同一目录——此前成员校验直接按资产根相对 install_path 进行，绕过
/// 覆盖，导致覆盖路径在位校验通过后必报「推理组件包校验失败」、永远无法就绪。
fn readiness_inference_pack(manifest: &SnapAssetManifest, root: &Path) -> Result<(), String> {
    let component = xberg_inference_ready(root)?;
    // 清单接入推理组件包后做成员级摘要校验（XB-09；未接入时在位校验已覆盖）。
    if let Some(pack) = &manifest.xberg_inference {
        let inference = inference_manifest_from_pack(pack)?;
        crate::asset_util::inference_layout_ready(&component, &inference)?;
    }
    Ok(())
}

/// Xberg 推理组件安装根：`<资产根>/xberg-inference/`（每个发布版本一个 tag 子目录）。
#[must_use]
pub fn xberg_inference_root() -> PathBuf {
    asset_root().join("xberg-inference")
}

/// 组件目录解析：开发期（仅 debug 构建）可用 `JCHTOOLS_XBERG_INFERENCE_DIR`
/// 覆盖到本地组件树；否则按可选的清单 tag 解析唯一子目录（XB-09：目录名必须
/// 与清单一致，不一致明确报错并指引更新；未接入时沿用「唯一子目录」启发式）。
fn resolve_xberg_component(root: &Path) -> Result<PathBuf, String> {
    if cfg!(debug_assertions) {
        if let Some(path) = std::env::var_os("JCHTOOLS_XBERG_INFERENCE_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
        {
            return Ok(path);
        }
    }
    let expected_tag = manifest_inference_tag();
    crate::asset_util::resolve_component_with_tag(
        &root.join("xberg-inference"),
        expected_tag.as_deref(),
        &xberg_not_configured(),
    )
}

/// 清单接入推理组件包时返回其 tag（读取失败按未接入处理）。
fn manifest_inference_tag() -> Option<String> {
    load_manifest()
        .ok()
        .and_then(|manifest| manifest.xberg_inference.map(|pack| pack.tag))
}

fn xberg_not_configured() -> String {
    if manifest_inference_tag().is_some() {
        "推理组件未配置：Xberg 推理组件（xberg.exe、截图模型与 onnxruntime）尚未安装；\
     请在截图 OCR 功能页重新初始化以下载推理组件包"
            .into()
    } else {
        "推理组件未配置：Xberg 推理组件（xberg.exe、截图模型与 onnxruntime）尚未安装；\
     其下载清单条目待发布版本落定后接入，开发期可设置 JCHTOOLS_XBERG_INFERENCE_DIR \
     指向本地组件目录"
            .into()
    }
}

/// Xberg 推理组件的在位校验（存在性；摘要校验待清单接入后补齐，O-09 的
/// 完整校验由后续清单条目承接）：
/// `xberg.exe` + `models/snapshot-ocr/{det.onnx,rec.onnx,dict/dict.txt}` + `onnxruntime.dll`。
pub fn xberg_inference_ready(root: &Path) -> Result<PathBuf, String> {
    let component = resolve_xberg_component(root)?;
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
    for path in &required {
        if !path.is_file() {
            let relative = path.strip_prefix(component).unwrap_or(path);
            return Err(format!(
                "推理组件不完整：缺少 {}（组件目录 {}）",
                relative.display(),
                component.display()
            ));
        }
    }
    Ok(())
}

/// 下载、校验并原子安装全部可选资产（O-06）。
///
/// 取消会终止当前下载或解包阶段并删除本轮 staging 目录；最终位置已校验的资产
/// 跨重试复用、不重下；失败不覆盖已经校验的完整资产。下载接缝与生产下载器
/// 共用 [`crate::asset_util::AssetDownloader`] / [`crate::asset_util::NetworkDownloader`]，
/// 测试可注入本地供给验证「补缺下载」与「失败后重试不重下」。
pub fn initialize(cancel: &AtomicBool, mut progress: impl FnMut(String)) -> Result<(), String> {
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
        &mut progress,
        &staging,
        &root,
        &mut downloader,
    );
    finalize_staging(result, &staging, &mut progress)
}

/// 初始化收尾：删除本轮 staging 目录，主结果优先返回。
///
/// staging 清理是尽力而为（B-1，与 markdown 侧同口径）：关键变更已成功时
/// 清理失败只经 progress 发出警告、维持 Ok（此前 Windows 上防护软件/索引器
/// 短暂持有 staging 内文件句柄即把成功初始化误报为失败）；残留目录由下次
/// initialize 入口的 [`cleanup_stale_staging`] 兜底收集。
fn finalize_staging(
    result: Result<(), String>,
    staging: &Path,
    progress: &mut dyn FnMut(String),
) -> Result<(), String> {
    if let Err(error) = fs::remove_dir_all(staging) {
        if result.is_ok() {
            progress(format!("警告：清理初始化临时目录失败：{error}"));
        }
    }
    result
}

/// 兜底清理资产根下历史残留的 `.staging-*` 目录（B-2，与 markdown 侧同口径），
/// 单项失败跳过继续。
///
/// 并发前提：主程序单实例，初始化由 gui.rs 的 snap_initializing 守卫串行
///（单初始化线程），入口处发现的 `.staging-*` 必为历史残留，不存在在途
/// staging 被误删的并发窗口。`.staging-*` 前缀的非目录条目不是本流程产物
///（staging 恒为目录），按设计跳过；资产根不存在（首次运行）时无需清理。
fn cleanup_stale_staging(root: &Path) {
    // 资产根尚不存在（首次运行）时无需清理。
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_staging = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".staging-"));
        if is_staging && path.is_dir() {
            // 尽力而为：残留被防护软件短暂锁定时跳过，下次初始化再试。
            let _ = fs::remove_dir_all(&path);
        }
    }
}

fn initialize_staged(
    manifest: &SnapAssetManifest,
    cancel: &AtomicBool,
    progress: &mut impl FnMut(String),
    staging: &Path,
    root: &Path,
    downloader: &mut dyn AssetDownloader,
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
    // 推理组件包（XB-10）：复用共享的整目录原子安装路径（staging 组装 +
    // 成员级复核 + 目录级原子落位 + 旧版本清理），失败不留部分安装。
    if let Some(pack) = &manifest.xberg_inference {
        ensure_not_cancelled(cancel)?;
        let inference = inference_manifest_from_pack(pack)?;
        if crate::asset_util::inference_ready(&inference, root).is_err() {
            progress(format!(
                "下载资产 {}/{}：{}",
                total + 1,
                total + 1,
                pack.asset.id
            ));
        }
        install_inference_pack(&inference, staging, root, cancel, downloader, progress)?;
        // XB-09：写入清单 tag 标记；截图服务进程（无法读主程序清单）按同口径
        // 校验组件目录版本。
        write_expected_tag(root, &pack.tag)?;
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
            total + 2,
            total + 2,
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
    downloader: &mut dyn AssetDownloader,
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
fn write_expected_tag(root: &Path, tag: &str) -> Result<(), String> {
    let base = root.join("xberg-inference");
    fs::create_dir_all(&base).map_err(|error| format!("创建推理组件根目录失败：{error}"))?;
    let marker = base.join("expected-tag.txt");
    let temporary = base.join(format!(".expected-tag-{}", Uuid::new_v4().simple()));
    fs::write(&temporary, format!("{tag}\n"))
        .map_err(|error| format!("写入组件版本标记失败：{error}"))?;
    atomic_replace_file(&temporary, &marker)
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
    if let Some(pack) = &manifest.xberg_inference {
        let tag_ok = !pack.tag.is_empty()
            && !pack.tag.contains('/')
            && !pack.tag.contains('\\')
            && !pack.tag.contains("..")
            && !pack.tag.contains(':');
        if !tag_ok {
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
    progress: &mut impl FnMut(String),
) -> Result<(), String> {
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
    progress: &mut impl FnMut(String),
    fetch: &mut F,
) -> Result<(), String>
where
    F: FnMut(&Path, &mut dyn FnMut(String)) -> Result<(), String>,
{
    for attempt in 1..=3 {
        ensure_not_cancelled(cancel)?;
        if let Err(error) = fetch(partial, progress) {
            // 取消优先于重试语义：用户取消时不发重试提示，直接收场。
            ensure_not_cancelled(cancel)?;
            if attempt < 3 {
                progress(format!("下载中断，准备重试（{attempt}/3）：{error}"));
                let _ = fs::remove_file(partial);
                continue;
            }
            let _ = fs::remove_file(partial);
            return Err(format!("下载资产失败：{error}"));
        }
        match verify_file(partial, expected_size, expected_sha256) {
            Ok(()) => {
                fs::rename(partial, destination)
                    .map_err(|error| format!("写入下载资产失败：{error}"))?;
                return Ok(());
            }
            Err(error) if attempt < 3 => {
                progress(format!("资产校验失败，准备重试（{attempt}/3）：{error}"));
                let _ = fs::remove_file(partial);
            }
            Err(error) => {
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
    let agent = ureq::builder()
        .redirects(3)
        // 连接超时 + 单次读超时，整体时长由用户取消控制。
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
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::AtomicBool;

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
    /// 两模块同时改写 JCHTOOLS_XBERG_INFERENCE_DIR 相互覆盖。
    struct ComponentGuard {
        root: tempfile::TempDir,
        /// 进入用例前的覆盖变量旧值：drop 时恢复而不是无条件删除——GUI 测试
        /// 装配设置的隔离根必须在本用例结束后仍然生效（C'-1，监督线程常驻，
        /// 任意时刻可能读资产根与派生管道名）。
        previous: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    fn redirect_component_env() -> ComponentGuard {
        let lock = crate::asset_util::test_env::env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::env::var_os("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT");
        let root = tempfile::tempdir().expect("创建资产根目录");
        std::env::set_var("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT", root.path());
        std::env::remove_var("JCHTOOLS_XBERG_INFERENCE_DIR");
        ComponentGuard {
            root,
            previous,
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
            std::env::remove_var("JCHTOOLS_XBERG_INFERENCE_DIR");
        }
    }

    // 覆盖 C-2：debug 组件目录覆盖（JCHTOOLS_XBERG_INFERENCE_DIR）下，推理
    // 组件包成员校验必须基于 resolve_xberg_component 解析出的组件目录，而非
    // 资产根相对 install_path——否则覆盖路径永远无法就绪（在位校验通过后
    // 必报「推理组件包校验失败」）。
    // 断言强度说明：清单成员摘要对应真实大文件（xberg.exe 约 105MB），测试
    // 无法伪造同摘要字节，故以「错误来自组件目录级成员校验、点名的实际大小
    // 取自覆盖树桩文件」证明校验路径已切换；覆盖树下真实摘要全绿路径未验证。
    #[test]
    fn readiness_inference_pack_honors_component_dir_override() {
        let guard = redirect_component_env();
        let external = guard.root.path().join("external-component");
        let models = external.join("models").join("snapshot-ocr");
        fs::create_dir_all(models.join("dict")).expect("创建桩组件模型目录");
        fs::write(external.join("xberg.exe"), b"stub").expect("预置桩 xberg.exe");
        fs::write(models.join("det.onnx"), b"det").expect("预置桩检测模型");
        fs::write(models.join("rec.onnx"), b"rec").expect("预置桩识别模型");
        fs::write(models.join("dict").join("dict.txt"), b"dict").expect("预置桩字典");
        fs::write(external.join("onnxruntime.dll"), b"ort").expect("预置桩运行库");
        std::env::set_var("JCHTOOLS_XBERG_INFERENCE_DIR", &external);

        let manifest = super::load_manifest().expect("内置清单必须可解析");
        let error = super::readiness_inference_pack(&manifest, guard.root.path())
            .expect_err("桩文件摘要与清单不符必须失败");

        assert!(
            !error.contains("推理组件包校验失败"),
            "成员校验不得再走资产根相对路径（C-2）：{error}"
        );
        assert!(
            error.contains("推理组件成员 xberg.exe") && error.contains("大小 4，预期"),
            "错误应来自组件目录级成员校验并点名桩文件实际大小：{error}"
        );
    }

    // ── C-3：下载中断重试 ──

    fn sha256_bytes(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
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
}
