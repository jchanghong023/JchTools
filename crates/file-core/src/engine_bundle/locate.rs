use super::embedded;
use super::release::{existing_release_dirs, fresh_release_dir, hash_matches, release};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

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

pub(super) fn engine_name() -> &'static str {
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

/// 校验单个候选目录：全部文件通过「存在 + sha256 一致」校验则返回引擎路径；
/// 否则记录拒绝原因并压入汇总失败清单，返回 None（E-02：绝不执行、绝不删除
/// 或改写候选中的文件）。拒绝文案与失败行文案分别由两个标签参数逐字提供。
fn vet_candidate(
    reject_label: &str,
    fail_label: &str,
    directory: &Path,
    problems: &[Problem],
    failures: &mut Vec<String>,
) -> Option<PathBuf> {
    if problems.is_empty() {
        return Some(directory.join(engine_name()));
    }
    reject_candidate(reject_label, problems);
    failures.push(candidate_failure_line(fail_label, directory, problems));
    None
}

/// 在给定候选中按 E-02 顺序解析引擎（`bundled` = 随包目录，`embedded_base` = 内嵌释放目录）。
/// 抽出目录参数以便单元测试注入。每个候选必须在内嵌清单声明的全部文件上
/// 通过「存在 + sha256 一致」校验后才可使用；不可用的候选只记录原因并继续下一顺位，
/// 绝不执行、绝不删除或改写其中的文件（E-02：不自动覆盖现有文件）。
pub(super) fn resolve_among(
    bundled: Option<&Path>,
    embedded_base: Option<&Path>,
) -> Result<PathBuf> {
    let started = std::time::Instant::now();
    let mut failures: Vec<String> = Vec::new();

    // 候选 1：EXE 同目录的随包引擎目录。
    if let Some(directory) = bundled {
        let problems = candidate_problems(directory);
        if let Some(path) = vet_candidate(
            "随包 7-Zip 引擎",
            "随包引擎",
            directory,
            &problems,
            &mut failures,
        ) {
            return Ok(path);
        }
    } else {
        failures.push(format!(
            "随包引擎：EXE 同目录不存在含 {} 的 resources/7zip",
            engine_name()
        ));
    }

    // 候选 2/3 都依赖内嵌引擎；构建未内嵌时到此为止（E-05）。
    if !embedded_available() {
        tracing::error!(
            component = "engine_bundle",
            event = "engine_resolution_failed",
            stage = "embedded_unavailable",
            error_type = "embedded_engine_unavailable",
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "没有可用的 7-Zip 引擎"
        );
        let mut message = String::from(
            "未找到可用的 7-Zip 引擎：随包候选不存在或未通过清单校验，本构建也没有内嵌引擎。请运行 scripts/fetch-7zip.ps1 获取官方完整引擎后重新构建。",
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
    if let Some(path) = vet_candidate(
        "已释放的内嵌引擎",
        "已释放的内嵌引擎",
        base,
        &problems,
        &mut failures,
    ) {
        return Ok(path);
    }

    // 候选 2b：此前因固定位置被占用而新开的独立释放目录（release-*），按目录名顺序取第一个可用者。
    for directory in existing_release_dirs(base) {
        let problems = candidate_problems(&directory);
        if let Some(path) = vet_candidate(
            "已释放的内嵌引擎",
            "已释放的内嵌引擎",
            &directory,
            &problems,
            &mut failures,
        ) {
            return Ok(path);
        }
    }

    // 候选 3a：固定位置只缺文件时从 EXE 补齐释放（已存在文件一律不覆盖），补齐后复检。
    if problems.iter().any(|problem| problem.missing) {
        let release_started = std::time::Instant::now();
        if let Err(error) = release(base) {
            tracing::warn!(
                component = "engine_bundle",
                event = "engine_embedded_release_failed",
                stage = "fixed_location",
                error_type = "release_error",
                elapsed_ms = jchtools_foundation::logging::elapsed_ms(release_started),
                "固定目录释放失败，将尝试独立目录回退"
            );
            failures.push(format!("向固定位置释放内嵌引擎失败：{error:#}"));
        }
        problems = candidate_problems(base);
        if problems.is_empty() {
            return Ok(base.join(engine_name()));
        }
        // 仍无效：固定位置存在不可覆盖的无效文件，转新的独立位置（下方）。
        // 注意：此处复检失败不记拒绝原因与失败行（保持既有行为），由 3b 的新位置收尾。
    }

    // 候选 3b：固定位置被无效文件占用时，释放到新的独立自有位置（不删占用文件），仍校验后使用。
    tracing::info!(
        component = "engine_bundle",
        event = "engine_embedded_release_fallback_started",
        stage = "independent_location",
        reason = "fixed_location_unusable",
        "固定释放位置不可用，开始独立目录回退"
    );
    let independent_release_started = std::time::Instant::now();
    match fresh_release_dir(base).and_then(|directory| release(&directory).map(|()| directory)) {
        Ok(directory) => {
            let problems = candidate_problems(&directory);
            if let Some(path) = vet_candidate(
                "新释放的内嵌引擎",
                "新释放的内嵌引擎",
                &directory,
                &problems,
                &mut failures,
            ) {
                return Ok(path);
            }
        }
        Err(error) => {
            tracing::error!(
                component = "engine_bundle",
                event = "engine_embedded_release_failed",
                stage = "independent_location",
                error_type = "release_or_directory_error",
                elapsed_ms = jchtools_foundation::logging::elapsed_ms(independent_release_started),
                "独立位置创建或释放失败，无法解析 7-Zip 引擎"
            );
            failures.push(format!("新位置释放内嵌引擎失败：{error:#}"));
        }
    }

    tracing::error!(
        component = "engine_bundle",
        event = "engine_resolution_failed",
        stage = "candidate_resolution",
        error_type = "no_usable_candidate",
        failure_count = failures.len(),
        elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
        "全部 7-Zip 候选均不可用"
    );
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

/// 候选因校验问题被拒绝时写入受控诊断日志。
/// 拒绝只表示「不使用该候选」：其中的文件一律不删除、不改写。
fn reject_candidate(context: &str, problems: &[Problem]) {
    tracing::warn!(
        component = "engine_bundle",
        event = "engine_candidate_rejected",
        candidate = context,
        stage = "candidate_validation",
        error_type = "candidate_validation_failed",
        problem_count = problems.len(),
        "7-Zip 候选未通过清单校验"
    );
}

/// 汇总进最终报错的单个候选原因行（哪份引擎、哪个文件、缺失还是哈希不一致）。
fn candidate_failure_line(context: &str, directory: &Path, problems: &[Problem]) -> String {
    format!(
        "{context}（{}）：{}",
        directory.display(),
        join_problems(problems)
    )
}

/// 确认未内嵌时使用的外部清单符合官方获取脚本生成的固定版本格式。
fn validate_external_manifest(manifest: &serde_json::Value) -> Result<()> {
    let version = manifest["version"]
        .as_str()
        .context("随包引擎清单缺少 version")?;
    let version_bytes = version.as_bytes();
    if version_bytes.len() != 5
        || version_bytes[0..2]
            .iter()
            .any(|byte| !byte.is_ascii_digit())
        || version_bytes[2] != b'.'
        || version_bytes[3..].iter().any(|byte| !byte.is_ascii_digit())
    {
        bail!("随包引擎清单的 version 格式无效");
    }
    if manifest["upstream"].as_str() != Some("https://www.7-zip.org/") {
        bail!("随包引擎清单的 upstream 不是官方地址");
    }

    let version_digits = version.replace('.', "");
    for (kind, expected_name) in [
        ("installer", format!("7z{version_digits}-x64.msi")),
        ("source", format!("7z{version_digits}-src.tar.xz")),
    ] {
        let asset = manifest[kind]
            .as_object()
            .with_context(|| format!("随包引擎清单缺少 {kind} 信息"))?;
        let name = asset["name"]
            .as_str()
            .with_context(|| format!("随包引擎清单缺少 {kind} 名称"))?;
        if name != expected_name.as_str() {
            bail!("随包引擎清单的 {kind} 名称与版本不匹配");
        }
        let expected_url =
            format!("https://github.com/ip7z/7zip/releases/download/{version}/{name}");
        if asset["url"].as_str() != Some(expected_url.as_str()) {
            bail!("随包引擎清单的 {kind} URL 不是对应官方 release");
        }
        let hash = asset["sha256"]
            .as_str()
            .with_context(|| format!("随包引擎清单缺少 {kind} 的 sha256"))?;
        if !valid_sha256(hash) {
            bail!("随包引擎清单中的 {kind} sha256 格式无效");
        }
    }
    Ok(())
}

fn valid_sha256(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// 本目标平台必需的引擎文件（文件名 + 期望 sha256）。
/// 内嵌构建以本构建实际内嵌的文件为准；未内嵌构建从随包目录的 `manifest.json`
/// 读取其期望摘要。两种来源均缺少清单或必需条目时都不能退化为仅存在性检查。
fn required_engine_files(directory: &Path) -> Result<Vec<(String, String)>> {
    if embedded_available() {
        let mut files = Vec::new();
        for (name, _) in embedded::FILES {
            let expected = manifest_expectation(name)
                .with_context(|| format!("内嵌清单缺少引擎文件 {name} 的 sha256"))?;
            files.push(((*name).to_string(), expected));
        }
        if files.is_empty() {
            bail!("内嵌引擎清单没有必需文件");
        }
        return Ok(files);
    }

    let manifest_path = directory.join("manifest.json");
    let manifest_text = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("读取随包引擎清单失败：{}", manifest_path.display()))?;
    let manifest: serde_json::Value = serde_json::from_str(&manifest_text)
        .with_context(|| format!("解析随包引擎清单失败：{}", manifest_path.display()))?;
    validate_external_manifest(&manifest)?;
    let entries = manifest["files"]
        .as_array()
        .context("随包引擎清单缺少 files 数组")?;
    let mut names = vec![engine_name()];
    #[cfg(windows)]
    names.push("7z.dll");

    let mut files = Vec::with_capacity(names.len());
    for required_name in names {
        let mut expected = None;
        for entry in entries {
            if entry["name"]
                .as_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(required_name))
            {
                if expected.is_some() {
                    bail!("随包引擎清单重复声明 {required_name}");
                }
                let hash = entry["sha256"]
                    .as_str()
                    .with_context(|| format!("随包引擎清单缺少 {required_name} 的 sha256"))?;
                if !valid_sha256(hash) {
                    bail!("随包引擎清单中的 {required_name} sha256 格式无效");
                }
                expected = Some(hash.to_ascii_lowercase());
            }
        }
        files.push((
            required_name.to_string(),
            expected.with_context(|| format!("随包引擎清单缺少必需文件 {required_name}"))?,
        ));
    }
    Ok(files)
}

/// 按必需清单逐文件校验候选目录（存在 + sha256 一致），返回问题列表（空 = 可用）。
/// 本函数只读：绝不修改候选目录里的文件；「不覆盖占用文件」的释放策略由 resolve_among 处理。
fn candidate_problems(directory: &Path) -> Vec<Problem> {
    let files = match required_engine_files(directory) {
        Ok(files) => files,
        Err(error) => {
            return vec![Problem {
                description: format!("无法验证 7-Zip 清单：{error:#}"),
                missing: false,
            }]
        }
    };
    let mut problems = Vec::new();
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
                description: format!("{name} 的 sha256 与清单不一致"),
                missing: false,
            }),
            Err(error) => problems.push(Problem {
                description: format!("{name} 无法读取以校验 sha256：{error}"),
                missing: false,
            }),
        }
    }
    problems
}

/// 在内嵌清单中查找 `file_name` 的期望 sha256（小写十六进制）；找不到时返回 None。
pub(super) fn manifest_expectation(file_name: &str) -> Option<String> {
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
