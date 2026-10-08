//! 转 Markdown 的本地资产校验。Xberg 目录由应用级 SQLite 统一提供。
//! 文档与媒体分别校验自己的模型和运行库；设置页主动解析最新版并固定本次发布物，
//! 校验完整后保存为共享下载来源；下载安装、保存共享目录与切换到已下载
//! 来源都会经 [`ensure_document_notice`] 原子补写许可证 notice
//!（完成即满足文档场景 readiness，不再要求补一次初始化）。

use crate::asset_util::{
    atomic_replace_dir, cleanup_stale_staging_dirs, ensure_not_cancelled, finalize_staging,
    require_component_members, require_inference_members_for_scenario, resolve_xberg_component,
    state_dir_asset_root, valid_component_tag, validate_relative_path, verify_file_with_cancel,
    AssetDownloader, InferenceManifest,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write as FmtWrite;
use std::fs::{self, File, OpenOptions};
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use uuid::Uuid;

const MANIFEST: &str = include_str!("../resources/markdown-assets.json");
const DATA_DIRECTORY: &str = "markdown-assets";
pub(crate) const XBERG_TAG: &str = "v2026.10.6-0420-run58.1";
const RUNTIME_SELECTION_FILE: &str = "xberg-runtime-path.txt";
const XBERG_DOWNLOAD_STAGING_MARKER: &str = ".jchtools-xberg-download-staging-v1";
const XBERG_RELEASE_RECEIPT: &str = ".jchtools-xberg-release.json";
const XBERG_LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/jchanghong023/xberg/releases/latest";
const XBERG_ARCHIVE_NAME: &str = "xberg-cli-x86_64-pc-windows-msvc.zip";

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

#[derive(Debug, Deserialize, Serialize)]
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
    crate::xberg_settings::load()
}

/// 校验用户选择的 Xberg 运行目录：只做 document 场景成员存在性检查
/// （T-06 2026-10-02 随 XB-09 修订：不比对大小与 SHA-256，用户可自行替换
/// 或更新引擎版本），缺失时汇总指认缺失项。
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
        if !crate::xberg_runtime::asset_for_scenario(&member.path, "document") {
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
    crate::xberg_settings::save(path)?;
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
/// 返回已保存的 Xberg 运行目录；未选择时返回明确错误。
pub fn runtime_dir() -> Result<PathBuf, String> {
    load_saved_runtime_dir()?.ok_or_else(|| "尚未选择 Xberg 运行目录".to_string())
}

/// Xberg 推理组件安装根：`<资产根>/xberg-inference/`（每个发布版本一个 tag
/// 子目录）。与截图 OCR 各自独立安装（O-03：互不覆盖，只读复用同一发布源）。
#[must_use]
pub fn xberg_inference_root() -> PathBuf {
    asset_root().join("xberg-inference")
}

/// 组件目录解析规则与就绪成员检查共用 [`crate::asset_util`] 的共享实现
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
/// 存在性检查经 [`crate::asset_util::require_inference_members_for_scenario`]
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
    let root = asset_root();
    // B-2：先兜底清理历史残留的 staging（readiness 提前返回、取消后清理
    // 失败或进程崩溃都会残留 .staging-<uuid>，单个可超 1GB）。
    cleanup_stale_staging(&root);
    ensure_not_cancelled(cancel)?;
    if readiness().is_ok() {
        progress("转 Markdown 组件已就绪".to_string());
        return Ok(());
    }
    let manifest = load_manifest()?;
    fs::create_dir_all(&root).map_err(|error| format!("创建资产目录失败：{error}"))?;
    let staging = root.join(format!(".staging-{}", Uuid::new_v4().simple()));
    fs::create_dir_all(&staging).map_err(|error| format!("创建初始化临时目录失败：{error}"))?;
    let result = initialize_staged(&manifest, cancel, &mut progress, &staging, &root);
    finalize_staging(result, &staging, &mut progress)
}

/// 兜底清理资产根下历史残留的 `.staging-*` 目录与 `.xberg-runtime-path.txt.*`
/// 临时文件（B-2），单项失败跳过继续。
///
/// `.staging-*` 清扫与截图 OCR 侧共用 [`crate::asset_util::
/// cleanup_stale_staging_dirs`]；本模块只保留自己的残留策略。
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
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with(&selection_temp_prefix) && path.is_file() {
            // 尽力而为：同 .staging-*，单项失败跳过。
            let _ = fs::remove_file(&path);
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
    ensure_not_cancelled(cancel)?;
    // 运行目录校验失败时如实报告：初始化不替用户修复用户指定的 Xberg 目录。
    let runtime = runtime_dir()?;
    validate_runtime_dir(&runtime)?;
    let notice_stage = staging.join("licenses");
    fs::create_dir_all(&notice_stage).map_err(|error| format!("创建许可证目录失败：{error}"))?;
    write_notice(&notice_stage.join("THIRD_PARTY_NOTICES.md"), manifest)?;
    atomic_replace_dir(&notice_stage, &root.join("licenses"))?;
    // XB-10：用户提供 Xberg 运行目录，初始化不再下载任何引擎或组件包
    //（下载器仅媒体运行库的 download_runtime 路径仍在使用）。
    progress("转 Markdown 组件初始化完成".to_string());
    Ok(())
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
        crate::snap_ocr_assets::download_asset(
            url,
            destination,
            expected_size,
            expected_sha256,
            cancel,
            &mut sink,
        )
    }
}

/// XB-20/XB-21：主动下载固定发布物；旧目录和配置在失败/取消时保持不变。
pub fn download_runtime(
    cancel: &AtomicBool,
    mut progress: impl FnMut(String),
) -> Result<PathBuf, String> {
    ensure_not_cancelled(cancel)?;
    progress("正在查询 Xberg 最新发布版本…".into());
    let release = crate::snap_ocr_assets::fetch_release_json(
        XBERG_LATEST_RELEASE_URL,
        cancel,
        &mut progress,
    )?;
    let mut manifest = load_manifest()?;
    apply_latest_release(&mut manifest.xberg, &release)?;
    progress(format!("最新发布版本：{}", manifest.xberg.tag));
    download_runtime_with(&manifest, cancel, &mut progress, &mut NetworkDownloader)
}

/// XB-10：只接受固定官方源的 Windows 发布包与上游 SHA-256，不回退钉死版本。
fn apply_latest_release(
    pack: &mut XbergManifest,
    release: &serde_json::Value,
) -> Result<(), String> {
    let tag = release["tag_name"]
        .as_str()
        .filter(|tag| valid_component_tag(tag))
        .ok_or_else(|| "最新 Xberg 发布缺少合法版本标识".to_string())?;
    if release["draft"] == true || release["prerelease"] == true {
        return Err("最新发布接口返回草稿或预发布版本，未开始下载".into());
    }
    let matches: Vec<_> = release["assets"]
        .as_array()
        .ok_or_else(|| "最新 Xberg 发布缺少资产列表".to_string())?
        .iter()
        .filter(|asset| asset["name"] == XBERG_ARCHIVE_NAME)
        .collect();
    if matches.len() != 1 {
        return Err("最新 Xberg 发布必须恰好包含一个 Windows x64 CLI 包".into());
    }
    let asset = matches[0];
    let url = format!(
        "https://github.com/jchanghong023/xberg/releases/download/{tag}/{XBERG_ARCHIVE_NAME}"
    );
    if asset["browser_download_url"].as_str() != Some(url.as_str()) {
        return Err("最新 Xberg 发布包来源与固定官方源不一致".into());
    }
    let size = asset["size"]
        .as_u64()
        .filter(|size| *size > 0)
        .ok_or_else(|| "最新 Xberg 发布包大小无效".to_string())?;
    let digest = asset["digest"]
        .as_str()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .filter(|digest| digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| "最新 Xberg 发布包缺少上游 SHA-256，未开始下载".to_string())?;
    pack.tag = tag.into();
    pack.archive_url = url;
    pack.archive_size_bytes = size;
    pack.archive_sha256 = digest.to_ascii_lowercase();
    // 归档按最新上游摘要校验；可变引擎/DLL/许可不使用旧发布成员摘要。
    // 既定模型身份仍保留，其他场景成员仍必须存在，不静默缩小功能。
    for member in &mut pack.members {
        if !member.path.starts_with("models/") {
            member.size_bytes = 0;
            member.sha256.clear();
        }
    }
    Ok(())
}

fn runtime_recipe(pack: &XbergManifest) -> Result<String, String> {
    let bytes = serde_json::to_vec(&pack.members).map_err(|error| error.to_string())?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn reusable_runtime(
    base: &Path,
    pack: &XbergManifest,
    cancel: &AtomicBool,
) -> Result<Option<PathBuf>, String> {
    let recipe = runtime_recipe(pack)?;
    let entries = match fs::read_dir(base) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("无法检查已下载版本：{error}")),
    };
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name();
        let Some(suffix) = name
            .to_str()
            .and_then(|name| name.strip_prefix(&format!("{}-", pack.tag)))
        else {
            continue;
        };
        if Uuid::parse_str(suffix).is_err() {
            continue;
        }
        let path = entry.path();
        let receipt = path.join(XBERG_RELEASE_RECEIPT);
        let Ok(bytes) = fs::read(&receipt) else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        if value["owner"] == "JchTools-xberg-download-v1"
            && value["tag"] == pack.tag
            && value["archive_sha256"] == pack.archive_sha256
            && value["archive_size_bytes"] == pack.archive_size_bytes
            && value["members_recipe"] == recipe
            && pack
                .members
                .iter()
                .all(|member| path.join(&member.path).is_file())
        {
            // 仅用户主动获取最新版时核验自有缓存身份；正常运行和用户自供
            // 目录仍按 XB-09 做存在性检查，不在启动/转换/截图中计算摘要。
            let Some(expected) = value["engine_sha256"].as_str() else {
                continue;
            };
            ensure_not_cancelled(cancel)?;
            let Ok(actual) =
                crate::asset_util::sha256_file_inner(&path.join("xberg.exe"), Some(cancel))
            else {
                ensure_not_cancelled(cancel)?;
                continue;
            };
            if actual == expected {
                candidates.push(path);
            }
        }
    }
    candidates.sort();
    Ok(candidates.into_iter().next())
}

fn download_runtime_with(
    manifest: &AssetManifest,
    cancel: &AtomicBool,
    progress: &mut impl FnMut(String),
    downloader: &mut dyn AssetDownloader,
) -> Result<PathBuf, String> {
    // P-10：可选组件下载是关键功能任务，开始/结束统计必须落盘（单次下载
    // 内部的重试与代理回退日志由下载原语记录）。
    tracing::info!(
        tag = %manifest.xberg.tag,
        size_bytes = manifest.xberg.archive_size_bytes,
        "Xberg 运行时下载任务开始"
    );
    let started = std::time::Instant::now();
    let result = download_runtime_task(manifest, cancel, progress, downloader);
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    match &result {
        Ok(installed) => tracing::info!(
            elapsed_ms,
            installed = %installed.display(),
            "Xberg 运行时下载任务完成"
        ),
        Err(reason) if reason.contains("取消") => {
            tracing::info!(elapsed_ms, reason = %reason, "Xberg 运行时下载任务已取消");
        }
        Err(reason) => tracing::error!(
            elapsed_ms,
            reason = %reason,
            "Xberg 运行时下载任务失败"
        ),
    }
    result
}

fn download_runtime_task(
    manifest: &AssetManifest,
    cancel: &AtomicBool,
    progress: &mut impl FnMut(String),
    downloader: &mut dyn AssetDownloader,
) -> Result<PathBuf, String> {
    ensure_not_cancelled(cancel)?;
    let base = crate::xberg_settings::state_dir()?.join("xberg-downloads");
    fs::create_dir_all(&base).map_err(|e| e.to_string())?;
    cleanup_owned_download_staging(&base, progress);
    if let Some(installed) = reusable_runtime(&base, &manifest.xberg, cancel)? {
        ensure_not_cancelled(cancel)?;
        ensure_document_notice()?;
        ensure_not_cancelled(cancel)?;
        crate::xberg_settings::save_source(crate::xberg_settings::Source::Downloaded, &installed)?;
        progress(format!(
            "已存在 {}，复用已校验安装，不重复下载",
            manifest.xberg.tag
        ));
        return Ok(installed);
    }
    let staging = base.join(format!(".staging-{}", Uuid::new_v4()));
    fs::create_dir(&staging).map_err(|e| e.to_string())?;
    let mut staging_lock = match open_download_staging_lock(&staging) {
        Ok(file) => file,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
    };
    if let Err(error) = staging_lock.write_all(b"JchTools Xberg download staging\n") {
        drop(staging_lock);
        let _ = fs::remove_dir_all(&staging);
        return Err(format!("创建下载临时目录所有权标记失败：{error}"));
    }
    if let Err(error) = staging_lock.sync_all() {
        drop(staging_lock);
        let _ = fs::remove_dir_all(&staging);
        return Err(format!("同步下载临时目录所有权标记失败：{error}"));
    }
    let result = (|| {
        let archive = staging.join("runtime.zip");
        let pack = &manifest.xberg;
        downloader.download(
            &pack.archive_url,
            &archive,
            pack.archive_size_bytes,
            &pack.archive_sha256,
            cancel,
            progress,
        )?;
        ensure_not_cancelled(cancel)?;
        verify_file_with_cancel(
            &archive,
            pack.archive_size_bytes,
            &pack.archive_sha256,
            cancel,
        )?;
        let extracted = staging.join("unpacked");
        fs::create_dir(&extracted).map_err(|e| e.to_string())?;
        crate::asset_util::extract_zip_safely(&archive, &extracted, cancel)?;
        let component = extracted.join("xberg-cli-x86_64-pc-windows-msvc");
        for member in &pack.members {
            ensure_not_cancelled(cancel)?;
            let path = component.join(&member.path);
            if member.sha256.is_empty() {
                if !path.is_file() {
                    return Err(format!("最新 Xberg 发布缺少所需成员：{}", member.path));
                }
            } else {
                verify_file_with_cancel(&path, member.size_bytes, &member.sha256, cancel)?;
            }
        }
        let engine_sha256 = if let Some(member) = pack
            .members
            .iter()
            .find(|member| member.path == "xberg.exe" && !member.sha256.is_empty())
        {
            // 此成员已在上面的循环验证，不重复读取相同文件。
            member.sha256.clone()
        } else {
            crate::asset_util::sha256_file_inner(&component.join("xberg.exe"), Some(cancel))
                .map_err(|error| error.to_string())?
        };
        let receipt = serde_json::json!({
            "owner":"JchTools-xberg-download-v1",
            "tag":pack.tag,
            "archive_sha256":pack.archive_sha256,
            "archive_size_bytes":pack.archive_size_bytes,
            "members_recipe":runtime_recipe(pack)?,
            "engine_sha256":engine_sha256,
        });
        let mut receipt_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(component.join(XBERG_RELEASE_RECEIPT))
            .map_err(|error| format!("写入已校验发布记录失败：{error}"))?;
        receipt_file
            .write_all(&serde_json::to_vec(&receipt).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
        receipt_file.sync_all().map_err(|error| error.to_string())?;
        drop(receipt_file);
        // 上述清单含全部场景，逐成员核对，不按当前工具过滤模型。
        ensure_not_cancelled(cancel)?;
        let installed = base.join(format!("{}-{}", pack.tag, Uuid::new_v4()));
        // 提交段（rename → notice → save_source）取消检查点：紧贴 rename 再查
        // 一次，把「检查通过后用户才取消」的窗口收窄到提交动作本身；取消被
        // 观测到时不进入提交（T-05：可重试，staging 随后整体清理）。
        ensure_not_cancelled(cancel)?;
        fs::rename(&component, &installed).map_err(|e| format!("安装 Xberg 失败：{e}"))?;
        // 下载安装成功即确保许可证 notice（T-06，S3-01 集成后与保存共用
        // [`ensure_document_notice`] 原子闭环）：失败按未完成安装处理——
        // 撤销目录且不写 SQLite（与 save_source 失败同一回滚口径）。
        if let Err(error) = ensure_document_notice() {
            let _ = fs::remove_dir_all(&installed);
            return Err(error);
        }
        // save_source 前最后一次取消检查（XB-20：下载取消不覆盖有效配置）——
        // 取消在 SQLite 写入前被观测到时不切换来源；已 rename 落位的目录按
        // T-05「保留已校验资产」保留，不做删除回滚（残留目录不阻塞任何功能，
        // 下次下载安装到新目录）。
        ensure_not_cancelled(cancel)?;
        if let Err(error) = crate::xberg_settings::save_source(
            crate::xberg_settings::Source::Downloaded,
            &installed,
        ) {
            // 仅撤销本次创建且尚未成为有效配置的目录。
            let _ = fs::remove_dir_all(&installed);
            return Err(error);
        }
        Ok(installed)
    })();
    // 校验与安装已经结束；先释放禁止删除共享的 Windows marker 句柄。
    drop(staging_lock);
    let cleanup = fs::remove_dir_all(&staging);
    if let Err(error) = cleanup {
        progress(format!("下载临时目录清理失败：{error}"));
    }
    result
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
    text.push_str("Xberg 运行目录由用户指定；主程序安装包不包含这些资产。媒体转录的模型与推理运行库由 Xberg 推理组件提供，许可随组件树自带。\n\n");
    for license in &manifest.xberg.licenses {
        let _ = writeln!(
            &mut text,
            "- {}：{}，{}",
            license.component, license.license, license.source
        );
    }
    fs::write(path, text).map_err(|error| format!("写入许可证 notice 失败：{error}"))
}

#[cfg(test)]
mod tests {
    use super::{load_manifest, media_component_dir, write_notice};
    use crate::asset_util::{
        inference_ready, install_inference_pack, resolve_component_with_tag, restore_backup,
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
    /// snap_ocr_assets 的测试共用（见 asset_util::test_env），避免两模块的
    /// 覆盖变量在并行线程中相互覆盖。
    fn env_lock() -> &'static Mutex<()> {
        crate::asset_util::test_env::env_lock()
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

    // 覆盖 XB-20/XB-21：合成发布包经过真实下载安装核心；成员错误及取消保留原配置。
    fn release_metadata(tag: &str) -> serde_json::Value {
        serde_json::json!({
            "tag_name":tag,
            "draft":false,
            "prerelease":false,
            "assets":[{
                "name":super::XBERG_ARCHIVE_NAME,
                "browser_download_url":format!(
                    "https://github.com/jchanghong023/xberg/releases/download/{tag}/{}",
                    super::XBERG_ARCHIVE_NAME
                ),
                "size":12345,
                "digest":format!("sha256:{}", "a".repeat(64))
            }]
        })
    }

    // 覆盖 XB-10/O-07：采用实际最新发布元数据，不能仍钉死内置 tag 或引擎摘要。
    #[test]
    fn latest_metadata_replaces_pinned_engine_but_preserves_model_identity() {
        let mut pack = super::load_manifest().unwrap().xberg;
        let model = pack
            .members
            .iter()
            .find(|member| member.path.starts_with("models/"))
            .unwrap();
        let model_path = model.path.clone();
        let model_hash = model.sha256.clone();
        super::apply_latest_release(&mut pack, &release_metadata("v2099.1.1")).unwrap();
        assert_eq!(pack.tag, "v2099.1.1");
        assert_eq!(pack.archive_size_bytes, 12345);
        assert_eq!(pack.archive_sha256, "a".repeat(64));
        assert!(pack
            .members
            .iter()
            .find(|member| member.path == "xberg.exe")
            .unwrap()
            .sha256
            .is_empty());
        assert_eq!(
            pack.members
                .iter()
                .find(|member| member.path == model_path)
                .unwrap()
                .sha256,
            model_hash
        );
    }

    // 覆盖 XB-10：没有上游摘要不得下载，也不得静默改用旧版本。
    #[test]
    fn latest_metadata_without_digest_fails_without_mutating_pinned_pack() {
        let mut pack = super::load_manifest().unwrap().xberg;
        let old_tag = pack.tag.clone();
        let mut release = release_metadata("v2099.1.1");
        release["assets"][0]["digest"] = serde_json::Value::Null;
        assert!(super::apply_latest_release(&mut pack, &release)
            .unwrap_err()
            .contains("SHA-256"));
        assert_eq!(pack.tag, old_tag);
    }

    // 覆盖 P-03/XB-10：元数据不能扩大固定来源，歧义包也不得猜测。
    #[test]
    fn latest_metadata_rejects_changed_source_and_duplicate_archives() {
        let mut pack = super::load_manifest().unwrap().xberg;
        let mut release = release_metadata("v2099.1.1");
        release["assets"][0]["browser_download_url"] =
            serde_json::json!("https://example.invalid/engine.zip");
        assert!(super::apply_latest_release(&mut pack, &release).is_err());
        let mut release = release_metadata("v2099.1.1");
        let duplicate = release["assets"][0].clone();
        release["assets"].as_array_mut().unwrap().push(duplicate);
        assert!(super::apply_latest_release(&mut pack, &release).is_err());
    }

    // 覆盖 XB-10：同一已校验最新版再次选择下载，应复用而非再次下载与安装。
    #[test]
    fn latest_runtime_is_reused_without_repeating_archive_download() {
        use super::{AssetFile, AssetManifest, XbergManifest};
        use sha2::{Digest, Sha256};
        use std::io::{Cursor, Write};
        let _guard = redirect_component_env();
        let payload = b"synthetic engine";
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        zip.start_file(
            "xberg-cli-x86_64-pc-windows-msvc/xberg.exe",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(payload).unwrap();
        let archive = zip.finish().unwrap().into_inner();
        struct Download {
            archive: Vec<u8>,
            calls: usize,
        }
        impl AssetDownloader for Download {
            fn download(
                &mut self,
                _: &str,
                destination: &Path,
                _: u64,
                _: &str,
                _: &AtomicBool,
                _: &mut dyn FnMut(String),
            ) -> Result<(), String> {
                self.calls += 1;
                fs::write(destination, &self.archive).map_err(|error| error.to_string())
            }
        }
        let manifest = AssetManifest {
            schema_version: 1,
            xberg_inference: None,
            xberg: XbergManifest {
                tag: "latest-reuse-test".into(),
                archive_url: "https://example.invalid/latest.zip".into(),
                archive_size_bytes: u64::try_from(archive.len()).unwrap(),
                archive_sha256: format!("{:x}", Sha256::digest(&archive)),
                members: vec![AssetFile {
                    path: "xberg.exe".into(),
                    size_bytes: u64::try_from(payload.len()).unwrap(),
                    sha256: format!("{:x}", Sha256::digest(payload)),
                }],
                licenses: Vec::new(),
            },
        };
        let mut downloader = Download { archive, calls: 0 };
        let cancel = AtomicBool::new(false);
        let first =
            super::download_runtime_with(&manifest, &cancel, &mut |_| {}, &mut downloader).unwrap();
        let second =
            super::download_runtime_with(&manifest, &cancel, &mut |_| {}, &mut downloader).unwrap();
        assert_eq!(first, second, "已安装的最新版必须复用同一目录");
        assert_eq!(downloader.calls, 1, "已有最新版不得重复下载");
        // XB-10：主动再次获取最新版时，旧标记不能证明已被手工替换的引擎仍是该发布。
        // 正常运行及用户自供目录仍按 XB-09 只检查存在性，不受此下载身份检查影响。
        fs::write(first.join("xberg.exe"), b"manually replaced older engine").unwrap();
        let repaired =
            super::download_runtime_with(&manifest, &cancel, &mut |_| {}, &mut downloader).unwrap();
        assert_ne!(repaired, first, "被替换的缓存不能冒充可复用的最新版");
        assert_eq!(fs::read(repaired.join("xberg.exe")).unwrap(), payload);
        assert_eq!(downloader.calls, 2, "缓存身份失效时必须重新获取该次最新版");
        assert_eq!(
            fs::read(first.join("xberg.exe")).unwrap(),
            b"manually replaced older engine",
            "重新获取不得覆盖旧缓存中用户改写的文件"
        );
    }

    #[test]
    fn installed_runtime_is_verified_before_selection() {
        use super::{AssetFile, AssetManifest, XbergManifest};
        use std::io::{Cursor, Write};
        let guard = redirect_component_env();
        let custom = guard.root.path().join("custom");
        fs::create_dir(&custom).unwrap();
        fs::write(custom.join("xberg.exe"), b"existing").unwrap();
        crate::xberg_settings::save(&custom).unwrap();
        let user_staging = crate::xberg_settings::state_dir()
            .unwrap()
            .join("xberg-downloads")
            .join(".staging-user");
        fs::create_dir_all(&user_staging).unwrap();
        fs::write(user_staging.join("user.txt"), b"keep").unwrap();
        let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let mut members = Vec::new();
        for (name, bytes) in [
            ("xberg.exe", b"engine".as_slice()),
            ("models/snapshot-ocr/det.onnx", b"model".as_slice()),
        ] {
            archive
                .start_file(
                    format!("xberg-cli-x86_64-pc-windows-msvc/{name}"),
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            archive.write_all(bytes).unwrap();
            members.push(AssetFile {
                path: name.into(),
                size_bytes: bytes.len() as u64,
                sha256: format!("{:x}", Sha256::digest(bytes)),
            });
        }
        let bytes = archive.finish().unwrap().into_inner();
        struct Download(Vec<u8>);
        impl AssetDownloader for Download {
            fn download(
                &mut self,
                _: &str,
                destination: &Path,
                _: u64,
                _: &str,
                _: &AtomicBool,
                _: &mut dyn FnMut(String),
            ) -> Result<(), String> {
                fs::write(destination, &self.0).map_err(|e| e.to_string())
            }
        }
        let mut manifest = AssetManifest {
            schema_version: 1,
            xberg_inference: None,
            xberg: XbergManifest {
                tag: "test".into(),
                archive_url: "https://example.invalid/synthetic.zip".into(),
                archive_size_bytes: bytes.len() as u64,
                archive_sha256: format!("{:x}", Sha256::digest(&bytes)),
                members,
                licenses: Vec::new(),
            },
        };
        let mut downloader = Download(bytes);
        let cancel = AtomicBool::new(false);
        let installed =
            super::download_runtime_with(&manifest, &cancel, &mut |_| {}, &mut downloader).unwrap();
        assert_eq!(
            fs::read(installed.join("models/snapshot-ocr/det.onnx")).unwrap(),
            b"model"
        );
        assert_eq!(
            crate::xberg_settings::required().unwrap(),
            installed.canonicalize().unwrap()
        );
        assert_eq!(
            crate::xberg_settings::settings().unwrap().custom.unwrap(),
            custom.canonicalize().unwrap()
        );
        manifest.xberg.members[1].sha256 = "0".repeat(64);
        assert!(
            super::download_runtime_with(&manifest, &cancel, &mut |_| {}, &mut downloader).is_err()
        );
        assert_eq!(
            crate::xberg_settings::required().unwrap(),
            installed.canonicalize().unwrap()
        );
        assert!(super::download_runtime_with(
            &manifest,
            &AtomicBool::new(true),
            &mut |_| {},
            &mut downloader
        )
        .is_err());
        assert_eq!(fs::read(custom.join("xberg.exe")).unwrap(), b"existing");
        assert!(
            user_staging.exists(),
            "无 ownership marker 的用户 staging 必须保留"
        );
        let remaining_staging: Vec<String> = fs::read_dir(installed.parent().unwrap())
            .unwrap()
            .flatten()
            .filter_map(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .map(str::to_string)
                    .filter(|name| name.starts_with(".staging-"))
            })
            .collect();
        assert_eq!(
            remaining_staging,
            vec![".staging-user".to_string()],
            "本轮带 ownership marker 的 staging 必须清除，用户目录必须保留"
        );
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
        crate::xberg_settings::save(&external).expect("经设置链保存本地组件目录");
        let resolved = media_component_dir().expect_err("外部树缺模型时必须指出缺失项");
        assert!(resolved.contains("model.int8.onnx"), "{resolved}");
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

    // 覆盖 T-05/XB-20：取消在任何提交动作（rename/notice/save_source）之前的
    // 检查点被观测到时，任务必须以取消错误结束——SQLite 不写 downloaded、
    // 不留已安装目录、staging 清理（取消不覆盖有效配置）。
    #[test]
    fn download_cancel_before_commit_keeps_config_untouched() {
        use super::{AssetFile, AssetManifest, XbergManifest};
        use std::sync::atomic::Ordering;
        let _guard = redirect_component_env();
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer
            .start_file(
                "xberg-cli-x86_64-pc-windows-msvc/xberg.exe",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        use std::io::Write as _;
        writer.write_all(b"engine").unwrap();
        let bytes = writer.finish().unwrap().into_inner();
        // 下载完成后置取消：任务必须在进入提交段前停止。
        struct CancelAtEnd(Vec<u8>);
        impl AssetDownloader for CancelAtEnd {
            fn download(
                &mut self,
                _: &str,
                destination: &Path,
                _: u64,
                _: &str,
                cancel: &AtomicBool,
                _: &mut dyn FnMut(String),
            ) -> Result<(), String> {
                fs::write(destination, &self.0).map_err(|e| e.to_string())?;
                cancel.store(true, Ordering::Release);
                Ok(())
            }
        }
        let manifest = AssetManifest {
            schema_version: 1,
            xberg_inference: None,
            xberg: XbergManifest {
                tag: "vcancel-test".into(),
                archive_url: "https://example.invalid/synthetic.zip".into(),
                archive_size_bytes: bytes.len() as u64,
                archive_sha256: sha256_bytes(&bytes),
                members: vec![AssetFile {
                    path: "xberg.exe".into(),
                    size_bytes: 6,
                    sha256: sha256_bytes(b"engine"),
                }],
                licenses: Vec::new(),
            },
        };
        let cancel = AtomicBool::new(false);
        let error =
            super::download_runtime_with(&manifest, &cancel, &mut |_| {}, &mut CancelAtEnd(bytes))
                .expect_err("下载尾置取消必须在提交前停止");
        assert!(error.contains("取消"), "错误必须是取消语义：{error}");
        assert!(
            crate::xberg_settings::settings()
                .unwrap()
                .downloaded
                .is_none(),
            "取消后不得写 downloaded 来源"
        );
        let base = crate::xberg_settings::state_dir()
            .unwrap()
            .join("xberg-downloads");
        let leftovers: Vec<String> = fs::read_dir(&base)
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            leftovers.is_empty(),
            "取消后不得残留 staging 或已安装目录：{leftovers:?}"
        );
    }

    // 覆盖 T-05/T-06（回归：设置页下载安装成功后必须直接满足文档场景
    // readiness——修复前 notice 唯一写点在 initialize_staged，下载完成后
    // readiness 仍因「许可证 notice 不存在」阻断转换，用户还得再点一次初始化）。
    #[test]
    fn download_runtime_write_notice_closes_readiness_loop() {
        use super::{AssetFile, AssetManifest, XbergManifest};
        use std::io::Write as _;
        let guard = redirect_component_env();
        // 合成发布包：覆盖真实清单 xberg 段全部成员（字节为桩，清单值按桩字节
        // 计算），使安装后 validate_runtime_dir 的 document 场景存在性检查通过；
        // 断言核心是 readiness 的 notice 检查不再失败。
        let real = load_manifest().expect("内置清单必须可解析");
        let mut archive = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let mut members = Vec::new();
        for member in &real.xberg.members {
            let bytes = format!("stub-{}", member.path).into_bytes();
            archive
                .start_file(
                    format!("xberg-cli-x86_64-pc-windows-msvc/{}", member.path),
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            archive.write_all(&bytes).unwrap();
            members.push(AssetFile {
                path: member.path.clone(),
                size_bytes: bytes.len() as u64,
                sha256: sha256_bytes(&bytes),
            });
        }
        let bytes = archive.finish().unwrap().into_inner();
        struct Download(Vec<u8>);
        impl AssetDownloader for Download {
            fn download(
                &mut self,
                _: &str,
                destination: &Path,
                _: u64,
                _: &str,
                _: &AtomicBool,
                _: &mut dyn FnMut(String),
            ) -> Result<(), String> {
                fs::write(destination, &self.0).map_err(|e| e.to_string())
            }
        }
        let manifest = AssetManifest {
            schema_version: 1,
            xberg_inference: None,
            xberg: XbergManifest {
                tag: "vdownload-notice-test".into(),
                archive_url: "https://example.invalid/synthetic.zip".into(),
                archive_size_bytes: bytes.len() as u64,
                archive_sha256: sha256_bytes(&bytes),
                members,
                licenses: Vec::new(),
            },
        };
        let cancel = AtomicBool::new(false);
        let installed =
            super::download_runtime_with(&manifest, &cancel, &mut |_| {}, &mut Download(bytes))
                .expect("合成下载安装应成功");
        // 媒体场景不受影响：无推理段时在位校验照常。
        assert!(super::validate_media().is_ok());
        // 断言链：notice 存在且文档场景 readiness 通过（修复前在此失败）。
        let notice = guard
            .root
            .path()
            .join("licenses")
            .join("THIRD_PARTY_NOTICES.md");
        assert!(
            notice.is_file(),
            "下载安装成功后必须写入 notice：{}",
            notice.display()
        );
        assert!(
            super::readiness().is_ok(),
            "下载完成即应满足文档场景 readiness：{:?}",
            super::readiness()
        );
        assert!(installed.is_dir());
    }

    // 覆盖 XB-19：保存共享目录时不强制其他场景的模型就绪；每个功能仅在自身
    // 启动前按场景检查。即使目录缺少截图和媒体模型，文档所需成员齐全也可保存。
    #[test]
    fn save_runtime_dir_allows_other_scenarios_to_be_unready() {
        let guard = redirect_component_env();
        let manifest = load_manifest().expect("内置清单必须可解析");
        let engine = tempfile::tempdir().expect("创建自选引擎目录");
        for member in &manifest.xberg.members {
            if !crate::xberg_runtime::asset_for_scenario(&member.path, "document") {
                continue;
            }
            let path = engine.path().join(&member.path);
            fs::create_dir_all(path.parent().expect("成员路径有父目录")).expect("创建成员父目录");
            fs::write(&path, format!("stub-{}", member.path)).expect("写入成员桩文件");
        }

        crate::xberg_runtime::validate_assets(engine.path(), "document").expect("文档场景成员齐全");
        assert!(
            crate::xberg_runtime::validate_assets(engine.path(), "snapshot").is_err(),
            "缺截图模型时截图场景应仍未就绪"
        );
        assert!(
            crate::xberg_runtime::validate_assets(engine.path(), "media").is_err(),
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
        // 后续阶段会因未选择 Xberg 运行目录而失败，属预期；本用例只断言入口清理。
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
            if !crate::xberg_runtime::asset_for_scenario(&member.path, "document") {
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
        // 后续阶段会因未选择 Xberg 运行目录而失败，属预期；本用例只断言入口清理。
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
            if !crate::xberg_runtime::asset_for_scenario(&member.install_path, "media") {
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

        fs::remove_file(external.join("models").join("vad").join("silero_vad.onnx"))
            .expect("删除一个媒体模型");
        let error = super::readiness_inference_pack(&manifest, guard.root.path())
            .expect_err("缺失成员必须失败");
        assert!(
            error.contains("silero_vad.onnx") && error.contains("缺失"),
            "错误应基于保存目录点名缺失成员：{error}"
        );
    }

    // 覆盖 T-06（2026-10-02 随 XB-09 修订）：用户指定的 Xberg 运行目录只做
    // document 场景成员存在性检查——文件被替换为不同字节（用户自行更新引擎
    // 版本）必须放行；成员缺失时明确指出缺失项，不执行不完整环境。
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
