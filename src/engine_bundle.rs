//! 7-Zip 引擎定位：随包目录优先，其次使用内嵌副本（首次使用时释放到用户数据目录）。
//!
//! 顺序与理由：
//! 1. `<exe 目录>/resources/7zip/`：发布包与开发环境的显式引擎，用户可直接替换（LGPL 要求可替换）；
//! 2. `<用户数据目录>/JchTools/engine/<版本-哈希>/`：内嵌副本的释放位置，同样允许用户覆盖；
//! 3. 只有以上都不存在时，才从 EXE 内嵌的压缩数据释放并逐文件校验 sha256。
//!    已存在的文件一律不重写（用户自备引擎优先，符合 LGPL 可替换要求），
//!    但会计算 sha256 与内嵌清单比对：不一致时保留文件并记录警告，防止预植文件无声通过校验。
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/embedded_engine.rs"));
}

/// 构建时是否内嵌了完整引擎。
pub fn embedded_available() -> bool {
    !embedded::FILES.is_empty() && !embedded::MANIFEST.is_empty()
}

/// 随包携带的引擎目录（`resources/7zip`），存在即返回。
pub fn bundled_dir() -> Option<PathBuf> {
    let directory = std::env::current_exe()
        .ok()?
        .parent()?
        .join("resources/7zip");
    let executable = directory.join(engine_name());
    executable.is_file().then_some(directory)
}

/// 内嵌引擎的释放目录；用户可以把自备的 7z.exe / 7z.dll 放在这里覆盖内嵌副本。
pub fn embedded_dir() -> Option<PathBuf> {
    let base = crate::config::state_dir().ok()?;
    let id = if embedded::ID.is_empty() {
        "unknown".to_string()
    } else {
        embedded::ID.to_string()
    };
    Some(base.join("engine").join(id))
}

fn engine_name() -> &'static str {
    if cfg!(windows) {
        "7z.exe"
    } else {
        "7zz"
    }
}

/// 返回可执行的 7-Zip 路径：随包目录 → 已释放的内嵌副本 → 现释放内嵌副本。
pub fn resolve_executable() -> Result<PathBuf> {
    if let Some(directory) = bundled_dir() {
        let executable = directory.join(engine_name());
        // 随包目录本就允许用户替换引擎（LGPL）；与内嵌清单不一致时仅警告，不阻断。
        // 但 Windows 上 7z.dll 缺失不是「用户替换了引擎」，而是不完整引擎（例如只拷了 exe），
        // 必须在启动前失败，避免推迟到首次解压才报晦涩错误。
        #[cfg(windows)]
        {
            if executable.is_file() && !directory.join("7z.dll").is_file() {
                bail!("随包 7-Zip 引擎不完整：缺少 7z.dll（{}/）。请使用 scripts/fetch-7zip.ps1 获取官方完整引擎，或删除随包目录改用内嵌引擎。", directory.display());
            }
        }
        warn_all_manifest_mismatches(&directory, "随包 7-Zip 引擎");
        return Ok(executable);
    }
    if !embedded_available() {
        bail!("未找到 7-Zip 引擎：resources/7zip 里没有 {}, 本构建也没有内嵌引擎。请运行 scripts/fetch-7zip.ps1 获取官方完整引擎后重新构建。", engine_name());
    }
    let directory = embedded_dir().context("无法确定用户数据目录，不能释放内嵌的 7-Zip 引擎")?;
    release(&directory)?;
    cleanup_part_residue(&directory);
    let executable = directory.join(engine_name());
    if !executable.is_file() {
        bail!("内嵌引擎释放后仍缺少 {}", engine_name());
    }
    // 与随包路径对称：Windows 上缺 7z.dll 视为不完整引擎，释放后必须再检一次。
    #[cfg(windows)]
    {
        if !directory.join("7z.dll").is_file() {
            bail!(
                "内嵌引擎释放后不完整：缺少 7z.dll（{}）。",
                directory.display()
            );
        }
    }
    // 返回前按内嵌清单逐个复检哈希（主程序 + 7z.dll），缩小释放与实际调用之间的篡改窗口（TOCTOU）。
    // Windows 上 7z.dll 是主要攻击面；用户主动放入的替换引擎不一致时记录警告并放行（LGPL 可替换要求）。
    warn_all_manifest_mismatches(&directory, "已释放的内嵌 7-Zip 引擎");
    Ok(executable)
}

/// 在内嵌清单中查找 `file_name` 的期望 sha256（小写十六进制）；找不到时返回 None。
fn manifest_expectation(file_name: &str) -> Option<String> {
    let manifest: serde_json::Value = serde_json::from_str(embedded::MANIFEST).ok()?;
    let files = manifest["files"].as_array()?;
    files.iter().find_map(|entry| {
        let name = entry["name"].as_str()?;
        if name.eq_ignore_ascii_case(file_name) {
            entry["sha256"].as_str().map(str::to_lowercase)
        } else {
            None
        }
    })
}

/// 按内嵌清单对目录中所有声明的引擎文件逐个复检哈希（主程序 + 7z.dll）。
/// resolve_executable 返回前调用，缩小释放与实际调用之间的篡改窗口（TOCTOU）。
/// Windows 上 7z.dll 是主要攻击面，不能只复检主程序。
fn warn_all_manifest_mismatches(directory: &Path, context: &str) {
    let Ok(manifest) = serde_json::from_str::<serde_json::Value>(embedded::MANIFEST) else {
        return;
    };
    let Some(entries) = manifest["files"].as_array() else {
        return;
    };
    for entry in entries {
        let Some(name) = entry["name"].as_str() else {
            continue;
        };
        warn_if_hash_mismatch(&directory.join(name), context);
    }
}

/// 把引擎哈希警告追加写入用户数据目录下的 `engine-warnings.log`。
/// GUI 启动不展示 stderr，仅 eprintln 等于静默放行；落盘留下持久痕迹供排查。
fn persist_engine_warning(message: &str) {
    eprintln!("{message}");
    // 单元测试会用刻意不匹配的文件调用 release()：不得把测试痕迹写进真实用户数据目录的日志。
    #[cfg(not(test))]
    {
        if let Ok(dir) = crate::config::state_dir() {
            if std::fs::create_dir_all(&dir).is_ok() {
                use std::io::Write;
                let path = dir.join("engine-warnings.log");
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                {
                    let timestamp = chrono::Utc::now().to_rfc3339();
                    let _ = writeln!(file, "[{timestamp}] {message}");
                }
            }
        }
    }
}

/// 校验文件 sha256 是否与内嵌清单一致；不一致或无法读取时记录警告（不阻断，LGPL 允许替换）。
fn warn_if_hash_mismatch(path: &Path, context: &str) {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    let Some(expected) = manifest_expectation(name) else {
        return;
    };
    match hash_matches(path, &expected) {
        Ok(true) => {}
        Ok(false) => persist_engine_warning(&format!("警告：{context} {} 的 sha256 与内嵌清单不一致，可能是用户自备或被篡改的引擎，已放行（LGPL 允许替换），请自行确认来源可信", path.display())),
        Err(error) => persist_engine_warning(&format!("警告：无法读取{context} {} 以校验 sha256：{error}", path.display())),
    }
}

/// 清理本应用引擎目录里崩溃残留的 `.part-*` 临时文件（write_atomic 的中间产物）。
/// 只匹配本应用命名模式（引擎文件名 + `.part-` + 进程号）且仅为文件时删除；
/// 用户自备文件与旧版本目录一律不动，避免误删。
fn cleanup_part_residue(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.filter_map(std::result::Result::ok) {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let is_residue = path.is_file()
            && ["7z.exe", "7z.dll", "7zz"].iter().any(|engine| {
                name.strip_prefix(engine)
                    .and_then(|rest| rest.strip_prefix(".part-"))
                    .is_some_and(|pid| !pid.is_empty() && pid.chars().all(|c| c.is_ascii_digit()))
            });
        if is_residue {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// 目录中已存在同名引擎文件时的口径：保留该文件，按内嵌清单比对哈希并记录警告。
fn keep_existing_engine_file(target: &Path, name: &str, expected: &str) {
    match hash_matches(target, expected) {
        Ok(true) => {}
        Ok(false) => {
            persist_engine_warning(&format!("警告：引擎目录中已存在与内嵌清单 sha256 不一致的 {name}（{}），保留该文件（用户自备引擎可覆盖内嵌副本），请自行确认来源可信", target.display()));
        }
        Err(error) => {
            persist_engine_warning(&format!(
                "警告：无法读取已存在的引擎文件 {} 以校验 sha256：{error}，保留该文件",
                target.display()
            ));
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
        // 已存在的文件一律保留：用户把自备引擎放在这里即可覆盖内嵌副本（LGPL 可替换要求），
        // 只有缺失时才从 EXE 释放并逐文件校验 sha256。
        // 但已存在文件也要计算 sha256 与清单比对——不一致时保留并记录警告，
        // 防止恶意预植文件在"已存在即跳过"逻辑下无声绕过内嵌校验。
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

/// 先写临时文件再改名，避免中断时留下半个可执行文件；改名用不覆盖语义，
/// 已存在的目标一律拒绝替换（用户自备文件优先，不用覆盖语义替换用户文件）。
fn write_atomic(target: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let directory = target.parent().context("引擎目录缺少父级")?;
    let temporary = directory.join(format!(
        "{}.part-{}",
        target
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("engine"),
        std::process::id()
    ));
    {
        let mut file = std::fs::File::create(&temporary)?;
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
    }
    match crate::fsutil::rename_noreplace(&temporary, target) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 未内嵌引擎时 embedded_available 必须返回 false（合法开发配置，不是失败）；
    /// 内嵌时返回 true，且与 FILES / MANIFEST 一致。
    // 覆盖 E-05
    #[test]
    fn embedded_available_matches_build_configuration() {
        if embedded_available() {
            assert!(
                !embedded::FILES.is_empty(),
                "embedded_available 为 true 时 FILES 不得为空"
            );
            assert!(
                !embedded::MANIFEST.is_empty(),
                "embedded_available 为 true 时 MANIFEST 不得为空"
            );
        } else {
            // 未运行 fetch-7zip.ps1 或引擎不完整：合法的开发配置，但必须在测试输出里可见，
            // 避免「无内嵌时静默空转」掩盖真实回归；CI 需另有带引擎的构建路径。
            eprintln!("warning: 本构建未内嵌 7-Zip 引擎（开发配置合法）；带引擎的 CI 构建须覆盖 release 校验路径");
            assert!(
                embedded::FILES.is_empty() || embedded::MANIFEST.is_empty(),
                "embedded_available() 与 FILES/MANIFEST 状态不一致"
            );
        }
    }

    /// 用户把自备引擎放进释放目录后，程序不得用内嵌副本覆盖它（LGPL 可替换要求）。
    // 覆盖 E-02
    #[test]
    fn release_never_overwrites_existing_files() {
        if !embedded_available() {
            eprintln!("warning: 未内嵌 7-Zip 引擎，跳过 release 不覆盖已有文件的测试；带引擎构建须在 CI 验证此路径");
            return;
        }
        let (name, _) = embedded::FILES
            .first()
            .copied()
            .expect("内嵌可用时至少有一个引擎文件");
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join(name);
        std::fs::write(&target, b"user supplied engine").unwrap();
        release(directory.path()).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"user supplied engine");
        std::fs::remove_file(&target).unwrap();
        release(directory.path()).unwrap();
        assert!(target.is_file(), "缺失的引擎文件应从内嵌副本释放");
    }

    /// 有内嵌时 release 必须写出与清单一致的文件（写入后逐文件 sha256 校验）。
    // 覆盖 E-02
    #[test]
    fn release_writes_files_matching_manifest_hashes() {
        if !embedded_available() {
            eprintln!("warning: 未内嵌 7-Zip 引擎，跳过 release 哈希校验测试；带引擎构建须在 CI 验证此路径");
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        release(directory.path()).unwrap();
        let manifest: serde_json::Value =
            serde_json::from_str(embedded::MANIFEST).expect("内嵌清单应是合法 JSON");
        let entries = manifest["files"]
            .as_array()
            .expect("内嵌清单应有 files 数组");
        assert!(!entries.is_empty());
        for entry in entries {
            let name = entry["name"].as_str().expect("清单条目应有 name");
            let expected = entry["sha256"]
                .as_str()
                .expect("清单条目应有 sha256")
                .to_lowercase();
            let path = directory.path().join(name);
            assert!(path.is_file(), "内嵌引擎文件 {name} 应被释放");
            assert!(
                hash_matches(&path, &expected).unwrap(),
                "释放后的 {name} 必须与清单 sha256 一致"
            );
        }
    }
}
