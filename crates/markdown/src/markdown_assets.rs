//! 转 Markdown 的本地资产校验。三个独立功能包共用一个 SQLite 运行目录。
//! 下载按最新正式发布的组件清单核验来源、归档和成员；添加功能复用同版本
//! 已验证组件，升级同时迁移已安装功能。旧用户目录保持只读兼容。
//! 下载成功及离线组件检查经 [`ensure_document_notice`] 原子补写许可证 notice。
//! P-10：主动查询最新发布与下载校验落位分别创建关联 operation span；日志
//! 保留元数据验证、缓存复用、暂存、校验、原子落位、回滚及来源保存的阶段与结果，
//! 不记录发布 JSON、运行目录内容、完整路径或任意外部异常正文。

use jchtools_assets::asset_util::{
    atomic_replace_dir, cleanup_stale_staging_dirs, ensure_not_cancelled, finalize_staging,
    require_component_members, require_inference_members_for_scenario, resolve_xberg_component,
    state_dir_asset_root, valid_component_tag, validate_relative_path, verify_file_with_cancel,
    AssetDownloader, InferenceManifest,
};
use jchtools_foundation::xberg_settings::DownloadSelection;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as FmtWrite;
use std::fs::{self, File, OpenOptions};
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use uuid::Uuid;

const MANIFEST: &str = include_str!("../../../resources/markdown-assets.json");
const DATA_DIRECTORY: &str = "markdown-assets";
pub(crate) const XBERG_TAG: &str = "v2026.10.6-0420-run58.1";
const RUNTIME_SELECTION_FILE: &str = "xberg-runtime-path.txt";
const XBERG_DOWNLOAD_STAGING_MARKER: &str = ".jchtools-xberg-download-staging-v1";
const XBERG_RELEASE_RECEIPT: &str = ".jchtools-xberg-release.json";
const XBERG_LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/jchanghong023/xberg/releases/latest";
const XBERG_TARGET: &str = "x86_64-pc-windows-msvc";
const XBERG_COMPONENT_MANIFEST: &str = "xberg-components-x86_64-pc-windows-msvc.json";
const XBERG_ARCHIVE_PREFIX: &str = "xberg-cli-x86_64-pc-windows-msvc";
const COMPONENT_IDS: [&str; 3] = ["document", "snapshot", "media"];

#[derive(Debug, Deserialize)]
struct AssetManifest {
    schema_version: u32,
    xberg: XbergManifest,
    /// Xberg 推理组件包（XB-10 双轨：媒体转录所需的 xberg.exe、模型与原生
    /// 运行库，来源为固定发布 zip）。清单未接入该条目时为 None，组件缺失
    /// 时如实报告未配置。
    xberg_inference: Option<InferenceManifest>,
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

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
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

/// 读取应用级 SQLite 保存的共享 Xberg 运行目录。
pub fn load_saved_runtime_dir() -> Result<Option<PathBuf>, String> {
    jchtools_foundation::xberg_settings::load()
}

/// 校验已下载的 Xberg 运行目录：只做 document 场景成员存在性检查
/// （T-06 / XB-09：不比对大小与 SHA-256，用户可自行替换或更新引擎版本），
/// 缺失时汇总指认缺失项。
pub fn validate_runtime_dir(path: &Path) -> Result<(), String> {
    let manifest = load_manifest()?;
    if !path.is_dir() {
        return Err(format!(
            "Xberg 运行目录不存在或不是目录：{}",
            path.display()
        ));
    }
    let mut missing = Vec::new();
    for member in &manifest.xberg.members {
        if !jchtools_xberg::xberg_runtime::asset_for_scenario(&member.path, "document") {
            continue;
        }
        if !path.join(&member.path).is_file() {
            missing.push(member.path.clone());
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Xberg 运行目录缺失必需文件（{} 项）：{}",
            missing.len(),
            missing.join("；")
        ))
    }
}

/// 保存共享目录到 SQLite；各功能启动前分别校验其模型和运行库。
///
/// 保存成功后同步闭合文档场景就绪链（T-06/XB-19，S3-01 集成：转换页初始
/// 化入口移除后这里是 notice 的生产写点之一）：notice 缺失时原子补写。
/// notice 写入失败不回滚已保存的目录，但错误文本明确告知保存已生效，
/// 避免用户把有效目录当坏目录（目录本身有效，仅记录写入受阻）。
pub fn save_runtime_dir(path: &Path) -> Result<(), String> {
    jchtools_foundation::xberg_settings::save(path)?;
    ensure_document_notice().map_err(|error| {
        format!(
            "目录已保存，但许可证 notice 写入失败（文档场景就绪检查会被阻断，可重试保存）：{error}"
        )
    })
}

/// 确保文档场景就绪链的许可证 notice 已落盘（T-06）。
///
/// 已存在时幂等返回；缺失时经 staging 目录原子就位（与私有 `initialize_staged`
/// 同口径），半途失败不留半成品、不破坏既有文件——非原子直写曾在复审中
/// 被指出可能在磁盘满/杀软锁时截断旧 notice，把原本有效的安装打成
/// 「notice 不存在」。设置页保存与下载共用此闭环。
pub fn ensure_document_notice() -> Result<(), String> {
    let manifest = load_manifest()?;
    let root = asset_root();
    let target = root.join("licenses").join("THIRD_PARTY_NOTICES.md");
    if target.is_file() {
        return Ok(());
    }
    let staging = root.join(format!(".notice-staging-{}", Uuid::new_v4().simple()));
    fs::create_dir_all(&staging).map_err(|error| format!("创建许可证临时目录失败：{error}"))?;
    let result = write_notice(&staging.join("THIRD_PARTY_NOTICES.md"), &manifest)
        .and_then(|()| atomic_replace_dir(&staging, &root.join("licenses")));
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}
/// 返回已保存的 Xberg 运行目录；未下载组件时提示在设置页选择功能。
pub fn runtime_dir() -> Result<PathBuf, String> {
    load_saved_runtime_dir()?
        .ok_or_else(|| "尚未下载 Xberg 功能组件，请在设置页选择并下载".to_string())
}

/// Xberg 推理组件安装根：`<资产根>/xberg-inference/`（每个发布版本一个 tag
/// 子目录）。与截图 OCR 各自独立安装（O-03：互不覆盖，只读复用同一发布源）。
#[must_use]
pub fn xberg_inference_root() -> PathBuf {
    asset_root().join("xberg-inference")
}

/// 组件目录解析规则与就绪成员检查共用 [`jchtools_assets::asset_util`] 的共享实现
/// （同一安装只能有一个解析口径，XB-09/XB-19）。
///
/// 媒体转录组件的在位校验（存在性；XB-09 2026-10-02 修订后运行时不比对
/// 摘要，与截图 OCR 侧口径一致）：`xberg.exe` + SenseVoice/VAD 模型 +
/// sherpa-onnx 四 DLL + FFmpeg 四 DLL。返回解析出的组件目录供转录进程注入
/// 环境变量。
pub fn media_component_dir() -> Result<PathBuf, String> {
    let component = resolve_xberg_component()?;
    let required = [
        component.join("xberg.exe"),
        component
            .join("models")
            .join("sense_voice_zh_en_ja_ko_yue_2024_07_17")
            .join("model.int8.onnx"),
        component
            .join("models")
            .join("sense_voice_zh_en_ja_ko_yue_2024_07_17")
            .join("tokens.txt"),
        component.join("models").join("vad").join("silero_vad.onnx"),
        component.join("sherpa-onnx").join("sherpa-onnx-c-api.dll"),
        component
            .join("sherpa-onnx")
            .join("sherpa-onnx-cxx-api.dll"),
        component.join("sherpa-onnx").join("onnxruntime.dll"),
        component
            .join("sherpa-onnx")
            .join("onnxruntime_providers_shared.dll"),
        component.join("ffmpeg").join("avutil-61.dll"),
        component.join("ffmpeg").join("swresample-7.dll"),
        component.join("ffmpeg").join("avcodec-63.dll"),
        component.join("ffmpeg").join("avformat-63.dll"),
    ];
    require_component_members(&component, &required)?;
    Ok(component)
}

/// 使用媒体能力前校验它独有的固定资产；文档初始化不要求媒体模型。
pub fn validate_media() -> Result<(), String> {
    readiness_inference_pack(&load_manifest()?, &asset_root())
}

/// 检查文档场景及许可证；读取应用配置时可首次迁移旧文本，不联网。
pub fn readiness() -> Result<(), String> {
    // 先校验内置清单本身：失效清单不得被当作可运行环境。
    load_manifest()?;
    let runtime = runtime_dir()?;
    validate_runtime_dir(&runtime)?;
    let notice = asset_root().join("licenses").join("THIRD_PARTY_NOTICES.md");
    if !notice.is_file() {
        return Err(format!("许可证 notice 不存在：{}", notice.display()));
    }
    Ok(())
}

/// 就绪检查的推理组件段：成员级存在性检查（XB-09 2026-10-02 修订；清单
/// 未接入时在位校验已覆盖）。
///
/// 成员检查基于共享解析规则（C-2，与截图 OCR 侧同口径）：组件目录只来自
/// 应用 SQLite 保存的共享 Xberg 目录（设置页保存或产品内下载；2026-10-04
/// 起不再有环境变量覆盖），成员检查必须与在位校验
/// （[`media_component_dir`]）使用同一目录——此前成员校验直接按资产根拼
/// `xberg-inference/<tag>/`，绕过共享解析，导致配置目录在位校验通过后仍必报
/// 成员校验失败、永远无法就绪（markdown::run 阻断转换，initialize 还会重复
/// 下载约 291MB 组件包）。markdown 清单成员的 install_path 本就是组件目录相对
/// 路径（无 `xberg-inference/<tag>/` 前缀，与 snap 清单不同），成员过滤与
/// 存在性检查经 [`jchtools_assets::asset_util::require_inference_members_for_scenario`]
/// 与截图侧共用同一实现。`_root` 形参保留调用点形状；组件目录一律由共享
/// 解析规则提供（其删除属基线再生成事项，另行确认）。
fn readiness_inference_pack(manifest: &AssetManifest, _root: &Path) -> Result<(), String> {
    if let Some(inference) = &manifest.xberg_inference {
        let component = resolve_xberg_component()?;
        require_inference_members_for_scenario(&component, "media", &inference.members)?;
    }
    Ok(())
}

/// 校验用户运行目录并写入许可证 notice；不下载 Xberg（XB-10）。
///
/// 组件缺失时返回明确错误并指引
/// （不冒称就绪）。取消会删除本轮 staging 目录。
pub fn initialize(cancel: &AtomicBool, mut progress: impl FnMut(String)) -> Result<(), String> {
    let span = jchtools_foundation::logging::operation_span("markdown_assets", "initialize");
    let _entered = span.enter();
    let started = std::time::Instant::now();
    tracing::info!(
        event = "markdown_assets_initialize_started",
        "转 Markdown 组件初始化开始"
    );
    let mut stage = "prepare";
    let result = (|| {
        let root = asset_root();
        // B-2：先兜底清理历史残留的 staging（readiness 提前返回、取消后清理
        // 失败或进程崩溃都会残留 .staging-<uuid>，单个可超 1GB）。
        cleanup_stale_staging(&root);
        ensure_not_cancelled(cancel)?;
        if readiness().is_ok() {
            tracing::debug!(
                event = "markdown_assets_reused",
                result = "already_ready",
                "转 Markdown 组件已就绪"
            );
            progress("转 Markdown 组件已就绪".to_string());
            return Ok(());
        }
        stage = "manifest";
        let manifest = load_manifest()?;
        stage = "create_root";
        fs::create_dir_all(&root).map_err(|error| format!("创建资产目录失败：{error}"))?;
        let staging = root.join(format!(".staging-{}", Uuid::new_v4().simple()));
        stage = "create_staging";
        fs::create_dir_all(&staging).map_err(|error| format!("创建初始化临时目录失败：{error}"))?;
        stage = "runtime_validate_notice_install";
        let result = initialize_staged(&manifest, cancel, &mut progress, &staging, &root);
        stage = "finalize_staging";
        finalize_staging(result, &staging, &mut progress)
    })();
    let elapsed_ms = jchtools_foundation::logging::elapsed_ms(started);
    match &result {
        Ok(()) => tracing::info!(
            event = "markdown_assets_initialize_completed",
            elapsed_ms,
            result = "ready",
            "转 Markdown 组件初始化完成"
        ),
        Err(_) if cancel.load(std::sync::atomic::Ordering::Acquire) => tracing::info!(
            event = "markdown_assets_initialize_cancelled",
            stage,
            elapsed_ms,
            "转 Markdown 组件初始化已取消"
        ),
        Err(_) => tracing::error!(
            event = "markdown_assets_initialize_failed",
            stage,
            elapsed_ms,
            "转 Markdown 组件初始化失败"
        ),
    }
    result
}

/// 兜底清理资产根下历史残留的 `.staging-*` 目录与 `.xberg-runtime-path.txt.*`
/// 临时文件（B-2），单项失败跳过继续。
///
/// `.staging-*` 清扫与截图 OCR 侧共用
/// [`jchtools_assets::asset_util::cleanup_stale_staging_dirs`]；本模块只保留自己的残留策略。
///
/// 不扫 `.old-*` 备份：那是原子替换路径的暂存（asset_util），其中
/// xberg-inference/<tag> 目标的残留按设计由 prune_old_inference_tags 收集
///（该安装链当前仅在测试中启用，见 asset_util；生产初始化只校验共享目录，
/// XB-10），入口一概删除会把替换失败后仍可恢复的备份提前清掉。
fn cleanup_stale_staging(root: &Path) {
    cleanup_stale_staging_dirs(root);
    // save_runtime_dir 的临时选择文件（.xberg-runtime-path.txt.<uuid>）在写入
    // 或原子就位失败/进程崩溃时残留（文件非目录），与 .staging-* 同属入口兜底
    // 清扫。
    let selection_temp_prefix = format!(".{RUNTIME_SELECTION_FILE}.");
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(event = "runtime_selection_cleanup_failed", stage = "read_directory", error_kind = ?error.kind(), error_code = error.raw_os_error(), "读取运行目录选择临时文件目录失败");
            }
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(event = "runtime_selection_cleanup_failed", stage = "read_entry", error_kind = ?error.kind(), error_code = error.raw_os_error(), "读取运行目录选择临时文件条目失败，跳过此项");
                continue;
            }
        };
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with(&selection_temp_prefix) && path.is_file() {
            // 尽力而为：同 .staging-*，单项失败跳过。
            if let Err(error) = fs::remove_file(&path) {
                tracing::warn!(event = "runtime_selection_cleanup_failed", stage = "remove_selection_temp", error_kind = ?error.kind(), error_code = error.raw_os_error(), "删除运行目录选择临时文件失败，留待下次重试");
            }
        }
    }
}

fn initialize_staged(
    manifest: &AssetManifest,
    cancel: &AtomicBool,
    progress: &mut impl FnMut(String),
    staging: &Path,
    root: &Path,
) -> Result<(), String> {
    let started = std::time::Instant::now();
    let mut stage = "runtime_directory";
    let result = (|| {
        ensure_not_cancelled(cancel)?;
        // 已下载组件缺失时如实报告；转 Markdown 初始化不自动补装功能组件。
        let runtime = runtime_dir()?;
        stage = "runtime_validate";
        validate_runtime_dir(&runtime)?;
        let notice_stage = staging.join("licenses");
        stage = "notice_create";
        fs::create_dir_all(&notice_stage)
            .map_err(|error| format!("创建许可证目录失败：{error}"))?;
        stage = "notice_write";
        write_notice(&notice_stage.join("THIRD_PARTY_NOTICES.md"), manifest)?;
        // T-05：校验或 notice 写入期间的取消必须在最终落位前生效。
        ensure_not_cancelled(cancel)?;
        stage = "notice_install";
        atomic_replace_dir(&notice_stage, &root.join("licenses"))?;
        // XB-10：Xberg 功能组件由设置页独立下载，本阶段只检查已有目录并生成 notice。
        progress("转 Markdown 组件初始化完成".to_string());
        Ok(())
    })();
    match &result {
        Ok(()) => tracing::debug!(
            event = "markdown_assets_staged_completed",
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            result = "validated_notice_installed",
            "转 Markdown 运行目录校验与许可证落位完成"
        ),
        Err(_) if cancel.load(std::sync::atomic::Ordering::Acquire) => tracing::info!(
            component = "markdown_assets",
            event = "markdown_assets_staged_cancelled",
            stage,
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "转 Markdown 运行目录校验与许可证落位已取消"
        ),
        Err(_) => tracing::error!(
            event = "markdown_assets_staged_failed",
            stage,
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "转 Markdown 运行目录校验或许可证落位失败"
        ),
    }
    result
}

/// 生产下载器：只访问清单固定地址（T-21），HTTP 原语复用截图 OCR 侧的
/// 流式下载器（P-03 联网边界测试限制 `ureq` 只出现在两个资产模块内）。
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
        jchtools_assets::snap_ocr_assets::download_asset(
            url,
            destination,
            expected_size,
            expected_sha256,
            cancel,
            &mut sink,
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct ReleaseAsset {
    archive: String,
    url: String,
    size_bytes: u64,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct FeatureComponent {
    id: String,
    archive: String,
    requires: Vec<String>,
    size_bytes: u64,
    sha256: String,
    members: Vec<AssetFile>,
}

#[derive(Debug, Deserialize)]
struct ComponentManifest {
    schema_version: u32,
    target: String,
    components: Vec<FeatureComponent>,
}

#[derive(Debug, Deserialize, Serialize)]
struct ComponentReceipt {
    owner: String,
    tag: String,
    source: String,
    manifest: ReleaseAsset,
    components: Vec<FeatureComponent>,
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn release_asset(
    release: &serde_json::Value,
    tag: &str,
    name: &str,
) -> Result<ReleaseAsset, String> {
    let assets = release["assets"]
        .as_array()
        .ok_or("最新 Xberg 发布缺少资产列表")?;
    let matches: Vec<_> = assets
        .iter()
        .filter(|asset| asset["name"] == name)
        .collect();
    if matches.len() != 1 {
        return Err(format!(
            "最新 Xberg 发布缺少或重复独立组件资产：{name}；不会回退完整包"
        ));
    }
    let asset = matches[0];
    let url = format!("https://github.com/jchanghong023/xberg/releases/download/{tag}/{name}");
    if asset["browser_download_url"].as_str() != Some(url.as_str()) {
        return Err("最新 Xberg 组件来源与固定官方源不一致".into());
    }
    let size_bytes = asset["size"]
        .as_u64()
        .filter(|size| *size > 0)
        .ok_or("最新 Xberg 组件大小无效")?;
    let sha256 = asset["digest"]
        .as_str()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .filter(|digest| valid_digest(digest))
        .ok_or("最新 Xberg 组件缺少上游 SHA-256，未开始下载")?
        .to_ascii_lowercase();
    Ok(ReleaseAsset {
        archive: name.into(),
        url,
        size_bytes,
        sha256,
    })
}

fn release_components(
    release: &serde_json::Value,
) -> Result<(String, ReleaseAsset, BTreeMap<String, ReleaseAsset>), String> {
    let tag = release["tag_name"]
        .as_str()
        .filter(|tag| valid_component_tag(tag))
        .ok_or("最新 Xberg 发布缺少合法版本标识")?;
    if release["draft"] != false || release["prerelease"] != false {
        return Err("最新发布接口未返回正式发布版本，未开始下载".into());
    }
    let manifest = release_asset(release, tag, XBERG_COMPONENT_MANIFEST)?;
    let mut components = BTreeMap::new();
    for id in COMPONENT_IDS {
        let name = format!("xberg-{id}-{XBERG_TARGET}.zip");
        components.insert(id.into(), release_asset(release, tag, &name)?);
    }
    Ok((tag.into(), manifest, components))
}

fn member_owner(path: &str) -> &'static str {
    if path.starts_with("models/snapshot-ocr/") {
        "snapshot"
    } else if path.starts_with("models/models--") {
        "document"
    } else if path.starts_with("models/sense_voice")
        || path.starts_with("models/vad/")
        || path.starts_with("ffmpeg/")
        || path.starts_with("sherpa-onnx/")
        || path.starts_with("samples/")
    {
        "media"
    } else {
        "common"
    }
}

fn validate_component_manifest(
    manifest: &ComponentManifest,
    assets: &BTreeMap<String, ReleaseAsset>,
    pinned: &XbergManifest,
) -> Result<(), String> {
    if manifest.schema_version != 1
        || manifest.target != XBERG_TARGET
        || manifest.components.len() != 3
    {
        return Err("Xberg 独立组件清单版本、平台或组件数量无效".into());
    }
    let mut ids = BTreeSet::new();
    let mut paths: BTreeMap<String, &AssetFile> = BTreeMap::new();
    for component in &manifest.components {
        let asset = assets.get(&component.id).ok_or("Xberg 清单包含未知组件")?;
        let dependencies: Vec<String> = Vec::new();
        if !ids.insert(component.id.as_str())
            || component.requires != dependencies
            || component.archive != asset.archive
            || component.size_bytes != asset.size_bytes
            || component.sha256 != asset.sha256
            || component.members.is_empty()
        {
            return Err("Xberg 清单组件与正式发布资产不一致".into());
        }
        let mut component_paths = BTreeSet::new();
        for member in &component.members {
            validate_relative_path(&member.path)?;
            let normalized = member.path.to_ascii_lowercase();
            let owner = member_owner(&member.path);
            if member.path.contains('\\')
                || member.path == XBERG_RELEASE_RECEIPT
                || !valid_digest(&member.sha256)
                || !component_paths.insert(normalized.clone())
                || (owner != "common" && owner != component.id)
                || paths
                    .get(&normalized)
                    .is_some_and(|previous| *previous != member)
            {
                return Err(format!(
                    "Xberg 清单成员路径、摘要或组件归属无效：{}",
                    member.path
                ));
            }
            paths.insert(normalized, member);
        }
        for expected in &pinned.members {
            let owner = member_owner(&expected.path);
            if owner != "common" && owner != component.id {
                continue;
            }
            let actual = component
                .members
                .iter()
                .find(|member| member.path == expected.path)
                .ok_or_else(|| {
                    format!("Xberg {} 清单缺少所需成员：{}", component.id, expected.path)
                })?;
            if expected.path.starts_with("models/")
                && (actual.size_bytes != expected.size_bytes || actual.sha256 != expected.sha256)
            {
                return Err(format!(
                    "Xberg 发布模型身份与固定资产不一致：{}",
                    expected.path
                ));
            }
        }
    }
    Ok(())
}

/// 按功能获取同一正式发布的独立组件；不下载完整离线包、不修改旧运行目录。
/// 新安装与复用均返回规范化绝对目录，与保存的活动运行目录使用同一表示。
pub fn download_runtime(
    selection: DownloadSelection,
    cancel: &AtomicBool,
    mut progress: impl FnMut(String),
) -> Result<PathBuf, String> {
    let span = jchtools_foundation::logging::operation_span("markdown_assets", "download_runtime");
    let _entered = span.enter();
    ensure_selection(selection)?;
    ensure_not_cancelled(cancel)?;
    progress("正在查询 Xberg 最新发布版本…".into());
    let release = jchtools_assets::snap_ocr_assets::fetch_release_json(
        XBERG_LATEST_RELEASE_URL,
        cancel,
        &mut progress,
    )?;
    download_runtime_from_release(
        selection,
        &release,
        cancel,
        progress,
        &mut NetworkDownloader,
    )
}

fn ensure_selection(selection: DownloadSelection) -> Result<(), String> {
    if !selection.document && !selection.snapshot && !selection.media {
        return Err("请至少选择一种 Xberg 功能后下载".into());
    }
    Ok(())
}

/// 生产安装接缝：调用方可提供正式发布元数据与流式资产供给器。
/// 来源、清单、归档、成员、模型身份以及最终配置提交全部走生产校验。
/// 成功返回规范化绝对目录；新安装和已校验组件复用遵循相同路径表示。
pub fn download_runtime_from_release(
    selection: DownloadSelection,
    release: &serde_json::Value,
    cancel: &AtomicBool,
    progress: impl FnMut(String),
    downloader: &mut dyn AssetDownloader,
) -> Result<PathBuf, String> {
    let pinned = load_manifest()?;
    download_runtime_with_pinned(
        selection,
        release,
        cancel,
        progress,
        downloader,
        &pinned.xberg,
    )
}

fn download_runtime_with_pinned(
    selection: DownloadSelection,
    release: &serde_json::Value,
    cancel: &AtomicBool,
    mut progress: impl FnMut(String),
    downloader: &mut dyn AssetDownloader,
    pinned: &XbergManifest,
) -> Result<PathBuf, String> {
    let span = jchtools_foundation::logging::operation_span("markdown_assets", "install_runtime");
    let _entered = span.enter();
    ensure_selection(selection)?;
    ensure_not_cancelled(cancel)?;
    let (tag, manifest_asset, assets) = release_components(release)?;
    let base = jchtools_foundation::xberg_settings::state_dir()?.join("xberg-downloads");
    fs::create_dir_all(&base).map_err(|error| error.to_string())?;
    // 在 staging 和配置提交前统一路径表示，避免提交成功后再增加可能失败的操作。
    let base =
        fs::canonicalize(&base).map_err(|error| format!("解析 Xberg 下载目录失败：{error}"))?;
    cleanup_owned_download_staging(&base, &mut progress);
    let staging = base.join(format!(".staging-{}", Uuid::new_v4()));
    fs::create_dir(&staging).map_err(|error| error.to_string())?;
    let staging_lock = (|| {
        let mut file = open_download_staging_lock(&staging)?;
        file.write_all(b"JchTools Xberg download staging\n")
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        Ok::<_, String>(file)
    })();
    let staging_lock = match staging_lock {
        Ok(file) => file,
        Err(error) => {
            rollback_runtime_directory(&staging, "staging_lock");
            return Err(error);
        }
    };
    let result = (|| {
        progress(format!("最新发布版本：{tag}，正在获取独立组件清单…"));
        let path = staging.join("components.json");
        downloader.download(
            &manifest_asset.url,
            &path,
            manifest_asset.size_bytes,
            &manifest_asset.sha256,
            cancel,
            &mut progress,
        )?;
        verify_file_with_cancel(
            &path,
            manifest_asset.size_bytes,
            &manifest_asset.sha256,
            cancel,
        )?;
        let bytes = fs::read(path).map_err(|error| error.to_string())?;
        let manifest: ComponentManifest = serde_json::from_slice(&bytes)
            .map_err(|error| format!("Xberg 独立组件清单无效：{error}"))?;
        validate_component_manifest(&manifest, &assets, pinned)?;
        install_components(
            selection,
            &tag,
            &manifest_asset,
            &manifest,
            &base,
            &staging,
            cancel,
            &mut progress,
            downloader,
        )
    })();
    drop(staging_lock);
    if let Err(error) = fs::remove_dir_all(&staging) {
        tracing::warn!(event = "runtime_staging_cleanup_failed", error_kind = ?error.kind(), "下载暂存目录清理失败");
    }
    if result.is_ok() {
        tracing::info!(
            event = "runtime_install_completed",
            result = "verified_installed_saved",
            "Xberg 独立组件安装完成"
        );
    } else {
        tracing::warn!(
            event = "runtime_install_failed",
            cancelled = cancel.load(std::sync::atomic::Ordering::Acquire),
            "Xberg 独立组件安装未提交"
        );
    }
    result
}

fn owned_receipt(base: &Path) -> Result<Option<(PathBuf, ComponentReceipt)>, String> {
    let Some(path) = jchtools_foundation::xberg_settings::settings()?.downloaded else {
        return Ok(None);
    };
    let Ok(canonical_base) = fs::canonicalize(base) else {
        return Ok(None);
    };
    let Ok(path) = fs::canonicalize(path) else {
        return Ok(None);
    };
    // 只复用本流程创建的目录；旧文本/用户目录不会被复制、删除或改写。
    if path.parent() != Some(canonical_base.as_path()) {
        return Ok(None);
    }
    let receipt = fs::read(path.join(XBERG_RELEASE_RECEIPT))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<ComponentReceipt>(&bytes).ok());
    let Some(receipt) = receipt else {
        return Ok(None);
    };
    let suffix = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(&format!("{}-", receipt.tag)));
    if receipt.owner != "JchTools-xberg-components-v1"
        || receipt.source != XBERG_LATEST_RELEASE_URL
        || !valid_component_tag(&receipt.tag)
        || suffix.is_none_or(|suffix| Uuid::parse_str(suffix).is_err())
        || receipt.manifest.archive != XBERG_COMPONENT_MANIFEST
        || receipt.manifest.url
            != format!(
                "https://github.com/jchanghong023/xberg/releases/download/{}/{}",
                receipt.tag, XBERG_COMPONENT_MANIFEST
            )
    {
        return Ok(None);
    }
    let mut ids = BTreeSet::new();
    for component in &receipt.components {
        if !COMPONENT_IDS.contains(&component.id.as_str())
            || !ids.insert(component.id.as_str())
            || component.archive != format!("xberg-{}-{XBERG_TARGET}.zip", component.id)
        {
            return Ok(None);
        }
    }
    if ids.is_empty() {
        return Ok(None);
    }
    Ok(Some((path, receipt)))
}

fn verify_component(
    root: &Path,
    component: &FeatureComponent,
    cancel: &AtomicBool,
) -> Result<(), String> {
    for member in &component.members {
        ensure_not_cancelled(cancel)?;
        let path = root.join(&member.path);
        if fs::symlink_metadata(&path)
            .map_err(|error| error.to_string())?
            .file_type()
            .is_symlink()
        {
            return Err("已安装组件包含符号链接，不能复用".into());
        }
        verify_file_with_cancel(&path, member.size_bytes, &member.sha256, cancel)?;
    }
    Ok(())
}

fn collect_files(root: &Path, paths: &mut BTreeSet<String>, relative: &Path) -> Result<(), String> {
    for entry in fs::read_dir(root.join(relative)).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = relative.join(entry.file_name());
        let kind = entry.file_type().map_err(|error| error.to_string())?;
        if kind.is_dir() {
            collect_files(root, paths, &path)?;
        } else if kind.is_file() {
            let name = path
                .to_str()
                .ok_or("组件路径无法编码为 Unicode")?
                .replace('\\', "/");
            paths.insert(name);
        } else {
            return Err("组件归档包含非普通文件".into());
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn install_components(
    selection: DownloadSelection,
    tag: &str,
    manifest_asset: &ReleaseAsset,
    manifest: &ComponentManifest,
    base: &Path,
    staging: &Path,
    cancel: &AtomicBool,
    progress: &mut impl FnMut(String),
    downloader: &mut dyn AssetDownloader,
) -> Result<PathBuf, String> {
    let previous = owned_receipt(base)?;
    let mut wanted = BTreeSet::new();
    for (id, selected) in [
        ("document", selection.document),
        ("snapshot", selection.snapshot),
        ("media", selection.media),
    ] {
        if selected {
            wanted.insert(id);
        }
    }
    if let Some((_, receipt)) = &previous {
        if receipt.tag == tag {
            for component in &receipt.components {
                wanted.insert(component.id.as_str());
            }
        }
    }
    let components: Vec<_> = manifest
        .components
        .iter()
        .filter(|component| wanted.contains(component.id.as_str()))
        .cloned()
        .collect();
    let mut reusable = BTreeSet::new();
    if let Some((path, receipt)) = &previous {
        if receipt.tag == tag {
            for component in &components {
                if receipt.components.contains(component) {
                    match verify_component(path, component, cancel) {
                        Ok(()) => {
                            reusable.insert(component.id.as_str());
                        }
                        Err(_) => {
                            ensure_not_cancelled(cancel)?;
                        }
                    }
                }
            }
            if reusable.len() == components.len() {
                ensure_not_cancelled(cancel)?;
                ensure_document_notice()?;
                ensure_not_cancelled(cancel)?;
                jchtools_foundation::xberg_settings::save_source(
                    jchtools_foundation::xberg_settings::Source::Downloaded,
                    path,
                )?;
                progress(format!("已存在 {tag}，复用已校验组件，不重复下载"));
                return Ok(path.clone());
            }
        }
    }
    let runtime = staging.join("runtime");
    fs::create_dir(&runtime).map_err(|error| error.to_string())?;
    for component in &components {
        ensure_not_cancelled(cancel)?;
        if reusable.contains(component.id.as_str()) {
            let source = &previous.as_ref().ok_or("缺少可复用组件目录")?.0;
            for member in &component.members {
                ensure_not_cancelled(cancel)?;
                let destination = runtime.join(&member.path);
                if destination.exists() {
                    verify_file_with_cancel(
                        &destination,
                        member.size_bytes,
                        &member.sha256,
                        cancel,
                    )?;
                } else {
                    fs::create_dir_all(destination.parent().ok_or("组件路径缺少父目录")?)
                        .map_err(|error| error.to_string())?;
                    fs::copy(source.join(&member.path), &destination)
                        .map_err(|error| error.to_string())?;
                }
            }
            verify_component(&runtime, component, cancel)?;
            continue;
        }
        progress(format!("正在下载 Xberg {} 组件…", component.id));
        let archive = staging.join(&component.archive);
        let url = format!(
            "https://github.com/jchanghong023/xberg/releases/download/{tag}/{}",
            component.archive
        );
        downloader.download(
            &url,
            &archive,
            component.size_bytes,
            &component.sha256,
            cancel,
            progress,
        )?;
        verify_file_with_cancel(&archive, component.size_bytes, &component.sha256, cancel)?;
        let extracted = staging.join(format!("unpacked-{}", component.id));
        fs::create_dir(&extracted).map_err(|error| error.to_string())?;
        jchtools_assets::asset_util::extract_zip_safely(&archive, &extracted, cancel)?;
        let mut actual = BTreeSet::new();
        collect_files(&extracted, &mut actual, Path::new(""))?;
        let expected: BTreeSet<_> = component
            .members
            .iter()
            .map(|member| format!("{XBERG_ARCHIVE_PREFIX}/{}", member.path))
            .collect();
        if actual != expected {
            return Err(format!("Xberg {} 归档成员与清单不一致", component.id));
        }
        let unpacked = extracted.join(XBERG_ARCHIVE_PREFIX);
        verify_component(&unpacked, component, cancel)?;
        for member in &component.members {
            ensure_not_cancelled(cancel)?;
            let destination = runtime.join(&member.path);
            if destination.exists() {
                verify_file_with_cancel(&destination, member.size_bytes, &member.sha256, cancel)?;
            } else {
                fs::create_dir_all(destination.parent().ok_or("组件路径缺少父目录")?)
                    .map_err(|error| error.to_string())?;
                fs::rename(unpacked.join(&member.path), destination)
                    .map_err(|error| error.to_string())?;
            }
        }
    }
    let receipt = ComponentReceipt {
        owner: "JchTools-xberg-components-v1".into(),
        tag: tag.into(),
        source: XBERG_LATEST_RELEASE_URL.into(),
        manifest: manifest_asset.clone(),
        components,
    };
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(runtime.join(XBERG_RELEASE_RECEIPT))
        .map_err(|error| error.to_string())?;
    file.write_all(&serde_json::to_vec(&receipt).map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    drop(file);
    ensure_not_cancelled(cancel)?;
    let installed = base.join(format!("{tag}-{}", Uuid::new_v4()));
    fs::rename(&runtime, &installed).map_err(|error| format!("安装 Xberg 失败：{error}"))?;
    let commit = (|| {
        ensure_document_notice()?;
        ensure_not_cancelled(cancel)?;
        jchtools_foundation::xberg_settings::save_source(
            jchtools_foundation::xberg_settings::Source::Downloaded,
            &installed,
        )
    })();
    if let Err(error) = commit {
        rollback_runtime_directory(&installed, "notice_or_save_source");
        return Err(error);
    }
    Ok(installed)
}

fn rollback_runtime_directory(directory: &Path, stage: &'static str) {
    let started = std::time::Instant::now();
    tracing::warn!(
        event = "runtime_install_rollback_started",
        stage,
        action = "remove_new_directory",
        "Xberg 安装阶段失败，开始撤销本次目录"
    );
    match fs::remove_dir_all(directory) {
        Ok(()) => tracing::info!(
            event = "runtime_install_rollback_completed",
            stage,
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            result = "removed",
            "Xberg 本次目录已撤销"
        ),
        Err(error) => {
            tracing::warn!(event = "runtime_install_rollback_failed", stage, error_kind = ?error.kind(), error_code = error.raw_os_error(), error = %jchtools_foundation::logging::safe_error(&error.to_string()), elapsed_ms = jchtools_foundation::logging::elapsed_ms(started), "Xberg 本次目录撤销失败，保留既有错误处理策略");
        }
    }
}

fn open_download_staging_lock(staging: &Path) -> Result<File, String> {
    let marker = staging.join(XBERG_DOWNLOAD_STAGING_MARKER);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .share_mode(0b011)
            .open(&marker)
            .map_err(|error| format!("创建下载临时目录所有权标记失败：{error}"))
    }
    #[cfg(not(windows))]
    {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
            .map_err(|error| format!("创建下载临时目录所有权标记失败：{error}"))
    }
}

/// 只清理本流程创建并带有精确所有权标记的下载 staging 目录。
///
/// `xberg-downloads` 可能由用户自行创建或包含用户文件；没有标记的目录一律
/// 保留。有效下载版本目录不是 `.staging-*`，因此不会被触碰。
fn cleanup_owned_download_staging(root: &Path, progress: &mut impl FnMut(String)) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_staging = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".staging-"));
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !is_staging || !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let marker = path.join(XBERG_DOWNLOAD_STAGING_MARKER);
        let owned = fs::read_to_string(&marker)
            .is_ok_and(|value| value == "JchTools Xberg download staging\n");
        if owned {
            if let Err(error) = fs::remove_dir_all(&path) {
                tracing::warn!(event = "runtime_stale_staging_cleanup_failed", stage = "staging_remove", error_kind = ?error.kind(), error_code = error.raw_os_error(), "清理旧 Xberg 下载临时目录失败");
                progress(format!("警告：清理旧 Xberg 下载临时目录失败：{error}"));
            }
        }
    }
}

fn load_manifest() -> Result<AssetManifest, String> {
    parse_manifest(MANIFEST)
}

/// 解析并校验资产清单文本：各段自身完整性 + 段间交叉一致性。拆出纯文本
/// 入参是为了让篡改清单（段间漂移）可用单测覆盖（内置常量无法在运行期改写）。
fn parse_manifest(text: &str) -> Result<AssetManifest, String> {
    let manifest: AssetManifest =
        serde_json::from_str(text).map_err(|error| format!("转 Markdown 资产清单无效：{error}"))?;
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
    for member in &manifest.xberg.members {
        validate_relative_path(&member.path)?;
    }
    if let Some(inference) = &manifest.xberg_inference {
        if !valid_component_tag(&inference.tag)
            || inference.url.is_empty()
            || inference.size_bytes == 0
            || inference.sha256.len() != 64
        {
            return Err("Xberg 推理组件包元数据不完整".to_string());
        }
        if inference.members.is_empty() {
            return Err("Xberg 推理组件包成员清单为空".to_string());
        }
        for member in &inference.members {
            validate_relative_path(&member.path)?;
            validate_relative_path(&member.install_path)?;
        }
        // 段间交叉校验：两段描述同一发布物（设置页下载 xberg 段、媒体成员
        // 校验用 xberg_inference 段），四值必须一致；漂移会让下载 tag 与媒体
        // 成员校验口径分叉，按无效清单拒绝，不静默放行。
        let fixed = &manifest.xberg;
        if inference.tag != fixed.tag
            || inference.url != fixed.archive_url
            || inference.size_bytes != fixed.archive_size_bytes
            || inference.sha256 != fixed.archive_sha256
        {
            return Err(
                "资产清单 xberg 与 xberg_inference 段元数据不一致：两段必须指向同一发布物（tag/URL/大小/SHA-256）"
                    .to_string(),
            );
        }
    }
    Ok(manifest)
}

fn asset_root() -> PathBuf {
    if cfg!(test) || cfg!(feature = "test-hooks") {
        if let Some(path) = std::env::var_os("JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT") {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                return path;
            }
        }
    }
    state_dir_asset_root(DATA_DIRECTORY)
}

fn write_notice(path: &Path, manifest: &AssetManifest) -> Result<(), String> {
    let mut text = String::from("# JchTools 转 Markdown 可选组件许可证\n\n");
    text.push_str("Xberg 运行目录由共享配置提供；主程序安装包不包含这些资产。独立功能组件由设置页按需下载，模型与推理运行库的许可随组件树自带。\n\n");
    for license in &manifest.xberg.licenses {
        let _ = writeln!(
            &mut text,
            "- {}：{}，{}",
            license.component, license.license, license.source
        );
    }
    fs::write(path, text).map_err(|error| format!("写入许可证 notice 失败：{error}"))
}

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
#[cfg(test)]
mod tests {
    use super::{load_manifest, media_component_dir, write_notice};
    use jchtools_assets::asset_util::{
        inference_ready, install_inference_pack, resolve_component_with_tag,
        AssetDownloader, InferenceManifest, InferenceMember,
    };
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Mutex, MutexGuard};

    // 覆盖 T-06：内置清单必须可解析（媒体段退役后仅存 xberg 段）。
    #[test]
    fn manifest_parses_with_xberg_segment_only() {
        let manifest = load_manifest().expect("内置资产清单必须可解析");
        assert!(
            !manifest.xberg.members.is_empty(),
            "xberg 固定版本成员清单不得为空"
        );
    }

    #[test]
    fn cleanup_owned_download_staging_preserves_unowned_and_valid_versions() {
        let root = tempfile::tempdir().expect("创建测试目录");
        let owned = root.path().join(".staging-owned");
        fs::create_dir_all(&owned).expect("创建自有 staging");
        fs::write(
            owned.join(super::XBERG_DOWNLOAD_STAGING_MARKER),
            b"JchTools Xberg download staging\n",
        )
        .expect("写入自有 staging 标记");
        fs::write(owned.join("runtime.zip"), b"partial").expect("写入残留下载");

        let unowned = root.path().join(".staging-user");
        fs::create_dir_all(&unowned).expect("创建用户 staging");
        fs::write(unowned.join("user.txt"), b"keep").expect("写入用户文件");

        let version = root.path().join("v2026.10.6-0420-run58.1-uuid");
        fs::create_dir_all(&version).expect("创建有效版本目录");
        fs::write(version.join("xberg.exe"), b"valid").expect("写入有效版本");

        let mut progress = |_message: String| {};
        super::cleanup_owned_download_staging(root.path(), &mut progress);

        assert!(!owned.exists(), "带所有权标记的残留 staging 必须回收");
        assert!(unowned.exists(), "无所有权标记的目录不得删除");
        assert!(version.exists(), "有效下载版本目录不得删除");
    }

    // 覆盖 XB-19/T-05（回归：任务预检按所选分组分场景——纯媒体目录缺文档模型
    // 不再被文档条件提前拒绝；缺文档模型的文档任务启动前明确报错而不是静默放行
    // 后逐文件失败；page_readiness 允许任一场景可用即进入转换页）
    #[test]
    fn readiness_for_groups_checks_only_requested_scenarios() {
        // 平台门槛前置分支（T-03：转 Markdown 仅支持 Windows 11 x64，不满足时
        // 必须在启动前明确拒绝）。CI 与其他 Server 宿主（如 windows-2022 runner，
        // build 20348 / Server 产品类型）上 platform_preflight 必须短路——此时
        // 场景就绪组合不可观察，只断言平台错误如实透传；下方场景组合断言在
        // Win11 开发机（fastcheck/fulltest 的 cargo test）上照常执行，覆盖不缩小。
        // 背景：CI run 37128504702 在 Server runner 上因原测试无条件期待 Ok 而失败
        //（validate_media 通过、readiness_for_groups 因平台门槛报错），该期望与
        // T-03 相矛盾，故按合同纠正测试而非放宽产品行为。
        if crate::markdown::platform_preflight().is_err() {
            let platform_error =
                crate::markdown::readiness_for_groups(&[crate::markdown::FormatGroup::Media])
                    .expect_err("非 Windows 11 平台必须在启动前拒绝转换");
            assert!(
                platform_error.contains("Windows 11"),
                "平台错误必须如实说明原因：{platform_error}"
            );
            return;
        }
        let guard = redirect_component_env();
        let component = install_component(guard.root.path());
        // install_component 只建 media_component_dir 在位校验所需文件；
        // 清单成员检查还要求根级通用成员（engine 运行库与许可），一并补齐。
        for extra in [
            "onnxruntime.dll",
            "onnxruntime_providers_shared.dll",
            "LICENSE",
            "THIRD_PARTY_LICENSES.md",
            "MSVCP140.dll",
            "MSVCP140_1.dll",
            "VCRUNTIME140.dll",
            "VCRUNTIME140_1.dll",
            "ffmpeg/LICENSE.txt",
            "sherpa-onnx/LICENSE",
        ] {
            fs::write(component.join(extra), b"stub").expect("预置通用成员");
        }
        // 前置：媒体场景成员齐备，文档场景成员（models/models--… 文档模型）缺位。
        assert!(
            super::validate_media().is_ok(),
            "前置：媒体组件就绪（组件目录 {}）：{:?}",
            component.display(),
            super::validate_media()
        );
        // 纯媒体分组：不再被文档条件拒绝（修复前 run() 无条件 readiness()）。
        assert!(
            crate::markdown::readiness_for_groups(&[crate::markdown::FormatGroup::Media]).is_ok()
        );
        // 纯文档分组：启动前明确报文档组件未就绪，并指认缺失。
        let error = crate::markdown::readiness_for_groups(&[crate::markdown::FormatGroup::Pdf])
            .expect_err("缺文档模型的文档任务必须在启动前拒绝");
        assert!(
            error.contains("文档转换组件未就绪"),
            "错误必须区分场景并指认文档组件：{error}"
        );
        // 混选分组：两个场景都必须就绪，仍报未就绪的那个。
        let error = crate::markdown::readiness_for_groups(&[
            crate::markdown::FormatGroup::Pdf,
            crate::markdown::FormatGroup::Media,
        ])
        .expect_err("混选分组要求全部所选场景就绪");
        assert!(
            error.contains("文档转换组件未就绪"),
            "错误必须指认未就绪场景：{error}"
        );
        // 页面门槛（XB-19）：任一场景可用即可开始，纯媒体目录可进入转换页。
        assert!(crate::markdown::page_readiness().is_ok());
    }

    /// 测试共享进程环境变量，组件根相关用例必须串行访问；锁实例与
    /// snap_ocr_assets 的测试共用（见 foundation::test_support），避免两模块的
    /// 覆盖变量在并行线程中相互覆盖。
    fn env_lock() -> &'static Mutex<()> {
        jchtools_foundation::test_support::env_lock()
    }

    /// 把资产根目录与推理组件目录都重定向到临时目录；Drop 恢复环境。
    struct ComponentGuard {
        root: tempfile::TempDir,
        _lock: MutexGuard<'static, ()>,
    }

    fn redirect_component_env() -> ComponentGuard {
        let lock = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = tempfile::tempdir().expect("创建资产根目录");
        std::env::set_var("JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT", root.path());
        ComponentGuard { root, _lock: lock }
    }

    impl Drop for ComponentGuard {
        fn drop(&mut self) {
            std::env::remove_var("JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT");
        }
    }

    /// 搭建组件在位校验所需的完整文件树（存在性校验，内容任意）。
    /// 目录名与真实清单的推理组件 tag 保持一致（清单未接线时回退固定名），
    /// 与 `resolve_xberg_component` 的解析规则同步。
    fn install_component(base: &Path) -> PathBuf {
        let tag = load_manifest()
            .ok()
            .and_then(|manifest| manifest.xberg_inference.map(|inference| inference.tag))
            .unwrap_or_else(|| "vtest".to_string());
        let component = base.join("xberg-inference").join(tag);
        let files = [
            "xberg.exe",
            "models/sense_voice_zh_en_ja_ko_yue_2024_07_17/model.int8.onnx",
            "models/sense_voice_zh_en_ja_ko_yue_2024_07_17/tokens.txt",
            "models/vad/silero_vad.onnx",
            "sherpa-onnx/sherpa-onnx-c-api.dll",
            "sherpa-onnx/sherpa-onnx-cxx-api.dll",
            "sherpa-onnx/onnxruntime.dll",
            "sherpa-onnx/onnxruntime_providers_shared.dll",
            "ffmpeg/avutil-61.dll",
            "ffmpeg/swresample-7.dll",
            "ffmpeg/avcodec-63.dll",
            "ffmpeg/avformat-63.dll",
        ];
        for file in files {
            let target = component.join(file);
            fs::create_dir_all(target.parent().expect("组件路径有父目录")).expect("创建组件目录");
            fs::write(&target, b"x").expect("预置组件文件");
        }
        fs::write(
            base.join("xberg-runtime-path.txt"),
            component.to_str().expect("测试路径有效"),
        )
        .expect("模拟升级前的目录选择");
        component
    }

    struct ComponentFixture {
        tag: String,
        manifest: serde_json::Value,
        release: serde_json::Value,
        data: std::collections::BTreeMap<String, Vec<u8>>,
        pinned: super::XbergManifest,
        requests: Vec<String>,
        cancel_on: Option<String>,
    }

    impl ComponentFixture {
        fn new(tag: &str) -> Self {
            use std::io::{Cursor, Write};
            let mut components = Vec::new();
            let mut data = std::collections::BTreeMap::new();
            let mut pinned = Vec::new();
            for (id, model) in [
                ("document", "models/models--fixture/model.onnx"),
                ("snapshot", "models/snapshot-ocr/model.onnx"),
                ("media", "models/vad/model.onnx"),
            ] {
                let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
                let mut members = Vec::new();
                for (path, bytes) in [
                    ("xberg.exe", b"shared engine".as_slice()),
                    (model, id.as_bytes()),
                    ("LICENSE", b"shared license".as_slice()),
                ] {
                    zip.start_file(
                        format!("{}/{path}", super::XBERG_ARCHIVE_PREFIX),
                        zip::write::SimpleFileOptions::default(),
                    ).unwrap();
                    zip.write_all(bytes).unwrap();
                    let member = super::AssetFile {
                        path: path.into(),
                        size_bytes: bytes.len() as u64,
                        sha256: format!("{:x}", Sha256::digest(bytes)),
                    };
                    if path == model || (id == "document") {
                        pinned.push(member.clone());
                    }
                    members.push(member);
                }
                let archive = format!("xberg-{id}-{}.zip", super::XBERG_TARGET);
                let bytes = zip.finish().unwrap().into_inner();
                components.push(serde_json::json!({
                    "id":id, "archive":archive, "requires":[],
                    "size_bytes":bytes.len(), "sha256":format!("{:x}", Sha256::digest(&bytes)),
                    "members":members
                }));
                data.insert(archive, bytes);
            }
            let mut fixture = Self {
                tag: tag.into(),
                manifest: serde_json::json!({
                    "schema_version":1, "target":super::XBERG_TARGET, "components":components
                }),
                release: serde_json::Value::Null,
                data,
                pinned: super::XbergManifest {
                    tag: "old-pin".into(),
                    archive_url: String::new(),
                    archive_size_bytes: 0,
                    archive_sha256: String::new(),
                    members: pinned,
                    licenses: Vec::new(),
                },
                requests: Vec::new(),
                cancel_on: None,
            };
            fixture.refresh_release();
            fixture
        }

        fn refresh_release(&mut self) {
            self.data.insert(
                super::XBERG_COMPONENT_MANIFEST.into(),
                serde_json::to_vec(&self.manifest).unwrap(),
            );
            let assets: Vec<_> = self.data.iter().map(|(name, bytes)| serde_json::json!({
                "name":name,
                "size":bytes.len(),
                "digest":format!("sha256:{:x}", Sha256::digest(bytes)),
                "browser_download_url":format!(
                    "https://github.com/jchanghong023/xberg/releases/download/{}/{name}", self.tag
                )
            })).collect();
            self.release = serde_json::json!({
                "tag_name":self.tag, "draft":false, "prerelease":false, "assets":assets
            });
        }

        fn install(&mut self, selection: super::DownloadSelection) -> Result<PathBuf, String> {
            let release = self.release.clone();
            let pinned = super::XbergManifest {
                tag: self.pinned.tag.clone(),
                archive_url: String::new(),
                archive_size_bytes: 0,
                archive_sha256: String::new(),
                members: self.pinned.members.clone(),
                licenses: Vec::new(),
            };
            super::download_runtime_with_pinned(
                selection, &release, &AtomicBool::new(false), |_| {}, self, &pinned,
            )
        }

        fn use_known_runtime_paths(&mut self) {
            use std::io::{Cursor, Write};
            let real = super::load_manifest().unwrap();
            self.pinned.members = real.xberg.members.iter().map(|member| super::AssetFile {
                path: member.path.clone(),
                size_bytes: 1,
                sha256: format!("{:x}", Sha256::digest(b"x")),
            }).collect();
            let mut components = Vec::new();
            for id in super::COMPONENT_IDS {
                let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
                let members: Vec<_> = self.pinned.members.iter()
                    .filter(|member| {
                        let owner = super::member_owner(&member.path);
                        owner == "common" || owner == id
                    }).cloned().collect();
                for member in &members {
                    zip.start_file(
                        format!("{}/{}", super::XBERG_ARCHIVE_PREFIX, member.path),
                        zip::write::SimpleFileOptions::default(),
                    ).unwrap();
                    zip.write_all(b"x").unwrap();
                }
                let bytes = zip.finish().unwrap().into_inner();
                let name = format!("xberg-{id}-{}.zip", super::XBERG_TARGET);
                components.push(serde_json::json!({
                    "id":id, "requires":[], "archive":name,
                    "size_bytes":bytes.len(), "sha256":format!("{:x}", Sha256::digest(&bytes)),
                    "members":members
                }));
                self.data.insert(name, bytes);
            }
            self.manifest["components"] = serde_json::json!(components);
            self.refresh_release();
        }
    }

    impl AssetDownloader for ComponentFixture {
        fn download(
            &mut self,
            url: &str,
            destination: &Path,
            _: u64,
            _: &str,
            cancel: &AtomicBool,
            _: &mut dyn FnMut(String),
        ) -> Result<(), String> {
            let name = url.rsplit('/').next().ok_or("缺少文件名")?;
            self.requests.push(name.into());
            let bytes = self.data.get(name).ok_or("未提供组件资产")?;
            fs::write(destination, bytes).map_err(|error| error.to_string())?;
            if self.cancel_on.as_deref() == Some(name) {
                cancel.store(true, std::sync::atomic::Ordering::Release);
            }
            Ok(())
        }
    }

    #[test]
    fn split_snapshot_only_and_same_tag_addition_reuse_verified_components() {
        let _guard = redirect_component_env();
        let mut fixture = ComponentFixture::new("v2099.1");
        let snapshot = super::DownloadSelection::default();
        let first = fixture.install(snapshot).unwrap();
        assert_eq!(first, fs::canonicalize(&first).unwrap());
        assert_eq!(fixture.requests, [
            super::XBERG_COMPONENT_MANIFEST,
            "xberg-snapshot-x86_64-pc-windows-msvc.zip",
        ]);
        assert!(!first.join("models/models--fixture/model.onnx").exists());
        assert!(!first.join("models/vad/model.onnx").exists());
        fixture.requests.clear();
        let second = fixture.install(super::DownloadSelection {
            document: true, snapshot: false, media: false,
        }).unwrap();
        assert_ne!(first, second);
        assert_eq!(second, fs::canonicalize(&second).unwrap());
        assert_eq!(fixture.requests, [
            super::XBERG_COMPONENT_MANIFEST,
            "xberg-document-x86_64-pc-windows-msvc.zip",
        ]);
        assert!(second.join("models/snapshot-ocr/model.onnx").is_file());
        assert!(second.join("models/models--fixture/model.onnx").is_file());
        assert!(first.join("models/snapshot-ocr/model.onnx").is_file());
        fixture.requests.clear();
        let reused = fixture.install(snapshot).unwrap();
        assert_eq!(reused, second);
        assert_eq!(
            fs::canonicalize(&reused).unwrap(),
            fs::canonicalize(&second).unwrap()
        );
        assert_eq!(fixture.requests, [super::XBERG_COMPONENT_MANIFEST]);
        let receipt: super::ComponentReceipt = serde_json::from_slice(
            &fs::read(second.join(super::XBERG_RELEASE_RECEIPT)).unwrap(),
        ).unwrap();
        assert_eq!(receipt.tag, "v2099.1");
        assert_eq!(receipt.source, super::XBERG_LATEST_RELEASE_URL);
        assert_eq!(receipt.components.len(), 2);
    }

    #[test]
    fn split_upgrade_installs_selected_features_without_cross_tag_reuse() {
        let _guard = redirect_component_env();
        let first = ComponentFixture::new("v2099.1").install(super::DownloadSelection {
            document: true, snapshot: true, media: false,
        }).unwrap();
        let mut latest = ComponentFixture::new("v2099.2");
        let upgraded = latest.install(super::DownloadSelection {
            document: false, snapshot: false, media: true,
        }).unwrap();
        assert_ne!(first, upgraded);
        assert_eq!(latest.requests, [
            super::XBERG_COMPONENT_MANIFEST,
            "xberg-media-x86_64-pc-windows-msvc.zip",
        ]);
        assert!(!upgraded.join("models/snapshot-ocr/model.onnx").exists());
        assert!(!upgraded.join("models/models--fixture/model.onnx").exists());
        assert_eq!(fs::read(first.join("xberg.exe")).unwrap(), b"shared engine");
    }

    #[test]
    fn split_snapshot_selection_does_not_inherit_legacy_full_bundle_features() {
        let _guard = redirect_component_env();
        let base = jchtools_foundation::xberg_settings::state_dir().unwrap().join("xberg-downloads");
        let legacy = base.join(format!("v2098-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&legacy).unwrap();
        for member in &super::load_manifest().unwrap().xberg.members {
            let path = legacy.join(&member.path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"legacy").unwrap();
        }
        fs::write(
            legacy.join(super::XBERG_RELEASE_RECEIPT),
            serde_json::to_vec(&serde_json::json!({
                "owner":"JchTools-xberg-download-v1", "tag":"v2098",
                "archive_url":"https://github.com/jchanghong023/xberg/releases/download/v2098/xberg-cli-x86_64-pc-windows-msvc.zip"
            })).unwrap(),
        ).unwrap();
        jchtools_foundation::xberg_settings::save_source(
            jchtools_foundation::xberg_settings::Source::Downloaded, &legacy,
        ).unwrap();
        let mut fixture = ComponentFixture::new("v2099.1");
        let installed = fixture.install(super::DownloadSelection::default()).unwrap();
        assert_eq!(fixture.requests, [
            super::XBERG_COMPONENT_MANIFEST,
            "xberg-snapshot-x86_64-pc-windows-msvc.zip",
        ]);
        assert!(!installed.join("models/models--fixture/model.onnx").exists());
        assert!(!installed.join("models/vad/model.onnx").exists());
        assert_eq!(fs::read(legacy.join("xberg.exe")).unwrap(), b"legacy");
        assert!(legacy.join("models/vad/silero_vad.onnx").is_file());
    }

    #[test]
    fn split_damaged_installed_component_is_replaced_without_changing_original() {
        let _guard = redirect_component_env();
        let mut fixture = ComponentFixture::new("v2099.1");
        let first = fixture.install(super::DownloadSelection::default()).unwrap();
        fs::write(first.join("models/snapshot-ocr/model.onnx"), b"changed").unwrap();
        fixture.requests.clear();
        let replacement = fixture.install(super::DownloadSelection::default()).unwrap();
        assert_ne!(first, replacement);
        assert_eq!(fixture.requests.len(), 2);
        assert_eq!(fs::read(first.join("models/snapshot-ocr/model.onnx")).unwrap(), b"changed");
        assert_eq!(fs::read(replacement.join("models/snapshot-ocr/model.onnx")).unwrap(), b"snapshot");
    }

    #[test]
    fn split_integrity_failures_and_cancel_preserve_active_config_and_assets() {
        let guard = redirect_component_env();
        let active = guard.root.path().join("user-runtime");
        fs::create_dir(&active).unwrap();
        fs::write(active.join("xberg.exe"), b"original engine").unwrap();
        jchtools_foundation::xberg_settings::save(&active).unwrap();
        let original = jchtools_foundation::xberg_settings::required().unwrap();
        let selection = super::DownloadSelection { document: true, snapshot: true, media: true };
        for id in super::COMPONENT_IDS {
            let name = format!("xberg-{id}-{}.zip", super::XBERG_TARGET);
            for failure in ["digest", "size", "cancel"] {
                let mut fixture = ComponentFixture::new("v2099.1");
                match failure {
                    "digest" => {
                        let bytes = fixture.data.get_mut(&name).unwrap();
                        bytes[0] ^= 1;
                    }
                    "size" => { fixture.data.get_mut(&name).unwrap().pop(); }
                    "cancel" => { fixture.cancel_on = Some(name.clone()); }
                    _ => unreachable!(),
                }
                assert!(fixture.install(selection).is_err(), "{id}/{failure}");
                assert_eq!(jchtools_foundation::xberg_settings::required().unwrap(), original);
                assert_eq!(fs::read(active.join("xberg.exe")).unwrap(), b"original engine");
            }
        }
        let mut fixture = ComponentFixture::new("v2099.1");
        fixture.data.get_mut(super::XBERG_COMPONENT_MANIFEST).unwrap()[0] ^= 1;
        assert!(fixture.install(selection).is_err());
        assert_eq!(jchtools_foundation::xberg_settings::required().unwrap(), original);
        let mut fixture = ComponentFixture::new("v2099.1");
        fixture.manifest["components"][1]["members"][1]["sha256"] = serde_json::json!("0".repeat(64));
        fixture.refresh_release();
        assert!(fixture.install(selection).unwrap_err().contains("模型身份"));
        assert_eq!(jchtools_foundation::xberg_settings::required().unwrap(), original);
    }

    #[test]
    fn split_metadata_rejects_legacy_only_release_and_untrusted_assets() {
        let _guard = redirect_component_env();
        let mut empty = ComponentFixture::new("v2099.1");
        assert!(empty.install(super::DownloadSelection {
            document: false, snapshot: false, media: false,
        }).unwrap_err().contains("至少选择"));
        assert!(empty.requests.is_empty());
        let mut fixture = ComponentFixture::new("v2099.1");
        fixture.release["assets"].as_array_mut().unwrap().retain(
            |asset| asset["name"] == super::XBERG_COMPONENT_MANIFEST,
        );
        assert!(fixture.install(super::DownloadSelection::default()).unwrap_err().contains("不会回退"));
        assert!(fixture.requests.is_empty());
        for field in ["digest", "browser_download_url"] {
            let mut fixture = ComponentFixture::new("v2099.1");
            fixture.release["assets"][0][field] = serde_json::json!("https://example.invalid/wrong");
            assert!(fixture.install(super::DownloadSelection::default()).is_err());
            assert!(fixture.requests.is_empty());
        }
        let mut fixture = ComponentFixture::new("v2099.1");
        let duplicate = fixture.release["assets"][0].clone();
        fixture.release["assets"].as_array_mut().unwrap().push(duplicate);
        assert!(fixture.install(super::DownloadSelection::default()).is_err());
        assert!(fixture.requests.is_empty());
    }

    #[test]
    fn split_member_integrity_overlap_and_zip_safety_failures_preserve_active_install() {
        use std::io::{Cursor, Write};
        let _guard = redirect_component_env();
        let mut first = ComponentFixture::new("v2099.1");
        let active = first.install(super::DownloadSelection::default()).unwrap();
        let original = jchtools_foundation::xberg_settings::required().unwrap();
        for failure in ["member_digest", "member_missing", "member_extra", "unsafe_path", "overlap"] {
            let mut fixture = ComponentFixture::new("v2099.2");
            let name = "xberg-snapshot-x86_64-pc-windows-msvc.zip";
            if failure == "overlap" {
                fixture.manifest["components"][1]["members"][0]["sha256"] = serde_json::json!("0".repeat(64));
            } else {
                let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
                let mut files = vec![
                    ("xberg.exe".to_string(), b"shared engine".as_slice()),
                    ("LICENSE".to_string(), b"shared license".as_slice()),
                ];
                if failure != "member_missing" {
                    files.push(("models/snapshot-ocr/model.onnx".into(),
                        if failure == "member_digest" { b"wrong".as_slice() } else { b"snapshot".as_slice() }));
                }
                if failure == "member_extra" { files.push(("extra.bin".into(), b"extra")); }
                if failure == "unsafe_path" { files.push(("../escape.bin".into(), b"escape")); }
                for (path, bytes) in files {
                    zip.start_file(
                        format!("{}/{path}", super::XBERG_ARCHIVE_PREFIX),
                        zip::write::SimpleFileOptions::default(),
                    ).unwrap();
                    zip.write_all(bytes).unwrap();
                }
                let bytes = zip.finish().unwrap().into_inner();
                fixture.manifest["components"][1]["size_bytes"] = serde_json::json!(bytes.len());
                fixture.manifest["components"][1]["sha256"] = serde_json::json!(format!("{:x}", Sha256::digest(&bytes)));
                fixture.data.insert(name.into(), bytes);
            }
            fixture.refresh_release();
            assert!(fixture.install(super::DownloadSelection::default()).is_err(), "{failure}");
            assert_eq!(jchtools_foundation::xberg_settings::required().unwrap(), original);
            assert_eq!(fs::read(active.join("xberg.exe")).unwrap(), b"shared engine");
        }
    }

    // 覆盖 T-05/T-06（XB-01/XB-12）：媒体组件按在位校验，齐全时返回组件目录。
    #[test]
    fn media_component_ready_returns_component_dir() {
        let guard = redirect_component_env();
        let component = install_component(guard.root.path());
        let resolved = media_component_dir().expect("齐全组件应通过在位校验");
        assert_eq!(resolved, component);
        assert_eq!(
            guard.root.path().join("xberg-inference"),
            super::xberg_inference_root(),
            "安装根必须固定在 <资产根>/xberg-inference"
        );
    }

    // 覆盖 T-05：组件未安装时如实报告未配置，不冒称就绪。
    #[test]
    fn media_component_missing_reports_not_configured() {
        let _guard = redirect_component_env();
        let error = media_component_dir().expect_err("缺失组件必须报未配置");
        assert!(error.contains("未配置"), "错误应说明组件未配置：{error}");
        assert!(error.contains("设置页"), "错误应指引共享配置入口：{error}");
    }

    // 覆盖 T-06：组件不完整时明确指出缺失项，不执行不完整环境。
    #[test]
    fn media_component_incomplete_reports_missing_file() {
        let guard = redirect_component_env();
        let component = install_component(guard.root.path());
        fs::remove_file(component.join("ffmpeg").join("avcodec-63.dll")).expect("删除一个 DLL");
        let error = media_component_dir().expect_err("不完整组件必须失败");
        assert!(
            error.contains("avcodec-63.dll"),
            "错误应指出缺失文件：{error}"
        );
        assert!(
            error.contains("推理组件不完整"),
            "错误应说明组件不完整：{error}"
        );
    }

    // 覆盖 T-05：设置页保存的本地目录即组件目录（2026-10-04 起组件目录只能
    // 经配置页保存或产品内下载，环境变量覆盖已删除）；缺媒体模型时明确指出
    // 缺失项，不执行不完整环境。
    #[test]
    fn media_component_saved_dir_points_at_local_tree() {
        let guard = redirect_component_env();
        let external = guard.root.path().join("external-component");
        fs::create_dir_all(&external).expect("创建外部组件目录");
        fs::write(external.join("xberg.exe"), b"x").expect("预置外部组件文件");
        jchtools_foundation::xberg_settings::save(&external).expect("经设置链保存本地组件目录");
        let resolved = media_component_dir().expect_err("外部树缺模型时必须指出缺失项");
        assert!(resolved.contains("model.int8.onnx"), "{resolved}");
    }


    #[test]
    fn download_cancel_before_commit_keeps_config_untouched() {
        let _guard = redirect_component_env();
        let mut fixture = ComponentFixture::new("v2099.1");
        fixture.cancel_on = Some("xberg-snapshot-x86_64-pc-windows-msvc.zip".into());
        assert!(fixture.install(super::DownloadSelection::default()).unwrap_err().contains("取消"));
        assert!(jchtools_foundation::xberg_settings::settings().unwrap().downloaded.is_none());
        let base = jchtools_foundation::xberg_settings::state_dir().unwrap().join("xberg-downloads");
        assert_eq!(fs::read_dir(base).unwrap().count(), 0);
    }

    #[test]
    fn download_runtime_write_notice_closes_readiness_loop() {
        let guard = redirect_component_env();
        let mut fixture = ComponentFixture::new("v2099.1");
        // 字节为合成资产，公开 readiness 仍按真实路径检查存在性，不能据此声称模型推理通过。
        fixture.use_known_runtime_paths();
        let installed = fixture.install(super::DownloadSelection {
            document: true, snapshot: true, media: true,
        }).unwrap();
        assert!(super::validate_media().is_ok());
        assert!(super::readiness().is_ok());
        assert!(guard.root.path().join("licenses/THIRD_PARTY_NOTICES.md").is_file());
        assert!(installed.is_dir());
    }

    #[test]
    fn split_sqlite_commit_failure_rolls_back_new_install_only() {
        let guard = redirect_component_env();
        let active = guard.root.path().join("user-runtime");
        fs::create_dir(&active).unwrap();
        fs::write(active.join("xberg.exe"), b"original").unwrap();
        jchtools_foundation::xberg_settings::save(&active).unwrap();
        let original = jchtools_foundation::xberg_settings::required().unwrap();
        let db = jchtools_foundation::xberg_settings::open(
            &jchtools_foundation::xberg_settings::state_dir().unwrap(),
        ).unwrap();
        db.execute_batch(
            "CREATE TRIGGER reject_download_source BEFORE INSERT ON app_settings
             WHEN NEW.key='xberg_source' AND NEW.value='downloaded'
             BEGIN SELECT RAISE(ABORT,'injected commit failure'); END;",
        ).unwrap();
        let mut fixture = ComponentFixture::new("v2099.1");
        assert!(fixture.install(super::DownloadSelection::default()).is_err());
        assert_eq!(jchtools_foundation::xberg_settings::required().unwrap(), original);
        assert_eq!(fs::read(active.join("xberg.exe")).unwrap(), b"original");
        let base = jchtools_foundation::xberg_settings::state_dir().unwrap().join("xberg-downloads");
        assert_eq!(fs::read_dir(base).unwrap().count(), 0);
    }

    #[test]
    fn split_notice_failure_preserves_active_assets_and_config() {
        let guard = redirect_component_env();
        let state = jchtools_foundation::xberg_settings::state_dir().unwrap();
        let active = guard.root.path().join("user-runtime");
        fs::create_dir(&active).unwrap();
        fs::write(active.join("xberg.exe"), b"original").unwrap();
        jchtools_foundation::xberg_settings::save(&active).unwrap();
        let original = jchtools_foundation::xberg_settings::required().unwrap();
        let blocker = guard.root.path().join("notice-root-file");
        fs::write(&blocker, b"keep").unwrap();
        struct RestoreRoots<'a>(&'a Path);
        impl Drop for RestoreRoots<'_> {
            fn drop(&mut self) {
                std::env::remove_var("JCHTOOLS_TEST_STATE_DIR");
                std::env::set_var("JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT", self.0);
            }
        }
        let _restore = RestoreRoots(guard.root.path());
        std::env::set_var("JCHTOOLS_TEST_STATE_DIR", &state);
        std::env::set_var("JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT", &blocker);
        let mut fixture = ComponentFixture::new("v2099.1");
        assert!(fixture.install(super::DownloadSelection::default()).is_err());
        assert_eq!(jchtools_foundation::xberg_settings::required().unwrap(), original);
        assert_eq!(fs::read(active.join("xberg.exe")).unwrap(), b"original");
        assert_eq!(fs::read(blocker).unwrap(), b"keep");
        assert_eq!(fs::read_dir(state.join("xberg-downloads")).unwrap().count(), 0);
    }

    // 覆盖 XB-19：保存共享目录时不强制其他场景的模型就绪；每个功能仅在自身
    // 启动前按场景检查。即使目录缺少截图和媒体模型，文档所需成员齐全也可保存。
    #[test]
    fn save_runtime_dir_allows_other_scenarios_to_be_unready() {
        let guard = redirect_component_env();
        let manifest = load_manifest().expect("内置清单必须可解析");
        let engine = tempfile::tempdir().expect("创建自选引擎目录");
        for member in &manifest.xberg.members {
            if !jchtools_xberg::xberg_runtime::asset_for_scenario(&member.path, "document") {
                continue;
            }
            let path = engine.path().join(&member.path);
            fs::create_dir_all(path.parent().expect("成员路径有父目录")).expect("创建成员父目录");
            fs::write(&path, format!("stub-{}", member.path)).expect("写入成员桩文件");
        }

        jchtools_xberg::xberg_runtime::validate_assets(engine.path(), "document").expect("文档场景成员齐全");
        assert!(
            jchtools_xberg::xberg_runtime::validate_assets(engine.path(), "snapshot").is_err(),
            "缺截图模型时截图场景应仍未就绪"
        );
        assert!(
            jchtools_xberg::xberg_runtime::validate_assets(engine.path(), "media").is_err(),
            "缺媒体模型时媒体场景应仍未就绪"
        );

        super::save_runtime_dir(engine.path()).expect("文档可用的自选目录必须可保存");
        let notice = guard
            .root
            .path()
            .join("licenses")
            .join("THIRD_PARTY_NOTICES.md");
        assert!(
            notice.is_file(),
            "保存自选目录后必须写入 notice：{}",
            notice.display()
        );
        assert!(
            super::readiness().is_ok(),
            "保存自选目录即应满足文档场景 readiness：{:?}",
            super::readiness()
        );
    }

    // 覆盖 T-06/XB-10（回归：清单 xberg 与 xberg_inference 两段的
    // tag/URL/大小/SHA-256 四值重复但各段独立校验——漂移会让设置页下载与
    // 媒体成员校验口径分叉；交叉校验必须在漂移时报错，不能静默放行）。
    #[test]
    fn manifest_cross_segment_drift_is_rejected() {
        let mark = "\"xberg_inference\"";
        let tamper = |from: &str, to: &str| {
            let position = super::MANIFEST.find(mark).expect("内置清单包含推理段");
            let (head, tail) = super::MANIFEST.split_at(position);
            format!("{head}{}", tail.replacen(from, to, 1))
        };
        // tag 漂移（换成另一合法形态 tag，先通过单段校验再触发段间校验）。
        let tampered = tamper(super::XBERG_TAG, "v2026.10.9-9999-run99.1");
        let error = super::parse_manifest(&tampered).expect_err("推理段 tag 漂移必须报错");
        assert!(
            error.contains("不一致"),
            "错误应说明段间元数据不一致：{error}"
        );
        // sha256 漂移（等长 64 位十六进制，单段长度检查照常通过）。
        let sha = load_manifest()
            .expect("内置清单必须可解析")
            .xberg
            .archive_sha256
            .clone();
        let tampered = tamper(&sha, &"0".repeat(64));
        assert!(
            super::parse_manifest(&tampered).is_err(),
            "推理段 sha256 漂移必须报错"
        );
        // 对照：内置清单两段一致，必须照常通过。
        super::parse_manifest(super::MANIFEST).expect("内置清单两段一致必须通过");
    }

    #[test]
    fn notice_lists_only_xberg_licenses() {
        let manifest = load_manifest().expect("内置资产清单必须可解析");
        let root = tempfile::tempdir().expect("创建测试目录");
        let notice = root.path().join("THIRD_PARTY_NOTICES.md");
        write_notice(&notice, &manifest).expect("写入 notice");
        let text = fs::read_to_string(&notice).expect("读取 notice");
        assert!(text.contains("Xberg CLI"), "应包含 Xberg CLI 条目：{text}");
        assert!(
            !text.contains("SenseVoice INT8"),
            "媒体模型许可条目已退役，不得出现：{text}"
        );
    }

    // ── Xberg 推理组件包：下载安装、复用、失败保护与版本一致性（XB-09/XB-10）──

    fn sha256_bytes(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }

    /// 构造两成员（xberg.exe + 字典）的推理组件包夹具：内存 zip + 对应清单。
    fn fixture_inference_pack() -> (InferenceManifest, Vec<u8>) {
        let exe = b"fake-xberg-exe-bytes".to_vec();
        let dict = b"fake-snapshot-dict".to_vec();
        let mut cursor = std::io::Cursor::new(Vec::new());
        {
            use std::io::Write as IoWrite;
            let mut writer = zip::ZipWriter::new(&mut cursor);
            let options = zip::write::SimpleFileOptions::default();
            writer
                .start_file("pkg/xberg.exe", options)
                .expect("写入 zip 成员");
            writer.write_all(&exe).expect("写入成员字节");
            writer
                .start_file("pkg/models/snapshot-ocr/dict.txt", options)
                .expect("写入 zip 成员");
            writer.write_all(&dict).expect("写入成员字节");
            writer.finish().expect("完成 zip");
        }
        let archive = cursor.into_inner();
        let manifest = InferenceManifest {
            tag: "vtest-inference".to_string(),
            url: "https://fixtures.invalid/inference.zip".to_string(),
            size_bytes: archive.len() as u64,
            sha256: sha256_bytes(&archive),
            members: vec![
                InferenceMember {
                    path: "pkg/xberg.exe".to_string(),
                    install_path: "xberg.exe".to_string(),
                    size_bytes: exe.len() as u64,
                    sha256: sha256_bytes(&exe),
                },
                InferenceMember {
                    path: "pkg/models/snapshot-ocr/dict.txt".to_string(),
                    install_path: "models/snapshot-ocr/dict.txt".to_string(),
                    size_bytes: dict.len() as u64,
                    sha256: sha256_bytes(&dict),
                },
            ],
        };
        (manifest, archive)
    }

    /// 注入式下载器：供给内存字节、可编程失败、记录调用次数。
    struct FakeInferenceDownloader {
        archive: Vec<u8>,
        fail: bool,
        calls: usize,
    }

    impl AssetDownloader for FakeInferenceDownloader {
        fn download(
            &mut self,
            _url: &str,
            destination: &Path,
            _expected_size: u64,
            _expected_sha256: &str,
            _cancel: &AtomicBool,
            _progress: &mut dyn FnMut(String),
        ) -> Result<(), String> {
            self.calls += 1;
            if self.fail {
                return Err("模拟下载失败".to_string());
            }
            fs::write(destination, &self.archive).map_err(|error| error.to_string())
        }
    }

    fn run_install(
        inference: &InferenceManifest,
        root: &Path,
        downloader: &mut FakeInferenceDownloader,
    ) -> Result<(), String> {
        let staging = root.join("staging");
        fs::create_dir_all(&staging).expect("创建 staging");
        let cancel = AtomicBool::new(false);
        let mut progress = |_message: String| {};
        install_inference_pack(
            inference,
            &staging,
            root,
            &cancel,
            downloader,
            &mut progress,
        )
    }

    // 覆盖 XB-10：组件包按成员落位到 xberg-inference/<tag>/，旧版本目录被移除。
    #[test]
    fn inference_pack_installs_members_and_prunes_old_tags() {
        let root = tempfile::tempdir().expect("创建测试根");
        let (manifest, archive) = fixture_inference_pack();
        let old = root.path().join("xberg-inference").join("vold");
        fs::create_dir_all(&old).expect("预置旧版本目录");
        fs::write(old.join("xberg.exe"), b"old").expect("预置旧文件");

        let mut downloader = FakeInferenceDownloader {
            archive,
            fail: false,
            calls: 0,
        };
        run_install(&manifest, root.path(), &mut downloader).expect("安装推理组件包应成功");

        let component = root.path().join("xberg-inference").join("vtest-inference");
        assert_eq!(
            fs::read(component.join("xberg.exe")).expect("xberg.exe 已安装"),
            b"fake-xberg-exe-bytes"
        );
        assert_eq!(
            fs::read(
                component
                    .join("models")
                    .join("snapshot-ocr")
                    .join("dict.txt")
            )
            .expect("字典已安装"),
            b"fake-snapshot-dict"
        );
        assert!(!old.exists(), "旧版本目录必须被移除（不混用版本）");
        assert_eq!(downloader.calls, 1);
        inference_ready(&manifest, root.path()).expect("安装后成员级校验应通过");
    }

    // 覆盖 XB-10「保留已校验资产」：已验证安装不重下，下载器不得被调用。
    #[test]
    fn inference_pack_reuse_skips_download() {
        let root = tempfile::tempdir().expect("创建测试根");
        let (manifest, archive) = fixture_inference_pack();
        let mut downloader = FakeInferenceDownloader {
            archive,
            fail: false,
            calls: 0,
        };
        run_install(&manifest, root.path(), &mut downloader).expect("首次安装应成功");
        run_install(&manifest, root.path(), &mut downloader).expect("复用安装应成功");
        assert_eq!(downloader.calls, 1, "已校验组件不得重复下载");
    }

    // 覆盖 XB-09/T-05：归档摘要不符时安装失败，已有安装不受影响。
    #[test]
    fn inference_pack_bad_archive_keeps_existing_install() {
        let root = tempfile::tempdir().expect("创建测试根");
        let (manifest, archive) = fixture_inference_pack();
        let mut downloader = FakeInferenceDownloader {
            archive,
            fail: false,
            calls: 0,
        };
        run_install(&manifest, root.path(), &mut downloader).expect("首次安装应成功");

        // 破坏一个已安装成员触发重装，再供给坏归档。
        let component = root.path().join("xberg-inference").join(&manifest.tag);
        fs::write(component.join("xberg.exe"), b"tampered").expect("破坏一个成员");
        let mut corrupt = FakeInferenceDownloader {
            archive: b"corrupt-archive".to_vec(),
            fail: false,
            calls: 0,
        };
        let error =
            run_install(&manifest, root.path(), &mut corrupt).expect_err("摘要不符必须失败");
        assert!(error.contains("校验失败"), "错误应说明校验失败：{error}");
        assert_eq!(corrupt.calls, 1);
    }

    // 覆盖 XB-09：成员被篡改时成员级校验必须发现（不因同名文件认定兼容）。
    #[test]
    fn inference_ready_detects_tampered_member() {
        let root = tempfile::tempdir().expect("创建测试根");
        let (manifest, archive) = fixture_inference_pack();
        let mut downloader = FakeInferenceDownloader {
            archive,
            fail: false,
            calls: 0,
        };
        run_install(&manifest, root.path(), &mut downloader).expect("首次安装应成功");
        let component = root.path().join("xberg-inference").join("vtest-inference");
        fs::write(component.join("xberg.exe"), b"tampered").expect("篡改成员");
        let error = inference_ready(&manifest, root.path()).expect_err("篡改必须被成员级校验发现");
        assert!(error.contains("xberg.exe"), "错误应指明成员：{error}");
    }

    // 覆盖 XB-09：目录 tag 与清单不一致时明确报错并指引更新，不静默使用。
    #[test]
    fn resolve_component_with_tag_mismatch_errors() {
        let root = tempfile::tempdir().expect("创建测试根");
        let base = root.path().join("xberg-inference");
        let installed = base.join("vinstalled");
        fs::create_dir_all(&installed).expect("预置安装目录");
        let error = resolve_component_with_tag(&base, Some("vexpected"), "未配置")
            .expect_err("版本不一致必须报错");
        assert!(
            error.contains("vexpected") && error.contains("更新"),
            "错误应指明清单要求并指引更新：{error}"
        );
        resolve_component_with_tag(&base, Some("vinstalled"), "未配置").expect("一致时应正常解析");
        resolve_component_with_tag(&base, None, "未配置").expect("无清单 tag 时唯一目录可解析");
    }

    // 覆盖 XB-09 自愈：多目录（如一次清理失败残留旧版本）时优先选中清单 tag
    // 目录，不阻塞使用；无清单 tag 目录才报错。
    #[test]
    fn resolve_component_multiple_dirs_prefers_manifest_tag() {
        let root = tempfile::tempdir().expect("创建测试根");
        let base = root.path().join("xberg-inference");
        for tag in ["vnew", "vold"] {
            fs::create_dir_all(base.join(tag)).expect("预置版本目录");
        }
        let resolved =
            resolve_component_with_tag(&base, Some("vnew"), "未配置").expect("应选中清单 tag 目录");
        assert_eq!(
            resolved.file_name().and_then(|name| name.to_str()),
            Some("vnew")
        );
        let error = resolve_component_with_tag(&base, Some("vabsent"), "未配置")
            .expect_err("清单 tag 不在场必须报错");
        assert!(error.contains("vabsent"), "错误应指明缺失 tag：{error}");
    }

    // ── B-1/B-2：staging 清理降级与残留兜底清理 ──

    // 覆盖 B-1：staging 清理失败时，已成功的关键变更不得被误报为失败（生产
    // 触发：防护软件/索引器短暂持有 staging 内文件句柄；gui.rs 的保存链路还
    // 会把误报转化为 convert_runtime_confirmed=false）。注入方式：staging
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
    // 收尾清理，入口不清理任何残留（残留单个可超 1GB）。
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

        let cancel = AtomicBool::new(false);
        // 后续阶段会因尚未下载 Xberg 功能组件而失败；本用例只断言入口清理。
        let _ = super::initialize(&cancel, |_message: String| {});

        assert!(
            !residue.exists(),
            "残留 staging 目录必须在 initialize 入口被兜底清理（B-2）"
        );
    }

    // 覆盖 B-2：单个残留目录清理失败（如被防护软件锁定）必须跳过继续，不得
    // 影响其余残留清理，也不得让初始化整体失败。注入方式：对残留内文件持有
    // 无 FILE_SHARE_DELETE 的句柄，remove_dir_all 稳定失败（目录不涉及改名，
    // 句柄方案可行；对照 asset_util 的目录原子替换用例）。
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

            let cancel = AtomicBool::new(false);
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

    // 覆盖 T-05：已就绪环境也必须响应预置取消，不能显示初始化成功。
    #[test]
    fn initialize_cancelled_ready_runtime_reports_cancel() {
        let guard = redirect_component_env();
        let runtime = guard.root.path().join("runtime");
        for member in load_manifest().expect("读取清单").xberg.members {
            if !jchtools_xberg::xberg_runtime::asset_for_scenario(&member.path, "document") {
                continue;
            }
            let target = runtime.join(&member.path);
            fs::create_dir_all(target.parent().expect("成员父目录")).expect("创建成员目录");
            fs::write(target, b"replaced-engine").expect("预置成员");
        }
        super::save_runtime_dir(&runtime).expect("保存就绪运行目录");
        super::readiness().expect("前置：文档环境已就绪");
        let mut messages = Vec::new();
        let result = super::initialize(&AtomicBool::new(true), |message| messages.push(message));
        assert!(
            result.is_err(),
            "预置取消必须返回取消状态，不能报告就绪成功：{result:?}"
        );
        assert!(result.expect_err("取消结果").contains("取消"));
        assert!(
            messages.is_empty(),
            "预置取消不得发出初始化成功进度：{messages:?}"
        );
    }

    // 覆盖 B-2 扩展：save_runtime_dir 写入失败/崩溃残留的
    // .xberg-runtime-path.txt.<uuid> 临时文件必须在 initialize 入口被兜底清扫
    //——修复前入口只清 .staging-*，该残留永久滞留；.old-* 备份不在清扫范围
    //（由原子替换路径管理），必须保留。
    #[test]
    fn initialize_cleans_stale_runtime_selection_residue_at_entry() {
        let guard = redirect_component_env();
        let root = guard.root.path();
        fs::create_dir_all(root).expect("预置资产根目录");
        let residue = root.join(".xberg-runtime-path.txt.deadbeef");
        fs::write(&residue, b"C:\\xberg\n").expect("预置运行目录残留临时文件");
        let keep_backup = root.join(".old-deadbeef");
        fs::create_dir_all(&keep_backup).expect("预置旧备份残留");

        let cancel = AtomicBool::new(false);
        // 后续阶段会因尚未下载 Xberg 功能组件而失败；本用例只断言入口清理。
        let _ = super::initialize(&cancel, |_message: String| {});

        assert!(
            !residue.exists(),
            "残留的 .xberg-runtime-path.txt.* 临时文件必须在 initialize 入口被兜底清理（B-2）"
        );
        assert!(
            keep_backup.exists(),
            ".old-* 备份不在初始化入口清扫范围（由原子替换路径管理），必须保留"
        );
    }

    // ── C-2（markdown 侧）：readiness 的推理包成员检查与组件目录解析同源 ──

    // 覆盖 C-2（markdown 侧；XB-09 2026-10-02 修订后的存在性口径）：设置页
    // 保存的组件目录下，推理组件成员检查必须基于 resolve_xberg_component
    // 解析出的组件目录，而非资产根相对路径。覆盖树内全部 media 成员以桩字节
    // （与清单摘要不同）在场：存在性口径下必须通过——资产根下无任何组件文件，
    // 若校验仍走资产根相对路径或仍比对摘要则必失败，一次断言同时钉住两条口径；
    // 删除成员后错误基于保存目录点名缺失项。
    #[test]
    fn readiness_inference_pack_honors_configured_component_dir() {
        let guard = redirect_component_env();
        let external = guard.root.path().join("external-component");
        let manifest = load_manifest().expect("内置清单必须可解析");
        let inference = manifest
            .xberg_inference
            .as_ref()
            .expect("内置清单已接入推理组件段");
        for member in &inference.members {
            if !jchtools_xberg::xberg_runtime::asset_for_scenario(&member.install_path, "media") {
                continue;
            }
            let target = external.join(&member.install_path);
            fs::create_dir_all(target.parent().expect("成员路径有父目录"))
                .expect("创建覆盖树成员目录");
            fs::write(&target, b"replaced-engine").expect("预置桩成员（字节与清单不同）");
        }
        fs::write(external.join("xberg.exe"), b"engine").expect("预置引擎文件供设置链校验");
        jchtools_foundation::xberg_settings::save(&external).expect("经设置链保存组件目录");

        super::readiness_inference_pack(&manifest, guard.root.path())
            .expect("桩字节成员应通过存在性检查（替换引擎免摘要校验）");

        fs::remove_file(external.join("models").join("vad").join("silero_vad.onnx"))
            .expect("删除一个媒体模型");
        let error = super::readiness_inference_pack(&manifest, guard.root.path())
            .expect_err("缺失成员必须失败");
        assert!(
            error.contains("silero_vad.onnx") && error.contains("缺失"),
            "错误应基于保存目录点名缺失成员：{error}"
        );
    }

    // 覆盖 T-06（XB-09）：已下载的 Xberg 运行目录只做 document 场景成员
    // 存在性检查；文件被用户替换为不同字节仍须放行，缺失时明确指认成员。
    #[test]
    fn validate_runtime_dir_presence_only_accepts_replaced_engine() {
        let temp = tempfile::tempdir().expect("创建测试目录");
        let root = temp.path();
        let manifest = load_manifest().expect("内置清单必须可解析");
        for member in &manifest.xberg.members {
            let target = root.join(&member.path);
            fs::create_dir_all(target.parent().expect("成员路径有父目录")).expect("创建成员目录");
            fs::write(&target, b"replaced-engine").expect("预置成员（字节与清单不同）");
        }
        super::validate_runtime_dir(root).expect("替换引擎文件后应放行（运行时不比对摘要）");

        fs::remove_file(root.join("xberg.exe")).expect("删除引擎文件");
        let error = super::validate_runtime_dir(root).expect_err("成员缺失必须报错");
        assert!(
            error.contains("xberg.exe") && error.contains("缺失"),
            "错误应指认缺失成员：{error}"
        );
    }
}
