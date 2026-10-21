use super::locate::{engine_name, manifest_expectation, resolve_among};
use super::release::{existing_release_dirs, fresh_release_dir, hash_matches, write_atomic};
use super::*;
use sha2::{Digest, Sha256};
use std::path::Path;

use super::*;

// 覆盖 E-02：固定释放目录本身被文件占用时，使用同级独立目录，不删除占用文件。
#[test]
fn resolve_uses_sibling_release_when_fixed_directory_is_a_file() {
    if !embedded_available() {
        eprintln!("warning: 未内嵌 7-Zip，固定目录被文件占用的回归未执行");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let base = root.path().join("fixed-engine");
    std::fs::write(&base, b"user owned file").unwrap();
    let executable = resolve_among(None, Some(&base)).unwrap();
    assert_valid_executable(&executable);
    assert_eq!(
        executable.parent().unwrap().parent().unwrap(),
        root.path().join("fixed-engine-releases")
    );
    assert_eq!(std::fs::read(&base).unwrap(), b"user owned file");
    assert_eq!(
        resolve_among(None, Some(&base)).unwrap(),
        executable,
        "后续启动须复用已校验的同级独立副本"
    );
}

// 覆盖 E-02：无内嵌构建也能验证固定目录占用时的独立释放路径规划与复用发现。
#[test]
fn independent_release_preserves_fixed_directory_occupier() {
    let root = tempfile::tempdir().unwrap();
    let base = root.path().join("fixed-engine");
    std::fs::write(&base, b"user owned file").unwrap();
    let first = fresh_release_dir(&base).unwrap();
    let second = fresh_release_dir(&base).unwrap();
    assert_ne!(first, second);
    assert_eq!(
        first.parent().unwrap(),
        root.path().join("fixed-engine-releases")
    );
    let mut expected = vec![first, second];
    expected.sort();
    assert_eq!(existing_release_dirs(&base), expected);
    assert_eq!(std::fs::read(base).unwrap(), b"user owned file");
}

/// 释放临时文件不得覆盖碰巧使用旧命名格式的既有文件。
// 覆盖 E-02
#[test]
fn write_atomic_preserves_preexisting_part_file() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join(engine_name());
    let sentinel = directory
        .path()
        .join(format!("{}.part-{}", engine_name(), std::process::id()));
    std::fs::write(&sentinel, b"user owned temporary file").unwrap();
    write_atomic(&target, b"new engine").unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"new engine");
    assert_eq!(
        std::fs::read(&sentinel).unwrap(),
        b"user owned temporary file",
        "释放不得改写或移除既有临时文件"
    );
}

/// 候选目录中的旧命名临时文件不因名称匹配而获得删除授权。
// 覆盖 E-02
#[test]
fn resolve_preserves_unowned_part_files() {
    if !embedded_available() {
        eprintln!("warning: 未内嵌 7-Zip，引擎释放的临时文件所有权用例未执行");
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let sentinel = directory.path().join("7z.exe.part-12345");
    std::fs::write(&sentinel, b"user owned temporary file").unwrap();
    let executable = resolve_among(None, Some(directory.path())).unwrap();
    assert_valid_executable(&executable);
    assert_eq!(
        std::fs::read(&sentinel).unwrap(),
        b"user owned temporary file",
        "候选解析不得按名称清理无所有权文件"
    );
}

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
        eprintln!(
            "warning: 未内嵌 7-Zip 引擎，跳过 release 哈希校验测试；带引擎构建须在 CI 验证此路径"
        );
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
fn write_external_manifest(directory: &Path, executable: &[u8], library: &[u8]) {
    let digest = |bytes: &[u8]| hex::encode(Sha256::digest(bytes));
    let manifest = serde_json::json!({
        "version": "26.03",
        "upstream": "https://www.7-zip.org/",
        "installer": {
            "name": "7z2603-x64.msi",
            "sha256": "a".repeat(64),
            "url": "https://github.com/ip7z/7zip/releases/download/26.03/7z2603-x64.msi"
        },
        "source": {
            "name": "7z2603-src.tar.xz",
            "sha256": "b".repeat(64),
            "url": "https://github.com/ip7z/7zip/releases/download/26.03/7z2603-src.tar.xz"
        },
        "files": [
            {"name": engine_name(), "sha256": digest(executable)},
            {"name": "7z.dll", "sha256": digest(library)}
        ]
    });
    std::fs::write(
        directory.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
}

/// 构建未内嵌引擎时，只接受随包目录中声明并校验通过的清单。
// 覆盖 E-01 / E-02 / E-05
#[test]
fn resolve_without_embedded_validates_bundled_manifest() {
    if embedded_available() {
        return; // 仅无内嵌构建执行（该配置合法，见 embedded_available_matches_build_configuration）
    }
    let bundled = tempfile::tempdir().unwrap();
    let executable = b"external 7-Zip executable";
    let library = b"external 7-Zip library";
    std::fs::write(bundled.path().join(engine_name()), executable).unwrap();
    std::fs::write(bundled.path().join("7z.dll"), library).unwrap();
    write_external_manifest(bundled.path(), executable, library);
    let path = resolve_among(Some(bundled.path()), None).unwrap();
    assert_eq!(path, bundled.path().join(engine_name()));
    let expected = hex::encode(Sha256::digest(executable));
    assert!(
        hash_matches(&path, &expected).unwrap(),
        "通过外部清单校验的引擎应保留其声明字节"
    );

    std::fs::write(&path, b"modified executable").unwrap();
    let error = resolve_among(Some(bundled.path()), None).unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("sha256 与清单不一致"),
        "与外部清单不匹配的引擎必须被拒绝：{message}"
    );
}

/// 外部清单声明 Windows 必需文件但文件缺失时，错误须指出缺失成员。
// 覆盖 E-01 / E-02 / E-05
#[test]
fn resolve_without_embedded_reports_missing_manifest_member() {
    if embedded_available() {
        return; // 仅无内嵌构建执行
    }
    let bundled = tempfile::tempdir().unwrap();
    let executable = b"only main exe";
    std::fs::write(bundled.path().join(engine_name()), executable).unwrap();
    write_external_manifest(bundled.path(), executable, b"missing library");
    let error = resolve_among(Some(bundled.path()), None).unwrap_err();
    let message = format!("{error:#}");
    #[cfg(windows)]
    assert!(
        message.contains("7z.dll"),
        "报错应说明随包清单声明的 7z.dll 缺失：{message}"
    );
}

/// 候选均不可用时的最终报错必须包含各候选的原因（哪份引擎、哪个文件、什么问题）。
// 覆盖 E-02 / E-05
#[test]
fn resolve_reports_per_candidate_reasons_when_all_candidates_invalid() {
    if !embedded_available() {
        eprintln!(
            "warning: 未内嵌 7-Zip 引擎，跳过全候选不可用报错测试；带引擎构建须在 CI 验证此路径"
        );
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
        message.contains("7z.exe 的 sha256 与清单不一致"),
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
        eprintln!("warning: 未内嵌 7-Zip 引擎，跳过新位置释放测试；带引擎构建须在 CI 验证此路径");
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

/// 构建未内嵌引擎且随包目录缺少清单时：即使主程序与 7z.dll 齐备也拒绝
///（E-02：候选必须按清单逐文件校验，不存在「最小检查」旁路）。
// 覆盖 E-02 / E-05
#[test]
fn resolve_without_embedded_rejects_bundled_dir_without_manifest() {
    if embedded_available() {
        return; // 仅无内嵌构建执行（该配置合法，见 embedded_available_matches_build_configuration）
    }
    let bundled = tempfile::tempdir().unwrap();
    std::fs::write(bundled.path().join(engine_name()), b"user supplied engine").unwrap();
    #[cfg(windows)]
    std::fs::write(bundled.path().join("7z.dll"), b"user supplied engine").unwrap();
    let error = resolve_among(Some(bundled.path()), None).unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("无法验证 7-Zip 清单"),
        "报错应说明随包引擎清单不可验证：{message}"
    );
}

/// 构建未内嵌引擎且随包清单声明的主程序缺失时：显式报错并说明缺失成员。
// 覆盖 E-02 / E-05
#[test]
fn resolve_without_embedded_reports_incomplete_bundled_dir() {
    if embedded_available() {
        return; // 仅无内嵌构建执行
    }
    let bundled = tempfile::tempdir().unwrap();
    let library = b"only 7z.dll";
    std::fs::write(bundled.path().join("7z.dll"), library).unwrap();
    write_external_manifest(bundled.path(), b"missing executable", library);
    let error = resolve_among(Some(bundled.path()), None).unwrap_err();
    let message = format!("{error:#}");
    #[cfg(windows)]
    assert!(
        message.contains("缺少 7z.exe"),
        "报错应说明随包引擎缺少清单声明的主程序：{message}"
    );
}
