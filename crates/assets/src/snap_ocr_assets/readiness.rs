use super::manifest::{load_manifest, SnapAsset, SnapWorker, DATA_DIRECTORY, WORKER_EXE_NAME};
use crate::asset_util::{
    require_component_members, resolve_xberg_component, state_dir_asset_root, verify_file,
};
use sha2::{Digest, Sha256};
use std::fmt::Write as FmtWrite;
use std::path::{Path, PathBuf};

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

/// 截图服务命名管道名：`\\.\pipe\jchtools-snap-ocr-<hash>`，hash 为当前进程
/// TokenUser SID 与 TokenSessionId 共同身份 UTF-8 字节 SHA-256 的前 16 个小写字符。
/// 身份查询失败时拒绝连接；测试资产根覆盖仍派生测试专用名。
pub fn pipe_name() -> Result<String, String> {
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
        return Ok(format!(r"\\.\pipe\jchtools-snap-ocr-test-{hash}"));
    }
    let identity = jchtools_foundation::process_identity::user_session_key()?;
    let digest = Sha256::digest(identity.as_bytes());
    let mut hash = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        // 两位十六进制，无格式化失败路径。
        let _ = write!(&mut hash, "{byte:02x}");
    }
    Ok(format!(r"\\.\pipe\jchtools-snap-ocr-{hash}"))
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

/// bundled worker 必须匹配构建/打包时回填的 size/SHA-256；目录名与
/// debug_assertions 不能作为执行自有后台程序的完整性豁免凭据。
/// 就绪检查与启动链（`worker_install_path`）共用本判据。
pub(super) fn verify_bundled_worker(path: &Path, worker: &SnapWorker) -> Result<(), String> {
    verify_file(path, worker.size_bytes, &worker.sha256)
        .map_err(|error| format!("{error}；{}", worker_repair_message(worker)))
}

/// 就绪检查的工作进程段：bundled 在场时走启动链同口径校验（S7-02：损坏的
/// worker 必须在 readiness 阶段即报未就绪并携带原因，不得拖到启动链才失败，
/// O-09「自有可选资产不完整必须阻止使用并明确提示」）；缓存安装按清单校验。
pub(super) fn worker_readiness(worker: &SnapWorker, root: &Path) -> Result<(), String> {
    if let Some(path) = bundled_worker() {
        return verify_bundled_worker(&path, worker)
            .map_err(|error| format!("截图 OCR 工作进程校验失败：{error}"));
    }
    if let Err(error) = worker_ready(worker, root) {
        tracing::error!(
            component = "snap_ocr_assets",
            event = "asset_worker_readiness_failed",
            stage = "cached_worker_verify",
            result = "not_ready",
            "截图 OCR 工作进程就绪检查失败"
        );
        return Err(format!("截图 OCR 工作进程校验失败：{error}"));
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
    readiness_inference_pack(&root)
}

pub(super) fn local_asset_readiness(assets: &[SnapAsset], root: &Path) -> Result<(), String> {
    for asset in assets {
        if asset.is_embedded() {
            continue;
        }
        // S7-05：透传具体失败原因（缺失/大小/摘要），GUI 侧直接展示该文案；
        // 压缩成统一「未安装或校验失败」会丢失失败种类（O-09/O-30）。
        if let Err(error) = asset_ready(asset, root) {
            tracing::error!(
                component = "snap_ocr_assets",
                event = "asset_readiness_failed",
                stage = "local_asset_verify",
                asset_id = %asset.id,
                result = "not_ready",
                "截图 OCR 资产就绪检查失败"
            );
            return Err(format!("资产 {}：{}", asset.id, error));
        }
    }
    Ok(())
}

/// 截图就绪与 worker 启动共用完整的 snapshot 成员清单；共同运行库不可遗漏，
/// 文档模型与音频可选资产不构成截图场景前置条件（XB-09）。
pub(super) fn readiness_inference_pack(root: &Path) -> Result<(), String> {
    let component = xberg_inference_ready(root)?;
    jchtools_xberg::xberg_runtime::validate_assets(&component, "snapshot")
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
pub(super) fn xberg_layout_ready(component: &Path) -> Result<(), String> {
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

pub(super) fn worker_repair_message(worker: &SnapWorker) -> String {
    format!(
        "截图 OCR 工作进程未安装或与当前清单摘要不匹配（{}）；请重新安装当前 JchTools 包修复 worker，不会下载其他版本",
        worker.install_path
    )
}

/// 单个资产（含全部成员）是否已在最终位置校验通过。
pub(super) fn asset_ready(asset: &SnapAsset, root: &Path) -> Result<(), String> {
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

pub(super) fn worker_ready(worker: &SnapWorker, root: &Path) -> Result<(), String> {
    if worker.archive_type != "file" {
        return Err(format!("工作进程 {} 必须是单文件资产", worker.id));
    }
    verify_file(
        &root.join(&worker.install_path),
        worker.size_bytes,
        &worker.sha256,
    )
    .map_err(|error| format!("{error}；{}", worker_repair_message(worker)))
}
