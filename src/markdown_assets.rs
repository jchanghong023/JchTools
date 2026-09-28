//! 转 Markdown 的本地资产状态与校验。
//!
//! 文档转换继续校验用户选择的 Xberg 运行目录（固定版本清单编译进主程序）。
//! 媒体转录（T-19）的模型与推理由 Xberg 推理组件提供：清单接入 `xberg_inference`
//! 条目后由初始化流程下载、按归档与成员 SHA-256 校验并安装到
//! `xberg-inference/<tag>/`（XB-09/XB-10）；条目未接入时缺失组件如实报告未配置。
//! 二进制和模型始终不进主程序包。staging/校验/下载/原子落位与推理组件包安装
//! 核心与截图 OCR 共用 [`crate::asset_util`]，两侧行为同源。

use crate::asset_util::{
    atomic_replace_dir, atomic_replace_file, ensure_not_cancelled, inference_ready,
    install_inference_pack, resolve_component_with_tag, validate_relative_path, verify_file,
    AssetDownloader, InferenceManifest,
};
use serde::Deserialize;
use std::fmt::Write as FmtWrite;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use uuid::Uuid;

const MANIFEST: &str = include_str!("../resources/markdown-assets.json");
const DATA_DIRECTORY: &str = "markdown-assets";
const XBERG_TAG: &str = "v2026.9.29-0212-run49.1";
const RUNTIME_SELECTION_FILE: &str = "xberg-runtime-path.txt";

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

/// Xberg 推理组件安装根：`<资产根>/xberg-inference/`（每个发布版本一个 tag
/// 子目录）。与截图 OCR 各自独立安装（O-03：互不覆盖，只读复用同一发布源）。
#[must_use]
pub fn xberg_inference_root() -> PathBuf {
    asset_root().join("xberg-inference")
}

/// 组件目录解析：开发期（仅 debug 构建）可用 `JCHTOOLS_XBERG_INFERENCE_DIR`
/// 覆盖到本地组件树；否则取安装根下的 tag 子目录。清单已接入推理组件包时，
/// 目录名必须与清单 tag 一致（XB-09：不混用其他版本、不因同名文件认定兼容），
/// 不一致明确报错并指引更新；清单未接入时沿用「唯一子目录」启发式。
/// 与 `snap_ocr_assets` 的同名解析规则保持一致（同一安装只能有一个版本）。
fn resolve_xberg_component(root: &Path) -> Result<PathBuf, String> {
    if cfg!(debug_assertions) {
        if let Some(path) = std::env::var_os("JCHTOOLS_XBERG_INFERENCE_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
        {
            return Ok(path);
        }
    }
    let expected_tag = load_manifest()
        .ok()
        .and_then(|manifest| manifest.xberg_inference.map(|inference| inference.tag));
    resolve_component_with_tag(
        &root.join("xberg-inference"),
        expected_tag.as_deref(),
        &xberg_media_not_configured(),
    )
}

fn xberg_media_not_configured() -> String {
    match load_manifest()
        .ok()
        .and_then(|manifest| manifest.xberg_inference)
    {
        Some(_) => "Xberg 推理组件未配置：媒体转录所需的 xberg.exe、SenseVoice/VAD 模型与 \
     FFmpeg/sherpa-onnx 运行库尚未安装；请在转 Markdown 页重新初始化以下载推理组件包；\
     开发期可设置 JCHTOOLS_XBERG_INFERENCE_DIR 指向本地组件目录"
            .into(),
        None => "Xberg 推理组件未配置：媒体转录所需的 xberg.exe、SenseVoice/VAD 模型与 \
     FFmpeg/sherpa-onnx 运行库尚未安装；其下载清单条目待 Xberg 发布 tag 落定后接入，\
     开发期可设置 JCHTOOLS_XBERG_INFERENCE_DIR 指向本地组件目录"
            .into(),
    }
}

/// 媒体转录组件的在位校验（存在性；摘要级清单待 Xberg 发布 tag 落定后接入，
/// 与截图 OCR 侧口径一致）：`xberg.exe` + SenseVoice/VAD 模型 + sherpa-onnx
/// 四 DLL + FFmpeg 四 DLL。返回解析出的组件目录供转录进程注入环境变量。
pub fn media_component_dir() -> Result<PathBuf, String> {
    let component = resolve_xberg_component(&asset_root())?;
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
    for path in &required {
        if !path.is_file() {
            let relative = path.strip_prefix(&component).unwrap_or(path);
            return Err(format!(
                "推理组件不完整：缺少 {}（组件目录 {}）",
                relative.display(),
                component.display()
            ));
        }
    }
    Ok(component)
}

/// 只读检查所有已安装资产。该函数不会联网、创建目录或修改文件。
pub fn readiness() -> Result<(), String> {
    // 先校验内置清单本身：失效清单不得被当作可运行环境。
    let manifest = load_manifest()?;
    let runtime = runtime_dir()?;
    validate_runtime_dir(&runtime)?;
    let notice = asset_root().join("licenses").join("THIRD_PARTY_NOTICES.md");
    if !notice.is_file() {
        return Err(format!("许可证 notice 不存在：{}", notice.display()));
    }
    media_component_dir()?;
    // 清单接入推理组件包后做成员级摘要校验（XB-09；未接入时在位校验已覆盖）。
    if let Some(inference) = &manifest.xberg_inference {
        inference_ready(inference, &asset_root())?;
    }
    Ok(())
}

/// 校验运行目录、写入许可证 notice，并按清单下载安装推理组件包（XB-10）。
///
/// 清单未接入推理组件包时不下载任何资产，组件缺失时返回明确错误并指引
/// （不冒称就绪）。取消会删除本轮 staging 目录。
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
    fs::create_dir_all(&staging).map_err(|error| format!("创建初始化临时目录失败：{error}"))?;
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
    ensure_not_cancelled(cancel)?;
    // 运行目录校验失败时如实报告：初始化不替用户修复用户指定的 Xberg 目录。
    let runtime = runtime_dir()?;
    validate_runtime_dir(&runtime)?;
    let notice_stage = staging.join("licenses");
    fs::create_dir_all(&notice_stage).map_err(|error| format!("创建许可证目录失败：{error}"))?;
    write_notice(&notice_stage.join("THIRD_PARTY_NOTICES.md"), manifest)?;
    atomic_replace_dir(&notice_stage, &root.join("licenses"))?;
    // 推理组件包（XB-10）：清单接入时下载并按归档/成员 SHA-256 校验后安装；
    // 未接入时保持只做在位校验（缺失即失败，不冒称就绪）。
    if let Some(inference) = &manifest.xberg_inference {
        install_inference_pack(inference, staging, root, cancel, downloader, progress)?;
    }
    media_component_dir()?;
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
    for member in &manifest.xberg.members {
        validate_relative_path(&member.path)?;
    }
    if let Some(inference) = &manifest.xberg_inference {
        let tag_ok = !inference.tag.is_empty()
            && !inference.tag.contains('/')
            && !inference.tag.contains('\\')
            && !inference.tag.contains("..")
            && !inference.tag.contains(':');
        if !tag_ok
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
    }
    Ok(manifest)
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
    use std::sync::{Mutex, MutexGuard, OnceLock};

    // 覆盖 T-06：内置清单必须可解析（媒体段退役后仅存 xberg 段）。
    #[test]
    fn manifest_parses_with_xberg_segment_only() {
        let manifest = load_manifest().expect("内置资产清单必须可解析");
        assert!(
            !manifest.xberg.members.is_empty(),
            "xberg 固定版本成员清单不得为空"
        );
    }

    /// 测试共享进程环境变量，组件根相关用例必须串行访问。
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
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
        std::env::remove_var("JCHTOOLS_XBERG_INFERENCE_DIR");
        ComponentGuard { root, _lock: lock }
    }

    impl Drop for ComponentGuard {
        fn drop(&mut self) {
            std::env::remove_var("JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT");
            std::env::remove_var("JCHTOOLS_XBERG_INFERENCE_DIR");
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
        component
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
        assert!(
            error.contains("JCHTOOLS_XBERG_INFERENCE_DIR"),
            "错误应指引环境变量：{error}"
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

    // 覆盖 T-05：开发期环境变量可指向本地组件树（与截图 OCR 同一变量）。
    #[test]
    fn media_component_env_override_points_at_local_tree() {
        let guard = redirect_component_env();
        let external = guard.root.path().join("external-component");
        let files = ["xberg.exe"];
        fs::create_dir_all(&external).expect("创建外部组件目录");
        for file in files {
            fs::write(external.join(file), b"x").expect("预置外部组件文件");
        }
        std::env::set_var("JCHTOOLS_XBERG_INFERENCE_DIR", &external);
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

    // 覆盖 T-06：notice 只携带 xberg 段的许可条目（媒体组件许可随组件树自带）。
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
}
