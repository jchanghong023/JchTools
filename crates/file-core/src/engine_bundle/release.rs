use super::embedded;
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// 固定目录本身被非目录项占用时，不可能在其下创建独立副本；改用同级稳定容器。
/// 后续解析与新释放复用同一位置，既有占用项始终保留（E-02）。
fn independent_release_base(base: &Path) -> Result<PathBuf> {
    match std::fs::symlink_metadata(base) {
        Ok(metadata) if !metadata.is_dir() => {
            let mut name = base.file_name().context("引擎目录缺少名称")?.to_os_string();
            name.push("-releases");
            Ok(base.with_file_name(name))
        }
        Ok(_) => Ok(base.to_path_buf()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(base.to_path_buf()),
        Err(error) => Err(error).context("无法检查固定引擎释放目录"),
    }
}

/// base 下已有的独立释放目录（release-* 前缀），按目录名排序保证顺序确定。
pub(super) fn existing_release_dirs(base: &Path) -> Vec<PathBuf> {
    let Ok(base) = independent_release_base(base) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(base) else {
        return Vec::new();
    };
    let mut directories: Vec<PathBuf> = entries
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("release-"))
        })
        .collect();
    directories.sort();
    directories
}

/// 在 base 下用独占创建语义新建一个独立释放目录 `release-<纳秒>-<进程号>-<序号>`，
/// 并发进程不会挤进同一个目录。
pub(super) fn fresh_release_dir(base: &Path) -> Result<PathBuf> {
    let base = independent_release_base(base)?;
    std::fs::create_dir_all(&base)
        .with_context(|| format!("创建引擎目录失败：{}", base.display()))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let pid = std::process::id();
    for attempt in 0..64u32 {
        let directory = base.join(format!("release-{nanos}-{pid}-{attempt}"));
        match std::fs::create_dir(&directory) {
            Ok(()) => return Ok(directory),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("创建独立引擎释放目录失败：{}", directory.display()))
            }
        }
    }
    bail!("无法创建新的独立引擎释放目录（连续 64 次目录名冲突）")
}

/// 目录中已存在同名引擎文件时的口径：保留该文件（绝不覆盖），按内嵌清单比对哈希并记录警告。
/// 与清单不一致时只记录、不替换；所在目录能否被使用由 resolve_among 的逐文件校验决定。
fn keep_existing_engine_file(target: &Path, name: &str, expected: &str) {
    match hash_matches(target, expected) {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!(
                component = "engine_bundle",
                event = "engine_existing_file_preserved",
                stage = "release_validation",
                file_name = name,
                error_type = "hash_mismatch",
                "既有引擎文件与内嵌清单不一致；保留原文件，候选仍须通过逐文件校验"
            );
        }
        Err(error) => {
            let io_error = error.downcast_ref::<std::io::Error>();
            tracing::warn!(
                component = "engine_bundle",
                event = "engine_existing_file_preserved",
                stage = "release_validation",
                file_name = name,
                error_type = "hash_read_failed",
                error_kind = ?io_error.map(std::io::Error::kind),
                error_code = io_error.and_then(std::io::Error::raw_os_error),
                "无法校验既有引擎文件；保留原文件，候选仍须通过逐文件校验"
            );
        }
    }
}

/// 把内嵌引擎释放到 `directory`，并在写入后校验每个文件的 sha256。
pub fn release(directory: &Path) -> Result<()> {
    let manifest: serde_json::Value =
        serde_json::from_str(embedded::MANIFEST).context("内嵌引擎清单无效")?;
    let entries = manifest["files"]
        .as_array()
        .context("内嵌引擎清单缺少 files 数组")?;
    std::fs::create_dir_all(directory)
        .with_context(|| format!("创建引擎目录失败：{}", directory.display()))?;
    for entry in entries {
        let name = entry["name"].as_str().context("内嵌引擎清单缺少文件名")?;
        // 清单由本机 build.rs 嵌入，但仍拒绝路径分隔符与 ".."，防止路径穿越写入引擎目录之外。
        crate::fsutil::validate_component(name)
            .with_context(|| format!("内嵌引擎清单文件名不安全：{name}"))?;
        let expected = entry["sha256"]
            .as_str()
            .context("内嵌引擎清单缺少 sha256")?
            .to_lowercase();
        let Some((_, compressed)) = embedded::FILES
            .iter()
            .find(|(file, _)| file.eq_ignore_ascii_case(name))
        else {
            bail!("内嵌引擎缺少清单里声明的文件：{name}");
        };
        let target = directory.join(name);
        // 已存在的文件一律保留（E-02：不自动覆盖现有文件），只有缺失时才从 EXE 释放并逐文件校验。
        // 已存在文件也计算 sha256 与清单比对并记录警告，防止恶意预植文件在
        // "已存在即跳过"逻辑下无声绕过内嵌校验；目录能否被使用由 resolve_among 在
        // 释放后整体复检决定——含不一致文件的候选会被拒绝并转新的独立位置释放。
        if target.is_file() {
            keep_existing_engine_file(&target, name, &expected);
            continue;
        }
        let bytes = inflate(compressed).with_context(|| format!("解压内嵌引擎失败：{name}"))?;
        let actual = hex::encode(Sha256::digest(&bytes));
        if actual != expected {
            bail!("内嵌引擎 {name} 的 sha256 与清单不一致，已拒绝写入");
        }
        match write_atomic(&target, &bytes) {
            Ok(()) => {}
            // 改名采用不覆盖语义：若改名前目标已出现（另一进程并发释放，或用户此刻放入
            // 自备引擎），保留已出现的目标并按"已存在文件"口径处理，绝不用覆盖语义替换。
            Err(_) if target.is_file() => {
                keep_existing_engine_file(&target, name, &expected);
                continue;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("写入引擎文件失败：{}", target.display()))
            }
        }
        match hash_matches(&target, &expected) {
            Ok(true) => {}
            Ok(false) => {
                // 写入后校验失败（坏道、位翻转、杀软篡改等）：删掉刚写入的坏文件再报错。
                // 否则坏文件会在下次运行时因"已存在"被跳过，永久占据释放目录，
                // 使 sha256 校验机制对它彻底失效，用户只会看到晦涩的 7z 启动错误。
                let _ = std::fs::remove_file(&target);
                bail!("引擎文件写入后校验失败：{}", target.display());
            }
            Err(error) => {
                // 校验读取失败同样删除：否则不可读/异常文件会因「已存在」被 keep_existing 永久信任。
                let _ = std::fs::remove_file(&target);
                return Err(error)
                    .with_context(|| format!("引擎文件写入后校验读取失败：{}", target.display()));
            }
        }
    }
    Ok(())
}

pub(super) fn hash_matches(path: &Path, expected: &str) -> Result<bool> {
    let bytes = std::fs::read(path)?;
    Ok(hex::encode(Sha256::digest(&bytes)) == expected)
}

fn inflate(compressed: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut decoder = flate2::read::ZlibDecoder::new(compressed);
    let mut bytes = Vec::new();
    decoder.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// 先写临时文件再改名，避免中断时留下半个可执行文件；改名用不覆盖语义，
/// 已存在的目标一律拒绝替换（用户自备文件优先，不用覆盖语义替换用户文件）。
pub(super) fn write_atomic(target: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let directory = target.parent().context("引擎目录缺少父级")?;
    // 随机名称加独占创建：不截断既有文件，也不与本进程的其他释放操作共用临时路径。
    let temporary = directory.join(format!(".engine-part-{}", uuid::Uuid::new_v4().simple()));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    // create_new 成功后才拥有该路径；写入、同步或改名失败只清理本次创建的文件，
    // 不扫描或删除仅名称相似的历史残留及其他进程的临时文件。
    let result = (|| -> Result<()> {
        let mut file = file;
        file.write_all(bytes)?;
        // Linux 等 unix：先设执行位再 fsync，保证权限与内容一并落盘。
        // 否则断电后可能出现「内容完整但无执行位」且被 keep_existing 长期信任。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o755))?;
        }
        // rename 前 fsync：避免断电后改名成功但内容未落盘，留下损坏的可执行文件。
        file.sync_all()?;
        drop(file);
        crate::fsutil::rename_noreplace(&temporary, target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}
