use jchtools_assets::asset_util::{
    atomic_replace_dir, cleanup_stale_staging_dirs, ensure_not_cancelled, finalize_staging,
    require_component_members, require_inference_members_for_scenario, resolve_xberg_component,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use uuid::Uuid;

use super::manifest::{asset_root, load_manifest, write_notice, AssetManifest};

const RUNTIME_SELECTION_FILE: &str = "xberg-runtime-path.txt";

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
pub(super) fn readiness_inference_pack(
    manifest: &AssetManifest,
    _root: &Path,
) -> Result<(), String> {
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
