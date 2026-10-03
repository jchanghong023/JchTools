//! 转 Markdown 的本地资产校验。Xberg 目录由应用级 SQLite 统一提供。
//! 文档与媒体分别校验自己的模型和运行库；设置页可主动下载固定发布物，
//! 校验完整后保存为共享下载来源。文档初始化仍只写许可证 notice。

use crate::asset_util::{
    atomic_replace_dir, cleanup_stale_staging_dirs, ensure_not_cancelled, finalize_staging,
    require_component_members, require_inference_members_for_scenario, resolve_xberg_component,
    state_dir_asset_root, valid_component_tag, validate_relative_path, verify_file,
    AssetDownloader, InferenceManifest,
};
use serde::Deserialize;
use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use uuid::Uuid;

const MANIFEST: &str = include_str!("../resources/markdown-assets.json");
const DATA_DIRECTORY: &str = "markdown-assets";
pub(crate) const XBERG_TAG: &str = "v2026.10.2-0920-run54.1";
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
pub fn save_runtime_dir(path: &Path) -> Result<(), String> {
    crate::xberg_runtime::validate_assets(path, "engine")?;
    crate::xberg_settings::save(path)
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
/// 成员检查基于共享解析规则（C-2，与截图 OCR 侧同口径）：解析支持 debug 构建
/// 的 `JCHTOOLS_XBERG_INFERENCE_DIR` 覆盖，成员检查必须与在位校验
/// （[`media_component_dir`]）使用同一目录——此前成员校验直接按资产根拼
/// `xberg-inference/<tag>/`，绕过覆盖，导致覆盖路径在位校验通过后仍必报成员
/// 校验失败、永远无法就绪（markdown::run 阻断转换，initialize 还会重复下载
/// 约 291MB 组件包）。markdown 清单成员的 install_path 本就是组件目录相对
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
    if readiness().is_ok() {
        progress("转 Markdown 组件已就绪".to_string());
        return Ok(());
    }
    ensure_not_cancelled(cancel)?;
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
    download_runtime_with(
        &load_manifest()?,
        cancel,
        &mut progress,
        &mut NetworkDownloader,
    )
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
    let staging = base.join(format!(".staging-{}", Uuid::new_v4()));
    fs::create_dir(&staging).map_err(|e| e.to_string())?;
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
        verify_file(&archive, pack.archive_size_bytes, &pack.archive_sha256)?;
        let extracted = staging.join("unpacked");
        fs::create_dir(&extracted).map_err(|e| e.to_string())?;
        crate::asset_util::extract_zip_safely(&archive, &extracted, cancel)?;
        let component = extracted.join("xberg-cli-x86_64-pc-windows-msvc");
        for member in &pack.members {
            ensure_not_cancelled(cancel)?;
            verify_file(
                &component.join(&member.path),
                member.size_bytes,
                &member.sha256,
            )?;
        }
        // 上述清单含全部场景，逐成员核对，不按当前工具过滤模型。
        ensure_not_cancelled(cancel)?;
        let installed = base.join(format!("{}-{}", pack.tag, Uuid::new_v4()));
        fs::rename(&component, &installed).map_err(|e| format!("安装 Xberg 失败：{e}"))?;
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
    let cleanup = fs::remove_dir_all(&staging);
    if let Err(error) = cleanup {
        progress(format!("下载临时目录清理失败：{error}"));
    }
    result
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

    // 覆盖 XB-19/T-05（回归：任务预检按所选分组分场景——纯媒体目录缺文档模型
    // 不再被文档条件提前拒绝；缺文档模型的文档任务启动前明确报错而不是静默放行
    // 后逐文件失败；page_readiness 允许任一场景可用即进入转换页）
    #[test]
    fn readiness_for_groups_checks_only_requested_scenarios() {
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
        fs::write(
            base.join("xberg-runtime-path.txt"),
            component.to_str().expect("测试路径有效"),
        )
        .expect("模拟升级前的目录选择");
        component
    }

    // 覆盖 XB-20/XB-21：合成发布包经过真实下载安装核心；成员错误及取消保留原配置。
    #[test]
    fn installed_runtime_is_verified_before_selection() {
        use super::{AssetFile, AssetManifest, XbergManifest};
        use std::io::{Cursor, Write};
        let guard = redirect_component_env();
        let custom = guard.root.path().join("custom");
        fs::create_dir(&custom).unwrap();
        fs::write(custom.join("xberg.exe"), b"existing").unwrap();
        crate::xberg_settings::save(&custom).unwrap();
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
        assert!(fs::read_dir(installed.parent().unwrap())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".staging-")));
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

    // 覆盖 C-2（markdown 侧；XB-09 2026-10-02 修订后的存在性口径）：debug
    // 组件目录覆盖（JCHTOOLS_XBERG_INFERENCE_DIR）下，推理组件成员检查必须
    // 基于 resolve_xberg_component 解析出的组件目录，而非资产根相对路径。
    // 覆盖树内全部 media 成员以桩字节（与清单摘要不同）在场：存在性口径下
    // 必须通过——资产根下无任何组件文件，若校验仍走资产根相对路径或仍比对
    // 摘要则必失败，一次断言同时钉住两条口径；删除成员后错误基于覆盖目录
    // 点名缺失项。
    #[test]
    fn readiness_inference_pack_honors_component_dir_override() {
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
        std::env::set_var("JCHTOOLS_XBERG_INFERENCE_DIR", &external);

        super::readiness_inference_pack(&manifest, guard.root.path())
            .expect("桩字节成员应通过存在性检查（替换引擎免摘要校验）");

        fs::remove_file(external.join("models").join("vad").join("silero_vad.onnx"))
            .expect("删除一个媒体模型");
        let error = super::readiness_inference_pack(&manifest, guard.root.path())
            .expect_err("缺失成员必须失败");
        assert!(
            error.contains("silero_vad.onnx") && error.contains("缺失"),
            "错误应基于覆盖目录点名缺失成员：{error}"
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
