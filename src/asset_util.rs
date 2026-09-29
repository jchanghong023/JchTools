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
use std::io::{self, Read};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
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
    pub(crate) size_bytes: u64,
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

/// 清单接入后推理组件的成员级摘要校验（XB-09）。
#[cfg(test)]
pub(crate) fn inference_ready(inference: &InferenceManifest, root: &Path) -> Result<(), String> {
    let component = root.join("xberg-inference").join(&inference.tag);
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
        || path.as_bytes().get(1) == Some(&b':')
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
        // 覆盖情况：xberg-inference/<tag> 目标的残留会被下次初始化的
        // prune_old_inference_tags 一并收集（它枚举 xberg-inference 下所有
        // 非 tag 目录删除）；其余目标（licenses 等）的残留不影响新资产使用，
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
/// 的测试都会改写 `JCHTOOLS_XBERG_INFERENCE_DIR` 与各自资产根覆盖变量，而
/// cargo test 的并行线程共享进程环境，必须经同一把锁串行，避免相互覆盖。
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

#[cfg(test)]
mod tests {
    use super::{atomic_replace_dir, atomic_replace_file};
    use std::fs;
    use std::process::Command;

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
