//! 可选资产层共享的底层工具：路径安全校验、SHA-256 校验、原子落位、
//! zip 安全解包与 Xberg 推理组件包安装核心。
//!
//! 转 Markdown（`markdown_assets`）与截图 OCR（`snap_ocr_assets`）各自维护
//! 清单与就绪口径，但落盘/校验/原子落位语义必须同源：同一份实现保证两个功能
//! 的 staging → 校验 → 原子落位、失败不动已验证资产、跨重试复用等行为
//! 完全一致（XB-09/XB-10 安装语义）。上层模块只保留各自的清单解析、
//! 就绪检查与用户指引文案；HTTP 下载原语（`ureq`）按 P-03 执法测试的
//! 文件白名单留在 `snap_ocr_assets`，本模块保持离线。

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use uuid::Uuid;
use zip::ZipArchive;

#[derive(Debug, Deserialize)]
pub(crate) struct InferenceManifest {
    pub(crate) tag: String,
    pub(crate) url: String,
    pub(crate) size_bytes: u64,
    pub(crate) sha256: String,
    pub(crate) members: Vec<InferenceMember>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct InferenceMember {
    pub(crate) path: String,
    pub(crate) install_path: String,
    // 摘要与大小只被下载安装链（install_inference_pack / inference_layout_ready，
    // 按 XB-10 当前仅在测试启用）消费；运行时存在性检查（XB-09 2026-10-02
    // 修订）不读取，非测试构建因此允许未读。
    // [quality-baseline approved 2026-10-03] 测试门控消费字段，经用户裁定保留
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) size_bytes: u64,
    // [quality-baseline approved 2026-10-03] 同上：测试门控消费字段
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) sha256: String,
}

/// 单个资产的下载接缝：生产实现走固定来源的 HTTP 下载（实现位于
/// `snap_ocr_assets`，P-03 联网边界测试按文件白名单执法），测试注入本地
/// 供给或失败脚本（T-05/T-21、O-06 语义）。
pub(crate) trait AssetDownloader {
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

/// 按可选的清单 tag 解析唯一组件目录（XB-09）：
/// - 无 tag（清单未接入）：唯一子目录即组件目录；
/// - 有 tag：目录名必须与清单一致，不一致明确报错并指引更新；
/// - 零个或多个候选都视为无法确定，不静默选择。
#[cfg(test)]
pub(crate) fn resolve_component_with_tag(
    base: &Path,
    expected_tag: Option<&str>,
    missing_message: &str,
) -> Result<PathBuf, String> {
    let entries = fs::read_dir(base).map_err(|_| missing_message.to_string())?;
    let mut versions = Vec::new();
    for entry in entries.flatten() {
        if entry.path().is_dir() {
            versions.push(entry.path());
        }
    }
    match (versions.len(), expected_tag) {
        (0, _) => Err(missing_message.to_string()),
        (1, None) => Ok(versions.remove(0)),
        (1, Some(tag)) => {
            let component = versions.remove(0);
            if component.file_name().and_then(|name| name.to_str()) == Some(tag) {
                Ok(component)
            } else {
                Err(format!(
                    "推理组件版本与清单不一致（安装 {}，清单要求 {tag}）；请在对应功能页重新初始化以更新组件",
                    component.display()
                ))
            }
        }
        // 多目录 + 有清单 tag：优先选中清单 tag 目录（一次瞬时清理失败留下的
        // 旧版本目录不应让组件不可用；下次初始化会再尝试清理）。
        (_, Some(tag)) => match versions
            .iter()
            .position(|path| path.file_name().and_then(|name| name.to_str()) == Some(tag))
        {
            Some(index) => Ok(versions.swap_remove(index)),
            None => Err(format!(
                "推理组件目录存在多个版本且无清单要求的 {tag}；请重新初始化以更新组件"
            )),
        },
        (_, None) => {
            Err("推理组件目录存在多个版本，无法确定使用哪一个；请只保留一个版本目录".into())
        }
    }
}

/// 下载、校验并原子安装推理组件包（XB-09/XB-10）。
///
/// 最终位置成员全部校验通过时不重下（跨重试复用）；下载与解包在 staging 内
/// 完成并二次校验（归档摘要 + 每成员摘要），全部通过后整目录原子落位到
/// `xberg-inference/<tag>/`，随后移除其他版本目录（不混用版本）。取消或
/// 失败不会触碰已验证的安装。
#[cfg(test)]
pub(crate) fn install_inference_pack(
    inference: &InferenceManifest,
    staging: &Path,
    root: &Path,
    cancel: &AtomicBool,
    downloader: &mut dyn AssetDownloader,
    progress: &mut impl FnMut(String),
) -> Result<(), String> {
    if inference_ready(inference, root).is_ok() {
        progress("复用已校验的推理组件".to_string());
        // 旧版本目录的清理是尽力而为：清单 tag 目录可正常解析与使用，清理失败
        // （如目录被占用）只提示，不让「已可用」的安装报失败。
        if let Err(error) = prune_old_inference_tags(root, &inference.tag) {
            progress(format!("警告：{error}"));
        }
        return Ok(());
    }
    ensure_not_cancelled(cancel)?;
    progress(format!(
        "下载 Xberg 推理组件包（{}，{} 字节）",
        inference.tag, inference.size_bytes
    ));
    let stage_dir = staging.join("xberg-inference");
    fs::create_dir_all(&stage_dir).map_err(|error| format!("创建推理组件临时目录失败：{error}"))?;
    let archive = stage_dir.join("download.zip");
    downloader.download(
        &inference.url,
        &archive,
        inference.size_bytes,
        &inference.sha256,
        cancel,
        progress,
    )?;
    ensure_not_cancelled(cancel)?;
    // 下载器校验之外独立复核暂存内容，防伪造的“下载成功”。
    verify_file(&archive, inference.size_bytes, &inference.sha256)
        .map_err(|error| format!("推理组件包下载内容校验失败：{error}"))?;
    let extracted = stage_dir.join("extracted");
    fs::create_dir_all(&extracted).map_err(|error| format!("创建推理组件解包目录失败：{error}"))?;
    extract_zip_safely(&archive, &extracted, cancel)?;
    let staged_component = stage_dir.join("component");
    for member in &inference.members {
        ensure_not_cancelled(cancel)?;
        let source = extracted.join(&member.path);
        verify_file(&source, member.size_bytes, &member.sha256)
            .map_err(|error| format!("推理组件成员 {} 校验失败：{error}", member.path))?;
        let target = staged_component.join(&member.install_path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("创建推理组件成员目录失败：{error}"))?;
        }
        fs::copy(&source, &target)
            .map_err(|error| format!("落位推理组件成员 {} 失败：{error}", member.install_path))?;
    }
    // staging 内自校验通过后整目录原子落位，失败恢复旧目录。
    inference_layout_ready(&staged_component, inference)?;
    atomic_replace_dir(
        &staged_component,
        &root.join("xberg-inference").join(&inference.tag),
    )?;
    // 落位成功后清理旧版本目录：尽力而为（清单 tag 目录已可解析使用，清理失败
    // 只提示，不把成功的安装报成失败）。
    if let Err(error) = prune_old_inference_tags(root, &inference.tag) {
        progress(format!("警告：{error}"));
    }
    Ok(())
}

/// 组件在位校验（存在性，不做摘要）：清单声明的成员在组件目录内逐个存在，
/// 缺失时报「推理组件不完整」并指明首个缺失成员与组件目录（截图 OCR 与
/// 转 Markdown 共用同一口径与文案）。`required` 由调用方以组件目录为前缀
/// 拼接，错误文案里的相对路径分隔符因此与各调用方的 join 链保持一致。
pub(crate) fn require_component_members(
    component: &Path,
    required: &[PathBuf],
) -> Result<(), String> {
    for path in required {
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

/// 资产根目录的固定回退链（不含各功能的测试覆盖变量）：用户状态目录 →
/// LOCALAPPDATA\JchTools → 系统临时目录；截图 OCR 与转 Markdown 两个资产
/// 模块共用，保证回退口径只有一处实现。
pub(crate) fn state_dir_asset_root(data_directory: &str) -> PathBuf {
    if let Ok(path) = crate::config::state_dir() {
        return path.join(data_directory);
    }
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local_app_data)
            .join("JchTools")
            .join(data_directory);
    }
    std::env::temp_dir().join("JchTools").join(data_directory)
}

/// 推理组件包 tag 合法性：非空且不含路径分隔符、`..` 与冒号（防止落位到
/// 组件目录之外）。两侧清单校验共用同一谓词。
pub(crate) fn valid_component_tag(tag: &str) -> bool {
    !tag.is_empty()
        && !tag.contains('/')
        && !tag.contains('\\')
        && !tag.contains("..")
        && !tag.contains(':')
}

/// 清单接入后推理组件的成员级摘要校验（XB-09）。
#[cfg(test)]
pub(crate) fn inference_ready(inference: &InferenceManifest, root: &Path) -> Result<(), String> {
    inference_layout_ready(
        &root.join("xberg-inference").join(&inference.tag),
        inference,
    )
}

/// 落位前对 staging 组件树做成员级复核（存在 + 摘要），确保原子替换进来的
/// 目录就是清单声明的完整安装。就绪检查（C-2）对解析出的组件目录复用
/// 同一口径。
#[cfg(test)]
pub(crate) fn inference_layout_ready(
    component: &Path,
    inference: &InferenceManifest,
) -> Result<(), String> {
    for member in &inference.members {
        verify_file(
            &component.join(&member.install_path),
            member.size_bytes,
            &member.sha256,
        )
        .map_err(|error| format!("推理组件成员 {}：{error}", member.install_path))?;
    }
    Ok(())
}

/// 成功安装清单 tag 后移除其他版本目录：同一安装只保留一个版本（XB-09）。
#[cfg(test)]
pub(crate) fn prune_old_inference_tags(root: &Path, keep: &str) -> Result<(), String> {
    let base = root.join("xberg-inference");
    let entries = fs::read_dir(&base).map_err(|error| format!("枚举推理组件目录失败：{error}"))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && path.file_name().and_then(|name| name.to_str()) != Some(keep) {
            fs::remove_dir_all(&path).map_err(|error| {
                format!("移除旧版本推理组件失败（{}）：{error}", path.display())
            })?;
        }
    }
    Ok(())
}

pub(crate) fn validate_relative_path(path: &str) -> Result<(), String> {
    let path = path.replace('\\', "/");
    let candidate = Path::new(&path);
    if path.is_empty()
        || candidate.is_absolute()
        || path.starts_with('/')
        || path.contains('\0')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || path.contains(':')
    {
        return Err(format!("资产路径不安全：{path}"));
    }
    Ok(())
}

pub(crate) fn verify_file(
    path: &Path,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<(), String> {
    verify_file_inner(path, expected_size, expected_sha256, None)
}

/// 与 [`verify_file`] 相同，但在大文件哈希期间按块响应取消。
pub(crate) fn verify_file_with_cancel(
    path: &Path,
    expected_size: u64,
    expected_sha256: &str,
    cancel: &AtomicBool,
) -> Result<(), String> {
    verify_file_inner(path, expected_size, expected_sha256, Some(cancel))
}

fn verify_file_inner(
    path: &Path,
    expected_size: u64,
    expected_sha256: &str,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err("不是普通文件".to_string());
    }
    if metadata.len() != expected_size {
        return Err(format!("大小 {}，预期 {expected_size}", metadata.len()));
    }
    let actual = sha256_file_inner(path, cancel).map_err(|error| {
        if error.kind() == io::ErrorKind::Interrupted {
            "用户已取消初始化".to_string()
        } else {
            error.to_string()
        }
    })?;
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        return Err(format!("SHA256 {actual}，预期 {expected_sha256}"));
    }
    Ok(())
}

pub(crate) fn sha256_file_inner(path: &Path, cancel: Option<&AtomicBool>) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        if cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "用户已取消初始化",
            ));
        }
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

pub(crate) fn atomic_replace_file(staged: &Path, destination: &Path) -> Result<(), String> {
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
            if let Err(restore_error) = restore_backup(&backup, destination, "文件") {
                return Err(format!("{install_error}；{restore_error}"));
            }
        }
        return Err(install_error);
    }
    if had_existing {
        // B-1：新文件已就位，关键变更成功；旧备份清理失败不构成安装失败
        //（此前 Windows 上防护软件/索引器短暂持有句柄即可把成功误报为失败）。
        // 残留为同目录下的 .old-file-<uuid>（罕见），不影响新资产使用，可安全
        // 手动删除；本函数无进度通道，静默忽略。
        let _ = fs::remove_file(&backup);
    }
    Ok(())
}

pub(crate) fn atomic_replace_dir(staged: &Path, destination: &Path) -> Result<(), String> {
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
            if let Err(restore_error) = restore_backup(&backup, destination, "资产") {
                return Err(format!("{install_error}；{restore_error}"));
            }
        }
        return Err(install_error);
    }
    if had_existing {
        // B-1：新目录已就位，关键变更成功；旧备份清理失败不构成安装失败。
        // 残留为同目录下的 .old-<uuid>（罕见，防护软件/索引器短暂持有句柄）。
        // 覆盖情况：xberg-inference/<tag> 目标的残留按设计交给
        // prune_old_inference_tags 收集（它枚举 xberg-inference 下所有非 tag
        // 目录删除；该安装链当前仅在测试中启用，生产初始化只校验共享目录，
        // XB-10）；其余目标（licenses 等）的残留不影响新资产使用，
        // 可手动删除。本函数无进度通道，静默忽略。
        let _ = fs::remove_dir_all(&backup);
    }
    Ok(())
}

pub(crate) fn restore_backup(backup: &Path, destination: &Path, kind: &str) -> Result<(), String> {
    fs::rename(backup, destination).map_err(|error| {
        format!(
            "恢复旧{kind}失败：{error}；旧{kind}仍保留在：{}",
            backup.display()
        )
    })
}

/// 测试支持：跨模块共享的进程环境变量锁。markdown_assets 与 snap_ocr_assets
/// 的测试都会改写各自资产根覆盖变量，而 cargo test 的并行线程共享进程环境，
/// 必须经同一把锁串行，避免相互覆盖。
#[cfg(test)]
pub(crate) mod test_env {
    use std::sync::{Mutex, OnceLock};

    pub(crate) fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }
}

pub(crate) fn ensure_not_cancelled(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) {
        Err("用户已取消初始化".to_string())
    } else {
        Ok(())
    }
}

// ── 两资产模块共享的初始化骨架 ─────────────────────────────────────────
// markdown_assets 与 snap_ocr_assets 对同一安装语义（组件解析、staging 收尾、
// 残留兜底、成员校验）此前各留一份相同实现，仅靠注释约定同步；收敛到本模块
// 后一处修改即可同时生效（XB-09/XB-16 同口径），模块只保留各自的清单与文案。

/// 组件目录解析：只取应用 SQLite 保存的共享 Xberg 目录（XB-18/XB-19），
/// 即设置页保存的目录或产品内下载后保存的目录（2026-10-04 用户指示删除
/// `JCHTOOLS_XBERG_INFERENCE_DIR` 环境变量覆盖）。两个功能必须用同一规则
/// 解析同一安装，不允许出现第二套目录口径。
pub(crate) fn resolve_xberg_component() -> Result<PathBuf, String> {
    crate::xberg_settings::required()
}

/// 就绪检查的推理组件成员检查：只检查请求场景需要的成员是否在场（XB-16
/// 场景隔离，归属规则见 `xberg_runtime::asset_for_scenario`），逐成员做
/// 存在性检查（XB-09 2026-10-02 修订：运行时不比对大小与 SHA-256，用户
/// 可自行替换引擎文件；成员缺失仍明确报错），错误统一带成员相对路径。
pub(crate) fn require_inference_members_for_scenario(
    component: &Path,
    scenario: &str,
    members: &[InferenceMember],
) -> Result<(), String> {
    for member in members {
        if crate::xberg_runtime::asset_for_scenario(&member.install_path, scenario)
            && !component.join(&member.install_path).is_file()
        {
            return Err(format!(
                "推理组件成员 {} 缺失（组件目录 {}）",
                member.install_path,
                component.display()
            ));
        }
    }
    Ok(())
}

/// 初始化收尾：删除本轮 staging 目录，主结果优先返回。
///
/// staging 清理是尽力而为（B-1）：关键变更已成功时清理失败只经 progress
/// 发出警告、维持 Ok（Windows 上防护软件短暂持有句柄曾把成功初始化误报为
/// 失败）；残留目录由下次 initialize 入口的 [`cleanup_stale_staging_dirs`]
/// 兜底收集。
pub(crate) fn finalize_staging(
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

/// 兜底清理资产根下历史残留的 `.staging-*` 目录（B-2），单项失败跳过继续；
/// 各模块特有的残留（旧选择临时文件、expected-tag 标记）由调用方在本函数
/// 之后自行清扫。
///
/// 并发前提：主程序单实例，各功能的初始化由 gui.rs 对应守卫串行（单初始化
/// 线程），入口处发现的 `.staging-*` 必为历史残留，不存在在途 staging 被误删
/// 的并发窗口。`.staging-*` 前缀的非目录条目不是本流程产物（staging 恒为
/// 目录），按设计跳过；资产根不存在（首次运行）时无需清理。
pub(crate) fn cleanup_stale_staging_dirs(root: &Path) {
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
        // Windows 冒号可寻址替代数据流，enclosed_name 只保证路径不越界。
        if entry.name().contains(':') {
            return Err(format!("压缩包包含不安全路径：{}", entry.name()));
        }
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
        // 使用文件系统的实际同名规则拒绝碰撞，保留已解出的首个成员。
        let mut output = File::options()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|error| format!("创建解包文件失败：{error}"))?;
        let mut buffer = vec![0_u8; 1024 * 1024];
        loop {
            ensure_not_cancelled(cancel)?;
            let read = entry
                .read(&mut buffer)
                .map_err(|error| format!("读取解包文件失败：{error}"))?;
            if read == 0 {
                break;
            }
            output
                .write_all(&buffer[..read])
                .map_err(|error| format!("写入解包文件失败：{error}"))?;
        }
        output
            .sync_all()
            .map_err(|error| format!("同步解包文件失败：{error}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{atomic_replace_dir, atomic_replace_file, verify_file_with_cancel};
    use std::fs;
    use std::process::Command;
    use std::sync::atomic::AtomicBool;

    // 覆盖 XB-10 / T-06 / O-09：Windows 等价成员不能覆盖已解出的成员。
    #[test]
    fn zip_case_collision_preserves_first_member() {
        let root = tempfile::tempdir().expect("创建测试目录");
        let archive = root.path().join("assets.zip");
        let mut writer = zip::ZipWriter::new(fs::File::create(&archive).expect("创建压缩包"));
        for (name, bytes) in [("asset.bin", b"first".as_slice()), ("ASSET.BIN", b"second")] {
            writer
                .start_file(name, zip::write::SimpleFileOptions::default())
                .expect("创建压缩包成员");
            std::io::Write::write_all(&mut writer, bytes).expect("写入成员");
        }
        writer.finish().expect("完成压缩包");
        let extracted = root.path().join("extracted");
        fs::create_dir(&extracted).expect("创建解包目录");

        let result = super::extract_zip_safely(&archive, &extracted, &AtomicBool::new(false));

        assert!(result.is_err(), "Windows 同名成员必须拒绝而非覆盖");
        assert_eq!(fs::read(extracted.join("asset.bin")).unwrap(), b"first");
    }

    // 覆盖 XB-10 / T-06 / O-09：资产成员路径不能寻址 Windows 替代数据流。
    #[test]
    fn asset_path_rejects_alternate_data_stream() {
        for path in ["dir/file:stream", "file.bin:stream", "dir/file::$DATA"] {
            assert!(
                super::validate_relative_path(path).is_err(),
                "必须拒绝替代数据流路径：{path}"
            );
        }
        assert!(super::validate_relative_path("dir/file.bin").is_ok());
    }

    // 覆盖 XB-10 / T-06 / O-09：下载归档的成员同样不能写入替代数据流。
    #[test]
    fn zip_rejects_alternate_data_stream_member() {
        let root = tempfile::tempdir().expect("创建测试目录");
        let archive = root.path().join("assets.zip");
        let mut writer = zip::ZipWriter::new(fs::File::create(&archive).expect("创建压缩包"));
        writer
            .start_file("dir/file:stream", zip::write::SimpleFileOptions::default())
            .expect("创建压缩包成员");
        std::io::Write::write_all(&mut writer, b"stream").expect("写入成员");
        writer.finish().expect("完成压缩包");
        let extracted = root.path().join("extracted");
        fs::create_dir(&extracted).expect("创建解包目录");

        let result = super::extract_zip_safely(&archive, &extracted, &AtomicBool::new(false));

        assert!(result.is_err(), "压缩包替代数据流成员必须拒绝");
        assert!(!extracted.join("dir/file").exists(), "不能创建流的宿主文件");
    }

    #[test]
    fn verify_file_with_cancel_stops_before_hashing() {
        let root = tempfile::tempdir().expect("创建测试目录");
        let file = root.path().join("asset.bin");
        fs::write(&file, b"asset").expect("写入资产");
        let cancel = AtomicBool::new(true);

        let error = verify_file_with_cancel(&file, 5, &"00".repeat(32), &cancel)
            .expect_err("已取消的哈希校验必须立即停止");
        assert_eq!(error, "用户已取消初始化");
    }

    // 覆盖 B-1：原子就位成功后，旧备份清理失败不得把已成功的安装误报为失败。
    // 注入方式：旧目标本身是目录时，备份（改名后的目录）无法被 remove_file
    // 删除，稳定复现「两次关键 rename 均已成功、仅清理失败」。生产触发是
    // 防护软件/索引器短暂持有句柄（不可控），此处以确定性删除失败替代；
    // 勿用句柄方案：句柄需在改名前打开，而无共享删除的句柄会先阻塞第一次
    // rename，根本走不到清理分支。
    #[test]
    fn atomic_replace_file_cleanup_failure_still_succeeds() {
        let root = tempfile::tempdir().expect("创建测试目录");
        let destination = root.path().join("asset.bin");
        fs::create_dir(&destination).expect("旧目标预置为目录（使备份不可删除）");
        let staged = root.path().join("staged.bin");
        fs::write(&staged, b"new-asset").expect("写入新资产");

        atomic_replace_file(&staged, &destination)
            .expect("关键变更已就位时清理失败不得报失败（B-1）");

        assert_eq!(
            fs::read(&destination).expect("新资产必须已就位"),
            b"new-asset",
            "destination 必须是新资产内容"
        );
    }

    // 覆盖 B-1：目录级原子就位成功后，备份清理失败不得报失败。
    // 注入方式（Windows ACL）：拒绝内部文件的 DELETE（仅对象）、目标目录
    // 继承拒绝 DC（删除子项）。目标目录整体改名只需对象自身 DELETE 权限
    // （未被拒）仍可成功；清理时删除内部文件的两条路径（对象 DELETE /
    // 父目录 DC）均被拒 → remove_dir_all 确定性失败。
    // 平台门禁原因：注入依赖 icacls（Windows 独有）；「清理失败不误报成功」
    // 的同一语义由无门禁的 atomic_replace_file 用例覆盖（本项目按 P-07 仅
    // 支持 Windows，门禁不影响生产平台覆盖）。
    #[cfg(windows)]
    #[test]
    fn atomic_replace_dir_cleanup_failure_still_succeeds() {
        let root = tempfile::tempdir().expect("创建测试目录");
        let destination = root.path().join("asset-dir");
        fs::create_dir_all(destination.join("nested")).expect("预置旧资产目录");
        fs::write(destination.join("nested").join("old.txt"), b"old").expect("预置旧资产文件");
        let user = std::env::var("USERNAME").expect("获取当前用户名");
        for (target, rule) in [
            (
                destination.join("nested").join("old.txt"),
                format!("{user}:(D)"),
            ),
            (destination.clone(), format!("{user}:(OI)(CI)(DC)")),
        ] {
            let output = Command::new("icacls")
                .arg(&target)
                .args(["/deny", &rule])
                .output()
                .expect("调用 icacls 注入删除失败");
            assert!(
                output.status.success(),
                "icacls 必须成功：{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let staged = root.path().join("staged-dir");
        fs::create_dir_all(&staged).expect("创建新资产目录");
        fs::write(staged.join("new.txt"), b"new-asset").expect("写入新资产");

        let result = atomic_replace_dir(&staged, &destination);
        // 无论结论如何先恢复 ACL，保证临时目录可被 tempfile 清理。
        let _ = Command::new("icacls")
            .arg(root.path())
            .args(["/reset", "/t", "/q"])
            .output();
        result.expect("关键变更已就位时清理失败不得报失败（B-1）");
        assert_eq!(
            fs::read(destination.join("new.txt")).expect("新资产必须已就位"),
            b"new-asset",
            "destination 必须是新资产内容"
        );
    }
}
