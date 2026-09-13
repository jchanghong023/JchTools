//! 7-Zip 引擎定位：随包目录优先，其次使用内嵌副本（首次使用时释放到用户数据目录）。
//!
//! 顺序与理由：
//! 1. `<exe 目录>/resources/7zip/`：发布包与开发环境的显式引擎，用户可直接替换（LGPL 要求可替换）；
//! 2. `<用户数据目录>/JchTools/engine/<版本-哈希>/`：内嵌副本的释放位置，同样允许用户覆盖；
//! 3. 只有以上都不存在时，才从 EXE 内嵌的压缩数据释放并逐文件校验 sha256。
//! 已存在的文件一律不重写：用户放进去的自备引擎优先，缺失时才释放内嵌副本并校验 sha256。
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/embedded_engine.rs"));
}

/// 构建时是否内嵌了完整引擎。
pub fn embedded_available() -> bool { !embedded::FILES.is_empty() && !embedded::MANIFEST.is_empty() }

/// 随包携带的引擎目录（`resources/7zip`），存在即返回。
pub fn bundled_dir() -> Option<PathBuf> {
    let directory = std::env::current_exe().ok()?.parent()?.join("resources/7zip");
    let executable = directory.join(engine_name());
    executable.is_file().then_some(directory)
}

/// 内嵌引擎的释放目录；用户可以把自备的 7z.exe / 7z.dll 放在这里覆盖内嵌副本。
pub fn embedded_dir() -> Option<PathBuf> {
    let base = crate::config::state_dir().ok()?;
    let id = if embedded::ID.is_empty() { "unknown".to_string() } else { embedded::ID.to_string() };
    Some(base.join("engine").join(id))
}

fn engine_name() -> &'static str { if cfg!(windows) { "7z.exe" } else { "7zz" } }

/// 返回可执行的 7-Zip 路径：随包目录 → 已释放的内嵌副本 → 现释放内嵌副本。
pub fn resolve_executable() -> Result<PathBuf> {
    if let Some(directory) = bundled_dir() {
        return Ok(directory.join(engine_name()));
    }
    if !embedded_available() {
        bail!("未找到 7-Zip 引擎：resources/7zip 里没有 {}, 本构建也没有内嵌引擎。请运行 scripts/fetch-7zip.ps1 获取官方完整引擎后重新构建，或用 --engine 指定完整 7z.exe。", engine_name());
    }
    let directory = embedded_dir().context("无法确定用户数据目录，不能释放内嵌的 7-Zip 引擎")?;
    release(&directory)?;
    cleanup_part_residue(&directory);
    let executable = directory.join(engine_name());
    if !executable.is_file() { bail!("内嵌引擎释放后仍缺少 {}", engine_name()); }
    Ok(executable)
}

/// 清理本应用引擎目录里崩溃残留的 `.part-*` 临时文件（write_atomic 的中间产物）。
/// 只匹配本应用命名模式（引擎文件名 + `.part-` + 进程号）且仅为文件时删除；
/// 用户自备文件与旧版本目录一律不动，避免误删。
fn cleanup_part_residue(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else { return };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let is_residue = path.is_file()
            && ["7z.exe", "7z.dll", "7zz"].iter().any(|engine| {
                name.strip_prefix(engine)
                    .and_then(|rest| rest.strip_prefix(".part-"))
                    .is_some_and(|pid| !pid.is_empty() && pid.chars().all(|c| c.is_ascii_digit()))
            });
        if is_residue { let _ = std::fs::remove_file(&path); }
    }
}

/// 把内嵌引擎释放到 `directory`，并在写入后校验每个文件的 sha256。
pub fn release(directory: &Path) -> Result<()> {
    let manifest: serde_json::Value = serde_json::from_str(embedded::MANIFEST).context("内嵌引擎清单无效")?;
    let entries = manifest["files"].as_array().context("内嵌引擎清单缺少 files 数组")?;
    std::fs::create_dir_all(directory).with_context(|| format!("创建引擎目录失败：{}", directory.display()))?;
    for entry in entries {
        let name = entry["name"].as_str().context("内嵌引擎清单缺少文件名")?;
        let expected = entry["sha256"].as_str().context("内嵌引擎清单缺少 sha256")?.to_lowercase();
        let Some((_, compressed)) = embedded::FILES.iter().find(|(file, _)| file.eq_ignore_ascii_case(name)) else {
            bail!("内嵌引擎缺少清单里声明的文件：{name}");
        };
        let target = directory.join(name);
        // 已存在的文件一律保留：用户把自备引擎放在这里即可覆盖内嵌副本（LGPL 可替换要求），
        // 只有缺失时才从 EXE 释放并逐文件校验 sha256。
        if target.is_file() { continue; }
        let bytes = inflate(compressed).with_context(|| format!("解压内嵌引擎失败：{name}"))?;
        let actual = hex::encode(Sha256::digest(&bytes));
        if actual != expected { bail!("内嵌引擎 {name} 的 sha256 与清单不一致，已拒绝写入"); }
        write_atomic(&target, &bytes).with_context(|| format!("写入引擎文件失败：{}", target.display()))?;
        if !hash_matches(&target, &expected)? {
            // 写入后校验失败（坏道、位翻转、杀软篡改等）：删掉刚写入的坏文件再报错。
            // 否则坏文件会在下次运行时因"已存在"被跳过，永久占据释放目录，
            // 使 sha256 校验机制对它彻底失效，用户只会看到晦涩的 7z 启动错误。
            let _ = std::fs::remove_file(&target);
            bail!("引擎文件写入后校验失败：{}", target.display());
        }
    }
    Ok(())
}

fn hash_matches(path: &Path, expected: &str) -> Result<bool> {
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

/// 先写临时文件再改名，避免中断时留下半个可执行文件。
fn write_atomic(target: &Path, bytes: &[u8]) -> Result<()> {
    let directory = target.parent().context("引擎目录缺少父级")?;
    let temporary = directory.join(format!("{}.part-{}", target.file_name().and_then(|name| name.to_str()).unwrap_or("engine"), std::process::id()));
    std::fs::write(&temporary, bytes)?;
    match std::fs::rename(&temporary, target) {
        Ok(()) => Ok(()),
        Err(error) => { let _ = std::fs::remove_file(&temporary); Err(error.into()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// 用户把自备引擎放进释放目录后，程序不得用内嵌副本覆盖它（LGPL 可替换要求）。
    #[test]
    fn release_never_overwrites_existing_files() {
        let Some((name, _)) = embedded::FILES.first().copied() else { return; };
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join(name);
        std::fs::write(&target, b"user supplied engine").unwrap();
        release(directory.path()).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"user supplied engine");
        std::fs::remove_file(&target).unwrap();
        release(directory.path()).unwrap();
        assert!(target.is_file(), "缺失的引擎文件应从内嵌副本释放");
    }
}
