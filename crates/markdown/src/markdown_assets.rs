//! 内置文档/截图运行目录与显式可选音频安装。
//! 音频只使用当前运行目录随包收据锁定的发布资产；离线迁移按目标收据逐成员
//! 校验，保留旧来源和已有有效文件，最后经共享 broker 安全提交统一来源。

use jchtools_assets::asset_util::{
    atomic_replace_dir, cleanup_stale_staging_dirs, ensure_not_cancelled, finalize_staging,
    require_component_members, require_inference_members_for_scenario, resolve_xberg_component,
    state_dir_asset_root, valid_component_tag, validate_relative_path, verify_file_with_cancel,
    AssetDownloader, InferenceManifest,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as FmtWrite;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use uuid::Uuid;

const MANIFEST: &str = include_str!("../../../resources/markdown-assets.json");
const DATA_DIRECTORY: &str = "markdown-assets";
pub(crate) const XBERG_TAG: &str = "v2026.10.6-0420-run58.1";
const RUNTIME_SELECTION_FILE: &str = "xberg-runtime-path.txt";
const XBERG_RELEASE_RECEIPT: &str = "jchtools-xberg-release.json";
const XBERG_LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/jchanghong023/xberg/releases/latest";
const XBERG_TARGET: &str = "x86_64-pc-windows-msvc";
const XBERG_COMPONENT_MANIFEST: &str = "xberg-components-x86_64-pc-windows-msvc.json";
const XBERG_ARCHIVE_PREFIX: &str = "xberg-cli-x86_64-pc-windows-msvc";
const AUDIO_ARCHIVE: &str = "xberg-audio-assets-x86_64-pc-windows-msvc.zip";
static OPTIONAL_INSTALL: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    jchtools_foundation::xberg_settings::effective_directory()
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
/// 已存在时幂等返回；缺失时先完整写入自有 staging，再以 create-new 语义原子发布文件。
/// 并发修复只复用已完整发布的成品，不交换整个 licenses 目录，不截断或覆盖既有文件。
/// 设置页保存、启动就绪检查与下载共用此闭环。
pub fn ensure_document_notice() -> Result<(), String> {
    let manifest = load_manifest()?;
    let root = asset_root();
    let licenses = root.join("licenses");
    let target = licenses.join("THIRD_PARTY_NOTICES.md");
    if target.is_file() {
        return Ok(());
    }
    fs::create_dir_all(&licenses).map_err(|error| format!("创建许可证目录失败：{error}"))?;
    let staging = root.join(format!(".notice-staging-{}", Uuid::new_v4().simple()));
    fs::create_dir_all(&staging).map_err(|error| format!("创建许可证临时目录失败：{error}"))?;
    let prepared = staging.join("THIRD_PARTY_NOTICES.md");
    let result =
        write_notice(&prepared, &manifest).and_then(|()| match fs::hard_link(&prepared, &target) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && target.is_file() => {
                Ok(())
            }
            Err(error) => Err(format!("原子就位许可证 notice 失败：{error}")),
        });
    if let Err(error) = fs::remove_dir_all(&staging) {
        tracing::warn!(
            event = "notice_staging_cleanup_failed",
            error_type = ?error.kind(),
            "许可证暂存目录清理失败"
        );
    }
    result
}
/// 返回应用唯一生效运行目录（含未打开 GUI 的随包默认目录）。
pub fn runtime_dir() -> Result<PathBuf, String> {
    load_saved_runtime_dir()?
        .ok_or_else(|| "未找到 Xberg 运行目录，请检查安装包或设置页的共享目录".to_string())
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
    readiness_inference_pack(&load_manifest()?, &asset_root()).map_err(|error| {
        format!("缺少可选音频资产（FFmpeg 与音频模型），请在设置页主动下载；不会自动联网：{error}")
    })
}

/// 检查文档场景及许可证；读取应用配置时可首次迁移旧文本，不联网。
pub fn readiness() -> Result<(), String> {
    // 先校验内置清单本身：失效清单不得被当作可运行环境。
    load_manifest()?;
    let runtime = runtime_dir()?;
    validate_runtime_dir(&runtime)?;
    ensure_document_notice()?;
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

#[derive(Debug, Deserialize, Serialize)]
struct AudioAsset {
    #[serde(flatten)]
    component: FeatureComponent,
    url: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct ComponentReceipt {
    owner: String,
    tag: String,
    source: String,
    manifest: ReleaseAsset,
    components: Vec<FeatureComponent>,
    audio_assets: AudioAsset,
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn optional_audio_path(path: &str) -> bool {
    path.starts_with("ffmpeg/")
        || path.starts_with("models/vad/")
        || path.strip_prefix("models/").is_some_and(|tail| {
            tail.split_once('/')
                .is_some_and(|(directory, _)| directory.starts_with("sense_voice_"))
        })
        || path.starts_with("samples/")
}

fn validate_member(member: &AssetFile) -> Result<(), String> {
    validate_relative_path(&member.path)?;
    // Windows 路径别名、ADS、大小写冲突不能借用音频清单覆盖基础成员。
    if member.path.contains('\\')
        || !valid_digest(&member.sha256)
        || member.path.split('/').any(|part| {
            let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
            part.ends_with(['.', ' '])
                || part.chars().any(|c| c < ' ' || "<>\"|?*".contains(c))
                || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                || (stem.len() == 4
                    && (stem.starts_with("COM") || stem.starts_with("LPT"))
                    && matches!(stem.as_bytes()[3], b'1'..=b'9'))
        })
    {
        return Err(format!("Xberg 收据成员路径或摘要无效：{}", member.path));
    }
    Ok(())
}

fn load_receipt(root: &Path) -> Result<ComponentReceipt, String> {
    let bytes = fs::read(root.join(XBERG_RELEASE_RECEIPT))
        .map_err(|error| format!("无法读取随包 Xberg 发布收据，请修复安装包：{error}"))?;
    let receipt: ComponentReceipt = serde_json::from_slice(&bytes)
        .map_err(|error| format!("随包 Xberg 发布收据无效：{error}"))?;
    let release_url = |archive: &str| {
        format!(
            "https://github.com/jchanghong023/xberg/releases/download/{}/{archive}",
            receipt.tag
        )
    };
    if receipt.owner != "JchTools-bundled-xberg-v1"
        || receipt.source != XBERG_LATEST_RELEASE_URL
        || !valid_component_tag(&receipt.tag)
        || receipt.manifest.archive != XBERG_COMPONENT_MANIFEST
        || receipt.manifest.url != release_url(XBERG_COMPONENT_MANIFEST)
        || receipt.manifest.size_bytes == 0
        || !valid_digest(&receipt.manifest.sha256)
        || receipt.components.len() != 2
    {
        return Err("随包 Xberg 发布收据来源、版本或基础组件无效".into());
    }
    let mut ids = BTreeSet::new();
    let mut base_paths: BTreeMap<String, &AssetFile> = BTreeMap::new();
    for component in &receipt.components {
        if !matches!(component.id.as_str(), "document" | "snapshot")
            || !ids.insert(component.id.as_str())
            || !component.requires.is_empty()
            || component.archive != format!("xberg-{}-{XBERG_TARGET}.zip", component.id)
            || component.size_bytes == 0
            || !valid_digest(&component.sha256)
            || component.members.is_empty()
        {
            return Err("随包 Xberg 基础组件记录无效".into());
        }
        let mut own_paths = BTreeSet::new();
        for member in &component.members {
            validate_member(member)?;
            let key = member.path.to_ascii_lowercase();
            if optional_audio_path(&key)
                || key == XBERG_RELEASE_RECEIPT
                || !own_paths.insert(key.clone())
                || base_paths
                    .get(&key)
                    .is_some_and(|previous| *previous != member)
            {
                return Err("随包 Xberg 基础成员冲突或混入可选音频资产".into());
            }
            base_paths.insert(key, member);
        }
    }
    let audio = &receipt.audio_assets.component;
    if audio.id != "audio-assets"
        || audio.archive != AUDIO_ARCHIVE
        || receipt.audio_assets.url != release_url(AUDIO_ARCHIVE)
        || !audio.requires.is_empty()
        || audio.size_bytes == 0
        || !valid_digest(&audio.sha256)
        || audio.members.is_empty()
    {
        return Err("随包 Xberg 音频收据与锁定发布资产不一致".into());
    }
    let mut audio_paths = BTreeSet::new();
    for member in &audio.members {
        validate_member(member)?;
        let key = member.path.to_ascii_lowercase();
        if !optional_audio_path(&member.path)
            || base_paths.contains_key(&key)
            || !audio_paths.insert(key)
        {
            return Err("音频收据包含基础文件、未知目录或重复成员，拒绝安装".into());
        }
    }
    Ok(receipt)
}

fn safe_member_path(root: &Path, relative: &str) -> Result<PathBuf, String> {
    validate_relative_path(relative)?;
    let mut current = root.to_path_buf();
    for segment in std::iter::once(None).chain(relative.split('/').map(Some)) {
        if let Some(segment) = segment {
            current.push(segment);
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                let mut linked = metadata.file_type().is_symlink();
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    linked |= metadata.file_attributes() & 0x400 != 0;
                }
                if linked {
                    return Err("音频资产路径包含符号链接或重解析点，拒绝安装".into());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("检查音频资产路径失败：{error}")),
        }
    }
    Ok(current)
}

fn verify_member(root: &Path, member: &AssetFile, cancel: &AtomicBool) -> Result<(), String> {
    verify_file_with_cancel(
        &safe_member_path(root, &member.path)?,
        member.size_bytes,
        &member.sha256,
        cancel,
    )
}

fn verify_component(
    root: &Path,
    component: &FeatureComponent,
    cancel: &AtomicBool,
) -> Result<(), String> {
    for member in &component.members {
        ensure_not_cancelled(cancel)?;
        verify_member(root, member, cancel)?;
    }
    Ok(())
}

fn collect_files(root: &Path, paths: &mut BTreeSet<String>, relative: &Path) -> Result<(), String> {
    for entry in
        fs::read_dir(root.join(relative)).map_err(|error| format!("读取解包目录失败：{error}"))?
    {
        let entry = entry.map_err(|error| format!("读取解包成员失败：{error}"))?;
        let path = relative.join(entry.file_name());
        let kind = entry
            .file_type()
            .map_err(|error| format!("读取解包成员类型失败：{error}"))?;
        if kind.is_dir() {
            collect_files(root, paths, &path)?;
        } else if kind.is_file() {
            paths.insert(
                path.to_str()
                    .ok_or("组件路径无法编码为 Unicode")?
                    .replace('\\', "/"),
            );
        } else {
            return Err("组件归档包含非普通文件".into());
        }
    }
    Ok(())
}

/// 只在用户主动操作时下载当前生效引擎锁定的音频附加包，不查询最新发布。
pub fn download_audio_runtime(
    cancel: &AtomicBool,
    progress: impl FnMut(String),
) -> Result<PathBuf, String> {
    download_audio_runtime_with(cancel, progress, &mut NetworkDownloader)
}

/// 注入真实资产供给器；来源、归档、成员与落位仍经过完整生产校验。
#[cfg(any(test, feature = "test-hooks"))]
pub fn download_audio_runtime_with_downloader(
    cancel: &AtomicBool,
    progress: impl FnMut(String),
    downloader: &mut dyn AssetDownloader,
) -> Result<PathBuf, String> {
    download_audio_runtime_with(cancel, progress, downloader)
}

fn download_audio_runtime_with(
    cancel: &AtomicBool,
    mut progress: impl FnMut(String),
    downloader: &mut dyn AssetDownloader,
) -> Result<PathBuf, String> {
    let _guard = OPTIONAL_INSTALL.lock().map_err(|_| "音频安装锁不可用")?;
    ensure_not_cancelled(cancel)?;
    let root = runtime_dir()?;
    let receipt = load_receipt(&root)?;
    #[cfg(any(test, feature = "test-hooks"))]
    audio_download_test_barrier(cancel, &mut progress)?;
    let audio = &receipt.audio_assets.component;
    if verify_component(&root, audio, cancel).is_ok() {
        ensure_not_cancelled(cancel)?;
        progress(format!("已复用 {} 的完整音频资产，无需下载", receipt.tag));
        return Ok(root);
    }
    ensure_not_cancelled(cancel)?;
    with_audio_staging(&root, &mut progress, |staging, progress| {
        progress(format!("正在下载 {} 的可选音频资产…", receipt.tag));
        let archive = staging.join(AUDIO_ARCHIVE);
        downloader.download(
            &receipt.audio_assets.url,
            &archive,
            audio.size_bytes,
            &audio.sha256,
            cancel,
            progress,
        )?;
        verify_file_with_cancel(&archive, audio.size_bytes, &audio.sha256, cancel)?;
        let extracted = staging.join("extracted");
        fs::create_dir(&extracted).map_err(|error| format!("创建音频解包目录失败：{error}"))?;
        jchtools_assets::asset_util::extract_zip_safely(&archive, &extracted, cancel)?;
        let mut actual = BTreeSet::new();
        collect_files(&extracted, &mut actual, Path::new(""))?;
        let expected = audio
            .members
            .iter()
            .map(|member| format!("{XBERG_ARCHIVE_PREFIX}/{}", member.path))
            .collect::<BTreeSet<_>>();
        if actual != expected {
            return Err("音频归档成员与锁定收据不一致，拒绝安装".into());
        }
        let source = extracted.join(XBERG_ARCHIVE_PREFIX);
        verify_component(&source, audio, cancel)?;
        install_audio_members(&root, &source, &audio.members, staging, cancel, || {
            // 同根补入资产不切换来源、不重启正在执行的唯一 worker。
            ensure_not_cancelled(cancel)
        })?;
        progress("可选音频资产已安装，现有文档与截图任务不受影响".into());
        Ok(root.clone())
    })
}

/// 真实 GUI 取消路径的离线屏障：仅隔离测试构建可启用，不供应伪造资产或成功结果。
#[cfg(any(test, feature = "test-hooks"))]
fn audio_download_test_barrier(
    cancel: &AtomicBool,
    progress: &mut impl FnMut(String),
) -> Result<(), String> {
    let Some(marker) = std::env::var_os("JCHTOOLS_TEST_AUDIO_DOWNLOAD_BARRIER") else {
        return Ok(());
    };
    let state = std::env::var_os("JCHTOOLS_TEST_STATE_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or("音频下载测试屏障必须使用隔离状态目录")?;
    let marker = PathBuf::from(marker);
    let parent = marker.parent().ok_or("音频下载测试屏障缺少父目录")?;
    let state =
        fs::canonicalize(state).map_err(|error| format!("解析测试状态目录失败：{error}"))?;
    let parent =
        fs::canonicalize(parent).map_err(|error| format!("解析测试屏障目录失败：{error}"))?;
    if !marker.is_absolute() || !parent.starts_with(&state) {
        return Err("音频下载测试屏障必须位于隔离状态目录内".into());
    }
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
        .map_err(|error| format!("创建音频下载测试屏障失败：{error}"))?;
    progress("测试音频下载已进入可取消等待，未发出网络请求".into());
    loop {
        ensure_not_cancelled(cancel)?;
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn with_audio_staging<T>(
    root: &Path,
    progress: &mut impl FnMut(String),
    operation: impl FnOnce(&Path, &mut dyn FnMut(String)) -> Result<T, String>,
) -> Result<T, String> {
    let staging = root.join(format!(".audio-staging-{}", Uuid::new_v4()));
    fs::create_dir(&staging).map_err(|error| format!("创建音频暂存目录失败：{error}"))?;
    let result = operation(&staging, progress);
    // 回滚失败的备份必须保留供恢复，绝不以清理吞掉原文件。
    if !staging.join("recovery-required").exists() {
        if let Err(error) = fs::remove_dir_all(&staging) {
            tracing::warn!(event = "audio_staging_cleanup_failed", error_kind = ?error.kind(), "音频暂存目录清理失败");
            progress(format!("警告：音频暂存目录清理失败：{error}"));
        }
    }
    result
}

/// 成员事务只写附加包路径；有效现有成员原位复用，失败恢复原有无效成员。
fn install_audio_members(
    root: &Path,
    source: &Path,
    members: &[AssetFile],
    staging: &Path,
    cancel: &AtomicBool,
    commit: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let mut installed = Vec::new();
    let mut backups = Vec::new();
    let mut directories = Vec::new();
    let result = (|| {
        for (index, member) in members.iter().enumerate() {
            ensure_not_cancelled(cancel)?;
            if !optional_audio_path(&member.path) {
                return Err("拒绝通过音频安装覆盖基础文件".into());
            }
            if verify_member(root, member, cancel).is_ok() {
                continue;
            }
            ensure_not_cancelled(cancel)?;
            verify_member(source, member, cancel)?;
            let target = safe_member_path(root, &member.path)?;
            let parent = target.parent().ok_or("音频成员缺少父目录")?;
            let mut missing = Vec::new();
            let mut cursor = parent;
            while !cursor.exists() {
                missing.push(cursor.to_path_buf());
                cursor = cursor.parent().ok_or("音频目录缺少父目录")?;
            }
            for directory in missing.into_iter().rev() {
                fs::create_dir(&directory)
                    .map_err(|error| format!("创建音频成员目录失败：{error}"))?;
                directories.push(directory);
            }
            let prepared = staging.join(format!("member-{index}"));
            fs::copy(safe_member_path(source, &member.path)?, &prepared)
                .map_err(|error| format!("暂存音频成员失败：{error}"))?;
            // Windows FlushFileBuffers 需要写权限；仅打开本次自有暂存文件。
            File::options()
                .write(true)
                .open(&prepared)
                .and_then(|file| file.sync_all())
                .map_err(|error| format!("同步音频暂存成员失败：{error}"))?;
            verify_file_with_cancel(&prepared, member.size_bytes, &member.sha256, cancel)?;
            ensure_not_cancelled(cancel)?;
            safe_member_path(root, &member.path)?;
            if target.exists() {
                if !target.is_file() {
                    return Err("音频目标被非普通文件占用".into());
                }
                let backup = staging.join(format!("backup-{index}"));
                fs::rename(&target, &backup)
                    .map_err(|error| format!("备份已有音频成员失败：{error}"))?;
                backups.push((target.clone(), backup));
            }
            // 同一文件系统内以 create-new 语义发布，绝不覆盖并发出现的文件。
            fs::hard_link(&prepared, &target)
                .map_err(|error| format!("安装音频成员失败：{error}"))?;
            installed.push(target);
        }
        ensure_not_cancelled(cancel)?;
        commit()
    })();
    if let Err(error) = result {
        let mut recovery = Vec::new();
        for path in installed.iter().rev() {
            if let Err(error) = fs::remove_file(path) {
                recovery.push(format!("删除本轮音频成员失败：{error}"));
            }
        }
        for (target, backup) in backups.iter().rev() {
            if let Err(error) = fs::hard_link(backup, target) {
                recovery.push(format!("恢复原音频成员失败：{error}"));
            }
        }
        for directory in directories.iter().rev() {
            let _ = fs::remove_dir(directory);
        }
        if !recovery.is_empty() {
            let _ = fs::write(staging.join("recovery-required"), b"keep backups");
            return Err(format!(
                "{error}；{}；原文件备份保留于 {}",
                recovery.join("；"),
                staging.display()
            ));
        }
        return Err(error);
    }
    Ok(())
}

/// 离线迁移兼容音频成员后，交由 broker 在空闲边界提交唯一随包运行目录。
/// 迁移失败保留并返回旧生效目录，不把已就绪媒体静默变为不可用。
pub fn migrate_bundled_runtime(
    cancel: &AtomicBool,
    progress: impl FnMut(String),
) -> Result<PathBuf, String> {
    migrate_bundled_runtime_with_activation(cancel, progress, |root| {
        jchtools_xberg::xberg_runtime::activate_bundled_runtime(root)
    })
}

fn migrate_bundled_runtime_with_activation(
    cancel: &AtomicBool,
    mut progress: impl FnMut(String),
    mut activate: impl FnMut(&Path) -> Result<(), String>,
) -> Result<PathBuf, String> {
    let _guard = OPTIONAL_INSTALL.lock().map_err(|_| "音频迁移锁不可用")?;
    let previous = jchtools_foundation::xberg_settings::effective_directory()?;
    let mut uncertain_commit = false;
    let attempt = (|| {
        ensure_not_cancelled(cancel)?;
        let bundled = jchtools_foundation::xberg_settings::bundled_directory()?
            .ok_or("未找到随包 Xberg 目录，请修复安装包")?;
        let bundled =
            fs::canonicalize(&bundled).map_err(|error| format!("解析随包运行目录失败：{error}"))?;
        let receipt = load_receipt(&bundled)?;
        jchtools_xberg::xberg_runtime::validate_assets(&bundled, "document")?;
        jchtools_xberg::xberg_runtime::validate_assets(&bundled, "snapshot")?;
        if previous.as_ref() == Some(&bundled) {
            ensure_not_cancelled(cancel)?;
            activate(&bundled)?;
            return Ok(bundled);
        }
        with_audio_staging(&bundled, &mut progress, |staging, progress| {
            let mut compatible = Vec::new();
            let old_media_ready = previous.as_ref().is_some_and(|old| {
                jchtools_xberg::xberg_runtime::validate_assets(old, "media").is_ok()
            });
            for member in &receipt.audio_assets.component.members {
                ensure_not_cancelled(cancel)?;
                if verify_member(&bundled, member, cancel).is_ok() {
                    continue;
                }
                ensure_not_cancelled(cancel)?;
                if let Some(old) = &previous {
                    if verify_member(old, member, cancel).is_ok() {
                        compatible.push(member.clone());
                        continue;
                    }
                }
                ensure_not_cancelled(cancel)?;
                if old_media_ready {
                    return Err(format!(
                        "旧目录音频已就绪，但成员与目标发布不兼容：{}；保留旧来源",
                        member.path
                    ));
                }
            }
            let source = previous.as_deref().unwrap_or(&bundled);
            progress(format!(
                "离线迁移 {} 项目标发布兼容的音频成员",
                compatible.len()
            ));
            let mut uncertain = None;
            install_audio_members(&bundled, source, &compatible, staging, cancel, || {
                ensure_not_cancelled(cancel)?;
                if old_media_ready {
                    jchtools_xberg::xberg_runtime::validate_assets(&bundled, "media")?;
                }
                match activate(&bundled) {
                    Ok(()) => Ok(()),
                    Err(error) => {
                        match jchtools_foundation::xberg_settings::effective_directory() {
                            Ok(Some(active)) if active == bundled => {
                                tracing::warn!(
                                    event = "bundled_runtime_commit_reply_lost",
                                    "来源已提交但代理确认失败，保留目标音频资产"
                                );
                                Ok(())
                            }
                            Ok(_) => Err(error),
                            Err(read_error) => {
                                // IPC 确认丢失且配置暂不可读时不能删除可能已生效的媒体文件。
                                uncertain_commit = true;
                                uncertain = Some(format!("来源提交状态暂不可确认，已保留目标音频资产：{error}；{read_error}"));
                                Ok(())
                            }
                        }
                    }
                }
            })?;
            if let Some(error) = uncertain {
                return Err(error);
            }
            Ok(bundled.clone())
        })
    })();
    match attempt {
        Ok(root) => Ok(root),
        Err(error) => {
            tracing::warn!(
                event = "bundled_runtime_migration_deferred",
                cancelled = cancel.load(std::sync::atomic::Ordering::Acquire),
                "随包运行目录迁移未获确认，保留既有文件与来源记录"
            );
            if uncertain_commit {
                progress(error.clone());
                return Err(error);
            }
            if let Some(previous) = previous {
                progress(format!("随包迁移未提交，继续使用原运行目录：{error}"));
                Ok(previous)
            } else {
                Err(error)
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
    text.push_str("文档与截图基础运行资产随应用提供；FFmpeg 和音频模型仅在设置页主动下载。各模型与推理运行库的许可随运行目录自带。\n\n");
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
        previous_env: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    fn redirect_component_env() -> ComponentGuard {
        let lock = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temporary_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.tmp");
        fs::create_dir_all(&temporary_root).expect("创建仓库临时目录");
        let root = tempfile::tempdir_in(temporary_root).expect("创建资产根目录");
        let previous_env = [
            "JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT", "JCHTOOLS_TEST_STATE_DIR",
            "JCHTOOLS_TEST_BUNDLED_XBERG_DIR", "JCHTOOLS_TEST_AUDIO_DOWNLOAD_BARRIER",
        ].into_iter().map(|name| (name, std::env::var_os(name))).collect();
        std::env::set_var("JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT", root.path());
        std::env::set_var("JCHTOOLS_TEST_STATE_DIR", root.path().join("app-settings"));
        std::env::remove_var("JCHTOOLS_TEST_BUNDLED_XBERG_DIR");
        std::env::remove_var("JCHTOOLS_TEST_AUDIO_DOWNLOAD_BARRIER");
        ComponentGuard { root, _lock: lock, previous_env }
    }

    impl Drop for ComponentGuard {
        fn drop(&mut self) {
            for (name, value) in &self.previous_env {
                if let Some(value) = value {
                    std::env::set_var(name, value);
                } else {
                    std::env::remove_var(name);
                }
            }
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

    struct AudioFixture {
        receipt: super::ComponentReceipt,
        archive: Vec<u8>,
        files: Vec<(String, Vec<u8>)>,
        requests: Vec<String>,
        cancel_download: bool,
    }

    impl AudioFixture {
        fn new() -> Self {
            let files: Vec<(String, Vec<u8>)> = [
                ("ffmpeg/avcodec-63.dll", b"codec".as_slice()),
                ("ffmpeg/LICENSE.txt", b"audio license".as_slice()),
                ("ffmpeg/avformat-63.dll", b"format".as_slice()),
                ("ffmpeg/avutil-61.dll", b"utility".as_slice()),
                ("ffmpeg/swresample-7.dll", b"resample".as_slice()),
                ("models/vad/silero_vad.onnx", b"vad".as_slice()),
                ("models/sense_voice_zh_en_ja_ko_yue_2024_07_17/model.int8.onnx", b"voice".as_slice()),
                ("models/sense_voice_zh_en_ja_ko_yue_2024_07_17/tokens.txt", b"tokens".as_slice()),
            ].into_iter().map(|(path, bytes)| (path.into(), bytes.to_vec())).collect::<Vec<_>>();
            let member = |path: &str, bytes: &[u8]| super::AssetFile {
                path: path.into(), size_bytes: bytes.len() as u64, sha256: sha256_bytes(bytes),
            };
            let components = ["document", "snapshot"].into_iter().map(|id| {
                super::FeatureComponent {
                    id: id.into(), archive: format!("xberg-{id}-{}.zip", super::XBERG_TARGET),
                    requires: Vec::new(), size_bytes: 1, sha256: sha256_bytes(b"base zip"),
                    members: vec![member("xberg.exe", b"base engine"), member("LICENSE", b"base license")],
                }
            }).collect();
            let mut fixture = Self {
                receipt: super::ComponentReceipt {
                    owner: "JchTools-bundled-xberg-v1".into(), tag: "v2099.1".into(),
                    source: super::XBERG_LATEST_RELEASE_URL.into(),
                    manifest: super::ReleaseAsset {
                        archive: super::XBERG_COMPONENT_MANIFEST.into(),
                        url: format!("https://github.com/jchanghong023/xberg/releases/download/v2099.1/{}", super::XBERG_COMPONENT_MANIFEST),
                        size_bytes: 1, sha256: sha256_bytes(b"manifest"),
                    },
                    components,
                    audio_assets: super::AudioAsset {
                        component: super::FeatureComponent {
                            id: "audio-assets".into(), archive: super::AUDIO_ARCHIVE.into(),
                            requires: Vec::new(), size_bytes: 1, sha256: String::new(),
                            members: files.iter().map(|(path, bytes)| member(path, bytes)).collect(),
                        },
                        url: format!("https://github.com/jchanghong023/xberg/releases/download/v2099.1/{}", super::AUDIO_ARCHIVE),
                    },
                },
                archive: Vec::new(), files, requests: Vec::new(), cancel_download: false,
            };
            fixture.rebuild_archive();
            fixture
        }

        fn rebuild_archive(&mut self) {
            use std::io::{Cursor, Write};
            let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
            for (path, bytes) in &self.files {
                zip.start_file(
                    format!("{}/{path}", super::XBERG_ARCHIVE_PREFIX),
                    zip::write::SimpleFileOptions::default(),
                ).unwrap();
                zip.write_all(bytes).unwrap();
            }
            self.archive = zip.finish().unwrap().into_inner();
            self.receipt.audio_assets.component.size_bytes = self.archive.len() as u64;
            self.receipt.audio_assets.component.sha256 = sha256_bytes(&self.archive);
        }

        fn prepare(&self, root: &Path) {
            fs::create_dir_all(root).unwrap();
            for component in &self.receipt.components {
                for member in &component.members {
                    let path = root.join(&member.path);
                    fs::create_dir_all(path.parent().unwrap()).unwrap();
                    fs::write(path, if member.path == "xberg.exe" { b"base engine".as_slice() } else { b"base license".as_slice() }).unwrap();
                }
            }
            self.write_receipt(root);
        }

        fn write_receipt(&self, root: &Path) {
            fs::write(root.join(super::XBERG_RELEASE_RECEIPT), serde_json::to_vec(&self.receipt).unwrap()).unwrap();
        }

        fn install(&mut self) -> Result<PathBuf, String> {
            super::download_audio_runtime_with_downloader(&AtomicBool::new(false), |_| {}, self)
        }

        fn write_audio(&self, root: &Path) {
            for (relative, bytes) in &self.files {
                let path = root.join(relative);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, bytes).unwrap();
            }
        }

        fn prepare_migration_base(&self, root: &Path) {
            self.prepare(root);
            for member in load_manifest().unwrap().xberg.members {
                if super::optional_audio_path(&member.path) {
                    continue;
                }
                let path = root.join(&member.path);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                if !path.exists() {
                    fs::write(path, b"base").unwrap();
                }
            }
            for relative in [
                "models/snapshot-ocr/det.onnx", "models/snapshot-ocr/rec.onnx",
                "models/snapshot-ocr/dict/dict.txt",
            ] {
                let path = root.join(relative);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, b"snapshot").unwrap();
            }
        }
    }

    impl AssetDownloader for AudioFixture {
        fn download(
            &mut self, url: &str, destination: &Path, _: u64, _: &str,
            cancel: &AtomicBool, _: &mut dyn FnMut(String),
        ) -> Result<(), String> {
            self.requests.push(url.into());
            fs::write(destination, &self.archive).map_err(|error| format!("夹具写入失败：{error}"))?;
            if self.cancel_download {
                cancel.store(true, std::sync::atomic::Ordering::Release);
            }
            Ok(())
        }
    }

    #[test]
    fn audio_download_uses_only_receipt_pin_and_reuses_complete_install() {
        let guard = redirect_component_env();
        let root = guard.root.path().join("bundle");
        let mut fixture = AudioFixture::new();
        fixture.prepare(&root);
        jchtools_foundation::xberg_settings::save(&root).unwrap();
        let original = jchtools_foundation::xberg_settings::required().unwrap();
        assert_eq!(fixture.install().unwrap(), original);
        assert_eq!(fixture.requests, [fixture.receipt.audio_assets.url.clone()]);
        assert_eq!(fs::read(root.join("xberg.exe")).unwrap(), b"base engine");
        fixture.install().unwrap();
        assert_eq!(fixture.requests.len(), 1, "完整音频资产不得重复联网");
        assert_eq!(jchtools_foundation::xberg_settings::required().unwrap(), original);
        super::verify_component(&root, &fixture.receipt.audio_assets.component, &AtomicBool::new(false)).unwrap();
    }

    #[test]
    fn audio_integrity_cancel_and_hostile_archives_preserve_existing_files() {
        let guard = redirect_component_env();
        for failure in ["digest", "size", "cancel", "traversal", "base_overwrite", "extra", "member_digest", "missing"] {
            let root = guard.root.path().join(failure);
            let mut fixture = AudioFixture::new();
            fixture.prepare(&root);
            fs::create_dir_all(root.join("ffmpeg")).unwrap();
            fs::write(root.join("ffmpeg/LICENSE.txt"), b"audio license").unwrap();
            fs::write(root.join("ffmpeg/avcodec-63.dll"), b"old incomplete codec").unwrap();
            jchtools_foundation::xberg_settings::save(&root).unwrap();
            let original = jchtools_foundation::xberg_settings::required().unwrap();
            match failure {
                "digest" => fixture.archive[0] ^= 1,
                "size" => { fixture.archive.pop(); }
                "cancel" => fixture.cancel_download = true,
                "traversal" => fixture.files.push(("../escape".into(), b"bad".to_vec())),
                "base_overwrite" => fixture.files.push(("xberg.exe".into(), b"bad".to_vec())),
                "extra" => fixture.files.push(("ffmpeg/unlisted.dll".into(), b"bad".to_vec())),
                "member_digest" => fixture.files[0].1 = b"bad".to_vec(),
                "missing" => { fixture.files.pop(); }
                _ => unreachable!(),
            }
            if !matches!(failure, "digest" | "size" | "cancel") {
                fixture.rebuild_archive();
                fixture.write_receipt(&root);
            }
            assert!(fixture.install().is_err(), "{failure}");
            assert_eq!(fs::read(root.join("xberg.exe")).unwrap(), b"base engine", "{failure}");
            assert_eq!(fs::read(root.join("ffmpeg/LICENSE.txt")).unwrap(), b"audio license", "{failure}");
            assert_eq!(fs::read(root.join("ffmpeg/avcodec-63.dll")).unwrap(), b"old incomplete codec", "{failure}");
            assert_eq!(jchtools_foundation::xberg_settings::required().unwrap(), original);
        }
    }

    #[test]
    fn audio_receipt_rejects_unpinned_urls_and_base_member_aliases_before_network() {
        let guard = redirect_component_env();
        for path in ["../xberg.exe", "xberg.exe", "sherpa-onnx/onnxruntime.dll", "ffmpeg/../xberg.exe", "ffmpeg/CON.dll", "ffmpeg/file.dll.", "ffmpeg/file:ads"] {
            let root = guard.root.path().join(uuid::Uuid::new_v4().to_string());
            let mut fixture = AudioFixture::new();
            fixture.prepare(&root);
            fixture.receipt.audio_assets.component.members[0].path = path.into();
            fixture.write_receipt(&root);
            jchtools_foundation::xberg_settings::save(&root).unwrap();
            assert!(fixture.install().is_err(), "{path}");
            assert!(fixture.requests.is_empty());
            assert_eq!(fs::read(root.join("xberg.exe")).unwrap(), b"base engine");
        }
        let root = guard.root.path().join("wrong-release");
        let mut fixture = AudioFixture::new();
        fixture.prepare(&root);
        fixture.receipt.audio_assets.url = fixture.receipt.audio_assets.url.replace("v2099.1", "latest");
        fixture.write_receipt(&root);
        jchtools_foundation::xberg_settings::save(&root).unwrap();
        assert!(fixture.install().is_err());
        assert!(fixture.requests.is_empty());
    }

    #[test]
    fn audio_member_commit_failure_restores_invalid_and_keeps_valid_members() {
        let guard = redirect_component_env();
        let root = guard.root.path().join("target");
        let source = guard.root.path().join("source");
        let staging = guard.root.path().join("staging");
        let fixture = AudioFixture::new();
        fixture.prepare(&root);
        fixture.write_audio(&source);
        fs::create_dir(&staging).unwrap();
        fs::create_dir_all(root.join("ffmpeg")).unwrap();
        fs::write(root.join("ffmpeg/avcodec-63.dll"), b"old bytes").unwrap();
        fs::write(root.join("ffmpeg/LICENSE.txt"), b"audio license").unwrap();
        let error = super::install_audio_members(
            &root, &source, &fixture.receipt.audio_assets.component.members, &staging,
            &AtomicBool::new(false), || Err("配置提交失败".into()),
        ).unwrap_err();
        assert_eq!(error, "配置提交失败");
        assert_eq!(fs::read(root.join("ffmpeg/avcodec-63.dll")).unwrap(), b"old bytes");
        assert_eq!(fs::read(root.join("ffmpeg/LICENSE.txt")).unwrap(), b"audio license");
        assert!(!root.join("models/vad/silero_vad.onnx").exists());
        assert_eq!(fs::read(source.join("ffmpeg/avcodec-63.dll")).unwrap(), b"codec");
    }

    #[test]
    fn bundled_migration_copies_only_compatible_members_and_preserves_source() {
        let guard = redirect_component_env();
        let fixture = AudioFixture::new();
        let old = guard.root.path().join("old");
        let bundle = guard.root.path().join("bundle");
        fixture.prepare_migration_base(&old);
        fixture.write_audio(&old);
        jchtools_xberg::xberg_runtime::validate_assets(&old, "media").unwrap();
        fixture.prepare_migration_base(&bundle);
        fs::write(old.join("unrelated-private.bin"), b"never copy").unwrap();
        jchtools_foundation::xberg_settings::save(&old).unwrap();
        std::env::set_var("JCHTOOLS_TEST_BUNDLED_XBERG_DIR", &bundle);
        let result = super::migrate_bundled_runtime_with_activation(
            &AtomicBool::new(false), |_| {}, |root| {
                jchtools_foundation::xberg_settings::save_bundled_directory(root)
            },
        ).unwrap();
        assert_eq!(result, fs::canonicalize(&bundle).unwrap());
        assert_eq!(jchtools_foundation::xberg_settings::required().unwrap(), result);
        super::verify_component(&bundle, &fixture.receipt.audio_assets.component, &AtomicBool::new(false)).unwrap();
        super::verify_component(&old, &fixture.receipt.audio_assets.component, &AtomicBool::new(false)).unwrap();
        assert!(!bundle.join("unrelated-private.bin").exists());
        assert_eq!(fs::read(bundle.join("xberg.exe")).unwrap(), b"base engine");
    }

    #[test]
    fn bundled_migration_commit_or_cancel_failure_keeps_old_configuration_and_audio() {
        let guard = redirect_component_env();
        let fixture = AudioFixture::new();
        let old = guard.root.path().join("old");
        let bundle = guard.root.path().join("bundle");
        fixture.prepare_migration_base(&old);
        fixture.write_audio(&old);
        jchtools_xberg::xberg_runtime::validate_assets(&old, "media").unwrap();
        fixture.prepare_migration_base(&bundle);
        jchtools_foundation::xberg_settings::save(&old).unwrap();
        let original = jchtools_foundation::xberg_settings::required().unwrap();
        std::env::set_var("JCHTOOLS_TEST_BUNDLED_XBERG_DIR", &bundle);
        let db = jchtools_foundation::xberg_settings::open(
            &jchtools_foundation::xberg_settings::state_dir().unwrap(),
        ).unwrap();
        db.execute_batch(
            "CREATE TRIGGER reject_bundle BEFORE INSERT ON app_settings
             WHEN NEW.key='xberg_source' AND NEW.value='bundled'
             BEGIN SELECT RAISE(ABORT,'injected failure'); END;",
        ).unwrap();
        let mut diagnostics = Vec::new();
        let result = super::migrate_bundled_runtime_with_activation(
            &AtomicBool::new(false), |message| diagnostics.push(message),
            |root| jchtools_foundation::xberg_settings::save_bundled_directory(root),
        ).unwrap();
        assert_eq!(result, original);
        assert_eq!(jchtools_foundation::xberg_settings::required().unwrap(), original);
        assert!(diagnostics.iter().any(|message| message.contains("继续使用原运行目录")));
        assert!(!bundle.join("ffmpeg/avcodec-63.dll").exists());
        super::verify_component(&old, &fixture.receipt.audio_assets.component, &AtomicBool::new(false)).unwrap();
        let result = super::migrate_bundled_runtime_with_activation(
            &AtomicBool::new(true), |_| {}, |_| panic!("取消不能切源"),
        ).unwrap();
        assert_eq!(result, original);
    }

    #[test]
    fn bundled_migration_retains_ready_old_media_until_target_is_compatible() {
        let guard = redirect_component_env();
        let fixture = AudioFixture::new();
        let old = guard.root.path().join("old");
        let bundle = guard.root.path().join("bundle");
        fixture.prepare_migration_base(&old);
        fixture.write_audio(&old);
        fixture.prepare_migration_base(&bundle);
        fs::write(old.join("ffmpeg/avcodec-63.dll"), b"incompatible but present").unwrap();
        jchtools_xberg::xberg_runtime::validate_assets(&old, "media").unwrap();
        jchtools_foundation::xberg_settings::save(&old).unwrap();
        let original = jchtools_foundation::xberg_settings::required().unwrap();
        std::env::set_var("JCHTOOLS_TEST_BUNDLED_XBERG_DIR", &bundle);
        let mut called = false;
        let result = super::migrate_bundled_runtime_with_activation(
            &AtomicBool::new(false), |_| {}, |_| { called = true; Ok(()) },
        ).unwrap();
        assert!(!called, "已就绪旧媒体不兼容时不能切源");
        assert_eq!(result, original);
        assert_eq!(jchtools_foundation::xberg_settings::required().unwrap(), original);
        assert!(!bundle.join("ffmpeg").exists());
        assert_eq!(fs::read(old.join("ffmpeg/avcodec-63.dll")).unwrap(), b"incompatible but present");
        fixture.write_audio(&bundle);
        let result = super::migrate_bundled_runtime_with_activation(
            &AtomicBool::new(false), |_| {}, |root| {
                jchtools_foundation::xberg_settings::save_bundled_directory(root)
            },
        ).unwrap();
        assert_eq!(result, fs::canonicalize(&bundle).unwrap());
        jchtools_xberg::xberg_runtime::validate_assets(&result, "media").unwrap();
        assert_eq!(fs::read(old.join("ffmpeg/avcodec-63.dll")).unwrap(), b"incompatible but present");
    }

    #[test]
    fn bundled_default_closes_notice_readiness_without_manual_initialization() {
        let guard = redirect_component_env();
        let fixture = AudioFixture::new();
        let bundle = guard.root.path().join("bundle");
        fixture.prepare_migration_base(&bundle);
        std::env::set_var("JCHTOOLS_TEST_BUNDLED_XBERG_DIR", &bundle);
        assert!(!guard.root.path().join("licenses/THIRD_PARTY_NOTICES.md").exists());
        let root = super::migrate_bundled_runtime_with_activation(
            &AtomicBool::new(false), |_| {}, |root| {
                jchtools_foundation::xberg_settings::save_bundled_directory(root)
            },
        ).unwrap();
        assert_eq!(root, fs::canonicalize(&bundle).unwrap());
        assert_eq!(jchtools_foundation::xberg_settings::settings().unwrap().source,
            jchtools_foundation::xberg_settings::Source::Bundled);
        super::readiness().unwrap();
        assert!(guard.root.path().join("licenses/THIRD_PARTY_NOTICES.md").is_file());
        assert!(super::validate_media().unwrap_err().contains("可选音频资产"));
        if crate::markdown::platform_preflight().is_ok() {
            crate::markdown::readiness_for_groups(&[
                crate::markdown::FormatGroup::Pdf, crate::markdown::FormatGroup::Images,
                crate::markdown::FormatGroup::Media,
            ]).unwrap();
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

    // 覆盖 T-06/XB-20：GUI 与后台同时修复缺失 notice，不得互相替换目录或留下截断成品。
    #[test]
    fn concurrent_notice_repairs_publish_one_complete_notice() {
        let guard = redirect_component_env();
        let expected = guard.root.path().join("expected-notice.md");
        write_notice(&expected, &load_manifest().unwrap()).unwrap();
        let target = guard.root.path().join("licenses/THIRD_PARTY_NOTICES.md");
        let start = std::sync::Arc::new(std::sync::Barrier::new(32));
        let workers: Vec<_> = (0..32).map(|_| {
            let start = start.clone();
            std::thread::spawn(move || {
                start.wait();
                super::ensure_document_notice()
            })
        }).collect();
        let results: Vec<_> = workers.into_iter().map(|worker| worker.join().unwrap()).collect();
        assert!(results.iter().all(Result::is_ok), "并发许可证修复失败：{results:?}");
        assert_eq!(fs::read(&target).unwrap(), fs::read(&expected).unwrap());
    }

    // ── Xberg 推理组件包：下载安装、复用、失败保护与版本一致性（XB-09/XB-10）──

    fn sha256_bytes(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hex::encode(hasher.finalize())
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
