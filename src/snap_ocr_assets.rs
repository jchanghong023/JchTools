//! 截图 OCR（O 分区）的可选资产初始化与就绪检查。
//!
//! 资产清单（resources/snap-ocr-assets.json）编译进主程序：结果窗专用字体为
//! 固定版本、固定来源、固定 SHA-256（O-05/O-09）；识别用的模型、推理运行库与
//! `xberg.exe` 由固定版本 Xberg 推理组件承接，安装在状态目录的
//! snap-ocr/xberg-inference/<tag>/ 子树（摘要级清单条目待发布 tag 落定后接入）。
//! 只在用户于图形界面主动初始化时联网下载（O-06/O-10），主程序包不携带这些重资产。
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
/// Xberg 推理组件按「在位校验」检查（存在性）；其摘要清单接入前缺失时如实
/// 报告「推理组件未配置」，不冒称就绪。
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
    xberg_inference_ready(&root)?;
    // 清单接入推理组件包后做成员级摘要校验（XB-09；未接入时在位校验已覆盖）。
    if let Some(pack) = &manifest.xberg_inference {
        asset_ready(&pack.asset, &root)
            .map_err(|error| format!("推理组件包校验失败：{error}"))?;
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
    crate::markdown_assets::resolve_component_with_tag(
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
    // 推理组件包（XB-10）：复用 markdown 侧的整目录原子安装路径（staging 组装 +
    // 成员级复核 + 目录级原子落位 + 旧版本清理），失败不留部分安装。
    if let Some(pack) = &manifest.xberg_inference {
        ensure_not_cancelled(cancel)?;
        let inference = crate::markdown_assets::InferenceManifest {
            tag: pack.tag.clone(),
            url: pack.asset.url.clone(),
            size_bytes: pack.asset.size_bytes,
            sha256: pack.asset.sha256.clone(),
            members: pack
                .asset
                .members
                .iter()
                .map(|member| crate::markdown_assets::InferenceMember {
                    path: member.path.clone(),
                    install_path: member.install_path.clone(),
                    size_bytes: member.size_bytes,
                    sha256: member.sha256.clone(),
                })
                .collect(),
        };
        if crate::markdown_assets::inference_ready(&inference, root).is_err() {
            progress(format!("下载资产 {}/{}：{}", total + 1, total + 1, pack.asset.id));
        }
        let mut adapter = PackDownloaderAdapter(downloader);
        crate::markdown_assets::install_inference_pack(
            &inference,
            staging,
            root,
            cancel,
            &mut adapter,
            progress,
        )?;
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

/// 把 snap 侧下载接缝适配为共享安装路径的下载器（同一签名，零行为差异）。
struct PackDownloaderAdapter<'a>(&'a mut dyn SnapDownloader);

impl crate::markdown_assets::AssetDownloader for PackDownloaderAdapter<'_> {
    fn download(
        &mut self,
        url: &str,
        destination: &Path,
        expected_size: u64,
        expected_sha256: &str,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(String),
    ) -> Result<(), String> {
        self.0
            .download(url, destination, expected_size, expected_sha256, cancel, progress)
    }
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

pub(crate) fn extract_zip_safely(
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

#[cfg(test)]
mod tests {
    use std::fs;

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
}
