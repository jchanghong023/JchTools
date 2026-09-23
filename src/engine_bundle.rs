//! 7-Zip 引擎定位：随包目录优先，其次使用内嵌副本（首次使用时释放到用户数据目录）。
//!
//! 候选顺序（E-02）：
//! 1. `<exe 目录>/resources/7zip/`：发布包与开发环境的显式引擎；
//! 2. 已释放的内嵌副本：`<用户数据目录>/JchTools/engine/<版本-哈希>/` 及其下的
//!    `release-*` 独立释放目录；
//! 3. 从 EXE 内嵌的压缩数据释放：固定位置只缺文件时补齐；固定位置被无效文件占用时，
//!    释放到新的独立自有位置（`release-*` 子目录），不删占用文件。
//!
//! 每个候选必须在内嵌清单声明的全部文件上通过「存在 + sha256 一致」校验后才可使用；
//! 缺失或校验失败的候选绝不执行，只记录原因（stderr + engine-warnings.log）并继续下一顺位。
//! 构建未内嵌引擎时没有可校验的清单，随包目录退化为最小完整性检查
//! （主程序存在 + Windows 上 7z.dll 存在）。全部候选不可用时按 E-05 口径报错停止，
//! 错误信息逐候选给出原因。
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

/// 随包携带的引擎目录（`resources/7zip`），存在即返回；能否使用由候选校验决定。
pub fn bundled_dir() -> Option<PathBuf> {
    let directory = std::env::current_exe()
        .ok()?
        .parent()?
        .join("resources/7zip");
    let executable = directory.join(engine_name());
    executable.is_file().then_some(directory)
}

/// 内嵌引擎的固定释放目录；被无效文件占用时会在其下新开 `release-*` 独立释放目录。
/// 放入与内嵌清单不一致的自备引擎会导致该目录被候选校验拒绝（E-02：只认符合清单的候选）。
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

/// 返回可执行的 7-Zip 路径：随包目录 → 已释放的内嵌副本（含独立释放目录）→ 从 EXE 释放；
/// 每个候选逐文件校验通过后才可使用（E-02）。
pub fn resolve_executable() -> Result<PathBuf> {
    let bundled = bundled_dir();
    let embedded_base = embedded_dir();
    resolve_among(bundled.as_deref(), embedded_base.as_deref())
}

/// 在给定候选中按 E-02 顺序解析引擎（`bundled` = 随包目录，`embedded_base` = 内嵌释放目录）。
/// 抽出目录参数以便单元测试注入。每个候选必须在内嵌清单声明的全部文件上
/// 通过「存在 + sha256 一致」校验后才可使用；不可用的候选只记录原因并继续下一顺位，
/// 绝不执行、绝不删除或改写其中的文件（E-02：不自动覆盖现有文件）。
fn resolve_among(bundled: Option<&Path>, embedded_base: Option<&Path>) -> Result<PathBuf> {
    let mut failures: Vec<String> = Vec::new();

    // 候选 1：EXE 同目录的随包引擎目录。
    if let Some(directory) = bundled {
        let problems = candidate_problems(directory);
        if problems.is_empty() {
            return Ok(directory.join(engine_name()));
        }
        reject_candidate("随包 7-Zip 引擎", directory, &problems);
        failures.push(candidate_failure_line("随包引擎", directory, &problems));
    } else {
        failures.push(format!(
            "随包引擎：EXE 同目录不存在含 {} 的 resources/7zip",
            engine_name()
        ));
    }

    // 候选 2/3 都依赖内嵌引擎；构建未内嵌时到此为止（E-05）。
    if !embedded_available() {
        let mut message = format!(
            "未找到 7-Zip 引擎：resources/7zip 里没有 {}, 本构建也没有内嵌引擎。请运行 scripts/fetch-7zip.ps1 获取官方完整引擎后重新构建。",
            engine_name()
        );
        if !failures.is_empty() {
            message.push_str("\n各候选不可用的原因：\n- ");
            message.push_str(&failures.join("\n- "));
        }
        bail!(message);
    }
    let base = embedded_base.context("无法确定用户数据目录，不能释放内嵌的 7-Zip 引擎")?;

    // 候选 2a：固定释放位置（已释放的内嵌副本）。
    let mut problems = candidate_problems(base);
    if problems.is_empty() {
        return Ok(base.join(engine_name()));
    }
    reject_candidate("已释放的内嵌引擎", base, &problems);
    failures.push(candidate_failure_line("已释放的内嵌引擎", base, &problems));

    // 候选 2b：此前因固定位置被占用而新开的独立释放目录（release-*），按目录名顺序取第一个可用者。
    for directory in existing_release_dirs(base) {
        let problems = candidate_problems(&directory);
        if problems.is_empty() {
            return Ok(directory.join(engine_name()));
        }
        reject_candidate("已释放的内嵌引擎", &directory, &problems);
        failures.push(candidate_failure_line(
            "已释放的内嵌引擎",
            &directory,
            &problems,
        ));
    }

    // 候选 3a：固定位置只缺文件时从 EXE 补齐释放（已存在文件一律不覆盖），补齐后复检。
    if problems.iter().any(|problem| problem.missing) {
        if let Err(error) = release(base) {
            failures.push(format!("向固定位置释放内嵌引擎失败：{error:#}"));
        }
        cleanup_part_residue(base);
        problems = candidate_problems(base);
        if problems.is_empty() {
            return Ok(base.join(engine_name()));
        }
        // 仍无效：固定位置存在不可覆盖的无效文件，转新的独立位置（下方）。
    }

    // 候选 3b：固定位置被无效文件占用时，释放到新的独立自有位置（不删占用文件），仍校验后使用。
    match fresh_release_dir(base).and_then(|directory| release(&directory).map(|()| directory)) {
        Ok(directory) => {
            cleanup_part_residue(&directory);
            let problems = candidate_problems(&directory);
            if problems.is_empty() {
                return Ok(directory.join(engine_name()));
            }
            reject_candidate("新释放的内嵌引擎", &directory, &problems);
            failures.push(candidate_failure_line(
                "新释放的内嵌引擎",
                &directory,
                &problems,
            ));
        }
        Err(error) => failures.push(format!("新位置释放内嵌引擎失败：{error:#}")),
    }

    bail!(
        "没有可用的 7-Zip 引擎（所有候选均未通过校验，按 E-05 停止）：\n- {}",
        failures.join("\n- ")
    )
}

/// 候选目录的单个校验问题；`missing` 为 true 表示文件缺失（可从 EXE 重新释放补齐），
/// 为 false 表示文件存在但无效（哈希不一致或不可读），重新释放也不得覆盖它。
struct Problem {
    description: String,
    missing: bool,
}

fn join_problems(problems: &[Problem]) -> String {
    problems
        .iter()
        .map(|problem| problem.description.as_str())
        .collect::<Vec<_>>()
        .join("；")
}

/// 候选因校验问题被拒绝时记录原因（stderr + engine-warnings.log）。
/// 拒绝只表示「不使用该候选」：其中的文件一律不删除、不改写。
fn reject_candidate(context: &str, directory: &Path, problems: &[Problem]) {
    persist_engine_warning(&format!(
        "已拒绝使用{context}（{}）：{}；按候选顺序尝试下一顺位",
        directory.display(),
        join_problems(problems)
    ));
}

/// 汇总进最终报错的单个候选原因行（哪份引擎、哪个文件、缺失还是哈希不一致）。
fn candidate_failure_line(context: &str, directory: &Path, problems: &[Problem]) -> String {
    format!(
        "{context}（{}）：{}",
        directory.display(),
        join_problems(problems)
    )
}

/// 本目标平台必需的引擎文件（文件名 + 期望 sha256，来自内嵌清单）。
/// 以本构建实际内嵌的文件（embedded::FILES）为准：交叉编译时清单里可能同时有其他平台的条目，
/// 那些不参与校验。构建未内嵌引擎时返回 None（候选退化为最小完整性检查）。
fn required_engine_files() -> Option<Vec<(String, String)>> {
    if !embedded_available() {
        return None;
    }
    let mut files = Vec::new();
    for (name, _) in embedded::FILES {
        let expected = manifest_expectation(name)?;
        files.push(((*name).to_string(), expected));
    }
    (!files.is_empty()).then_some(files)
}

/// 按必需清单逐文件校验候选目录（存在 + sha256 一致），返回问题列表（空 = 可用）。
/// 本函数只读：绝不修改候选目录里的文件；「不覆盖占用文件」的释放策略由 resolve_among 处理。
fn candidate_problems(directory: &Path) -> Vec<Problem> {
    let mut problems = Vec::new();
    if let Some(files) = required_engine_files() {
        for (name, expected) in files {
            let path = directory.join(&name);
            if !path.is_file() {
                problems.push(Problem {
                    description: format!("缺少 {name}"),
                    missing: true,
                });
                continue;
            }
            match hash_matches(&path, &expected) {
                Ok(true) => {}
                Ok(false) => problems.push(Problem {
                    description: format!("{name} 的 sha256 与内嵌清单不一致"),
                    missing: false,
                }),
                Err(error) => problems.push(Problem {
                    description: format!("{name} 无法读取以校验 sha256：{error}"),
                    missing: false,
                }),
            }
        }
    } else {
        // 无内嵌清单（构建未内嵌引擎）：没有可校验的清单，退化为最小完整性检查。
        // Windows 上 7z.dll 缺失不是「用户替换了引擎」，而是不完整引擎（例如只拷了 exe）。
        let name = engine_name();
        if !directory.join(name).is_file() {
            problems.push(Problem {
                description: format!("缺少 {name}"),
                missing: true,
            });
        }
        #[cfg(windows)]
        {
            if !directory.join("7z.dll").is_file() {
                problems.push(Problem {
                    description: "缺少 7z.dll（引擎不完整）".to_string(),
                    missing: true,
                });
            }
        }
    }
    problems
}

/// base 下已有的独立释放目录（release-* 前缀），按目录名排序保证顺序确定。
fn existing_release_dirs(base: &Path) -> Vec<PathBuf> {
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
fn fresh_release_dir(base: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(base)
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

/// 把引擎校验警告/拒绝记录追加写入用户数据目录下的 `engine-warnings.log`。
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

/// 目录中已存在同名引擎文件时的口径：保留该文件（绝不覆盖），按内嵌清单比对哈希并记录警告。
/// 与清单不一致时只记录、不替换；所在目录能否被使用由 resolve_among 的逐文件校验决定。
fn keep_existing_engine_file(target: &Path, name: &str, expected: &str) {
    match hash_matches(target, expected) {
        Ok(true) => {}
        Ok(false) => {
            persist_engine_warning(&format!("警告：引擎目录中已存在与内嵌清单 sha256 不一致的 {name}（{}），保留该文件（不覆盖既有文件），该目录能否使用以逐文件校验为准", target.display()));
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

    /// 在临时目录里用内嵌副本释放出一份「与清单一致」的引擎目录，充当合法随包目录。
    fn valid_engine_dir() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        release(directory.path()).unwrap();
        directory
    }

    /// 往目录里写入与清单不一致的引擎文件（主程序必写；Windows 上补一份坏 7z.dll）。
    fn write_tampered_engine(directory: &Path) {
        std::fs::write(directory.join(engine_name()), b"tampered engine bytes").unwrap();
        #[cfg(windows)]
        std::fs::write(directory.join("7z.dll"), b"tampered engine bytes").unwrap();
    }

    /// 断言路径是引擎文件且与内嵌清单一致。
    fn assert_valid_executable(path: &Path) {
        assert!(
            hash_matches(path, &manifest_expectation(engine_name()).unwrap()).unwrap(),
            "{} 应与内嵌清单 sha256 一致",
            path.display()
        );
    }

    /// 候选均不可用时的最终报错必须包含各候选的原因（哪份引擎、哪个文件、什么问题）。
    // 覆盖 E-02 / E-05
    #[test]
    fn resolve_reports_per_candidate_reasons_when_all_candidates_invalid() {
        if !embedded_available() {
            eprintln!("warning: 未内嵌 7-Zip 引擎，跳过全候选不可用报错测试；带引擎构建须在 CI 验证此路径");
            return;
        }
        let bundled = tempfile::tempdir().unwrap();
        write_tampered_engine(bundled.path());
        // 内嵌释放基目录落在一个普通文件之下：释放必然失败，模拟「新位置也无法产出可用引擎」。
        let base = tempfile::tempdir().unwrap();
        let blocked = base.path().join("occupied.txt");
        std::fs::write(&blocked, b"occupied").unwrap();
        let embedded_base = blocked.join("engine");
        let error = resolve_among(Some(bundled.path()), Some(&embedded_base)).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("随包"),
            "报错应说明随包候选被拒：{message}"
        );
        assert!(
            message.contains("sha256 与内嵌清单不一致"),
            "报错应给出具体文件与不一致原因：{message}"
        );
        assert!(
            message.contains(&bundled.path().display().to_string()),
            "报错应指出是哪份随包引擎：{message}"
        );
    }

    /// 已释放位置存在哈希不一致的引擎文件时：不得使用、不得覆盖或删除，
    /// 必须释放到新的独立自有位置，校验通过后才使用（自愈）。
    // 覆盖 E-02
    #[test]
    fn resolve_releases_to_fresh_location_when_existing_file_hash_mismatches() {
        if !embedded_available() {
            eprintln!(
                "warning: 未内嵌 7-Zip 引擎，跳过新位置释放测试；带引擎构建须在 CI 验证此路径"
            );
            return;
        }
        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(base.path()).unwrap();
        std::fs::write(base.path().join(engine_name()), b"corrupted engine bytes").unwrap();
        let path = resolve_among(None, Some(base.path())).unwrap();
        let released_dir = path.parent().expect("返回路径应有父目录");
        assert_eq!(
            released_dir.parent(),
            Some(base.path()),
            "新释放位置应是内嵌释放目录下的独立子目录"
        );
        assert!(
            released_dir
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("release-")),
            "独立释放目录应以 release- 命名：{}",
            released_dir.display()
        );
        assert_valid_executable(&path);
        // 被占用的无效文件不得删除或改写。
        assert_eq!(
            std::fs::read(base.path().join(engine_name())).unwrap(),
            b"corrupted engine bytes",
            "固定释放位置的无效文件必须原样保留"
        );
    }

    /// 随包目录校验不可用（哈希不一致）时：拒绝该候选并回退到内嵌释放，而不是警告后照常执行。
    // 覆盖 E-02
    #[test]
    fn resolve_falls_back_to_embedded_when_bundled_dir_hash_mismatches() {
        if !embedded_available() {
            eprintln!("warning: 未内嵌 7-Zip 引擎，跳过随包回退测试；带引擎构建须在 CI 验证此路径");
            return;
        }
        let bundled = tempfile::tempdir().unwrap();
        write_tampered_engine(bundled.path());
        let base = tempfile::tempdir().unwrap();
        let path = resolve_among(Some(bundled.path()), Some(base.path())).unwrap();
        assert_ne!(
            path.parent(),
            Some(bundled.path()),
            "不得使用与清单不一致的随包引擎"
        );
        assert_valid_executable(&path);
        assert_eq!(
            std::fs::read(bundled.path().join(engine_name())).unwrap(),
            b"tampered engine bytes",
            "随包目录里的文件必须原样保留"
        );
    }

    /// Windows 随包目录缺 7z.dll（不完整引擎）时：该候选不可用并回退内嵌，
    /// 而不是整体报错中断、放弃后续候选。
    // 覆盖 E-02
    #[test]
    fn resolve_falls_back_when_bundled_dir_missing_dll() {
        if !embedded_available() {
            eprintln!(
                "warning: 未内嵌 7-Zip 引擎，跳过随包缺 dll 回退测试；带引擎构建须在 CI 验证此路径"
            );
            return;
        }
        let bundled = tempfile::tempdir().unwrap();
        std::fs::write(bundled.path().join(engine_name()), b"only main exe").unwrap();
        let base = tempfile::tempdir().unwrap();
        let path = resolve_among(Some(bundled.path()), Some(base.path())).unwrap();
        assert_ne!(
            path.parent(),
            Some(bundled.path()),
            "不完整的随包引擎目录不得被使用"
        );
        assert_valid_executable(&path);
    }

    /// 合法随包目录仍然最优先，行为不变。
    // 覆盖 E-02
    #[test]
    fn resolve_prefers_valid_bundled_dir() {
        if !embedded_available() {
            eprintln!("warning: 未内嵌 7-Zip 引擎，跳过随包优先测试；带引擎构建须在 CI 验证此路径");
            return;
        }
        let bundled = valid_engine_dir();
        let base = tempfile::tempdir().unwrap();
        let path = resolve_among(Some(bundled.path()), Some(base.path())).unwrap();
        assert_eq!(path, bundled.path().join(engine_name()));
    }

    /// 构建未内嵌引擎时的候选口径：最小完整性检查（主程序存在 + Windows 上 7z.dll 存在）。
    // 覆盖 E-02 / E-05
    #[test]
    fn resolve_without_embedded_uses_minimal_bundled_checks() {
        if embedded_available() {
            return; // 仅无内嵌构建执行（该配置合法，见 embedded_available_matches_build_configuration）
        }
        let bundled = tempfile::tempdir().unwrap();
        std::fs::write(bundled.path().join(engine_name()), b"user supplied engine").unwrap();
        #[cfg(windows)]
        std::fs::write(bundled.path().join("7z.dll"), b"user supplied engine").unwrap();
        let base = tempfile::tempdir().unwrap();
        let path = resolve_among(Some(bundled.path()), Some(base.path())).unwrap();
        assert_eq!(path, bundled.path().join(engine_name()));
    }

    /// 构建未内嵌引擎且随包目录不完整（Windows 缺 7z.dll）时：显式报错并说明原因。
    // 覆盖 E-02 / E-05
    #[test]
    fn resolve_without_embedded_reports_incomplete_bundled_dir() {
        if embedded_available() {
            return; // 仅无内嵌构建执行
        }
        let bundled = tempfile::tempdir().unwrap();
        std::fs::write(bundled.path().join(engine_name()), b"only main exe").unwrap();
        let error = resolve_among(Some(bundled.path()), None).unwrap_err();
        let message = format!("{error:#}");
        #[cfg(windows)]
        assert!(
            message.contains("7z.dll"),
            "报错应说明随包引擎缺少 7z.dll：{message}"
        );
    }
}
