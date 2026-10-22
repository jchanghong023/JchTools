use super::*;

// ===== 合同对齐回归（C-02 / R-01 / E-05）：缺陷修复前必须失败 =====
// 覆盖 C-02（MUST NOT 提供同名不同内容的淘汰开关）
#[test]
fn config_has_no_version_elimination_switches() {
    let value = serde_json::to_value(Config::default()).unwrap();
    for key in [
        "same_name_same_size",
        "same_size_keep",
        "same_name_different_size",
        "different_size_keep",
        "conflict_scope_directory",
    ] {
        assert!(
            value.get(key).is_none(),
            "C-02：配置不得提供版本取舍开关：{key}"
        );
    }
}
// 覆盖 R-01, C-02
#[test]
fn rules_table_has_no_version_or_removed_switches() {
    let schema: serde_json::Value =
        serde_json::from_str(include_str!("../../resources/rules.json")).unwrap();
    let rows = schema.as_array().unwrap();
    // R-01/C-08：规则面板不得再出现已移除的开关行，且三类清理的删除方式必须可独立配置。
    for key in [
        "same_name_same_size",
        "same_size_keep",
        "conflict_scope_directory",
        "archive_delete",
        "extract_conflict",
        "conflict_delete",
        "clean_empty_dirs",
        "nested_archives",
        "cleanup_delete",
    ] {
        assert!(
            rows.iter().all(|r| r["key"].as_str() != Some(key)),
            "规则面板不得提供已移除的开关行：{key}"
        );
    }
    for key in ["junk_delete", "temp_delete", "zero_delete"] {
        assert!(
            rows.iter().any(|r| r["key"].as_str() == Some(key)),
            "C-08：三类清理各自独立覆盖删除方式，规则面板必须有对应行：{key}"
        );
    }
}

// ===== 核心公共接口缺口补测（分卷识别）=====
// 覆盖 X-01, X-06（分卷识别口径）
#[test]
fn multipart_name_covers_volume_detection_corners() {
    assert!(rules::multipart_name("x.part1.rar"));
    assert!(rules::multipart_name("x.part99.rar"));
    assert!(!rules::multipart_name("x.rar"));
    assert!(rules::multipart_name("x.7z.001"));
    assert!(!rules::multipart_name("x.7z.002"));
}

// 覆盖 X-01：自动解压只认用户意义上的归档文件（白名单）；安装资源、系统镜像、
// 语言包、安装包、文档容器即使 7-Zip 能打开，也不得自动解压。
#[test]
fn archive_name_only_accepts_the_user_archive_whitelist() {
    for name in [
        "a.zip",
        "a.7Z",
        "b.tar",
        "b.gz",
        "b.bz2",
        "b.xz",
        "b.zst",
        "b.lzma",
        "b.z",
        "b.tgz",
        "b.tbz2",
        "b.txz",
        "b.tzst",
        "f.tar.gz",
        "f.tar.bz2",
        "f.tar.xz",
        "f.tar.zst",
        "f.tar.lzma",
        "f.tar.z",
        "d.7z.001",
        "d.zip.001",
        "e.part1.rar",
    ] {
        assert!(
            rules::archive_name(name),
            "{name} 在白名单内，应识别为压缩包"
        );
    }
    for name in [
        "a.txt",
        "a.png",
        "a.zipx",
        "e.part2.rar",
        "archive",
        // 引擎没有解码器的流格式：放行只会把文件一律推进「解压失败」，故不碰
        "b.lz4",
        "b.lz",
        "b.br",
        "f.tar.lz4",
        "f.tar.lz",
        "f.tar.br",
        // X-09：白名单外格式的分卷与配不上主包的孤立编号文件同样不碰
        "windows.iso.001",
        "report.docx.001",
        "setup.msi.001",
        "data.001",
        "report.z01",
        "report.r00",
        // Windows 安装资源与安装包
        "data.cab",
        "setup.msi",
        "app.msix",
        "bundle.msixbundle",
        // 磁盘 / 系统镜像
        "windows.iso",
        "boot.wim",
        "install.esd",
        "disk.img",
        "disk.vhd",
        "disk.vhdx",
        // Java / Android / Python / .NET / 扩展包
        "library.jar",
        "service.war",
        "module.ear",
        "app.apk",
        "app.aab",
        "wheel.whl",
        "pack.nupkg",
        "ext.vsix",
        "addon.crx",
        "addon.xpi",
        // Apple 软件包与旧式容器
        "app.ipa",
        "installer.pkg",
        "disk.dmg",
        "legacy.lzh",
        "archive.cpio",
        // Office / OpenDocument / 电子书容器
        "report.docx",
        "sheet.xlsx",
        "slides.pptx",
        "macro.docm",
        "macro.xlsm",
        "doc.odt",
        "book.epub",
        // 可执行文件与自解压包
        "setup.exe",
        "self-extract.com",
    ] {
        assert!(
            !rules::archive_name(name),
            "{name} 不在自动解压白名单内，不得被当作压缩包处理"
        );
    }
}

// 覆盖 X-07（隔离目录中的包被用户移出后，下次扫描自然重试）
#[test]
fn quarantine_move_out_is_rescanned() {
    let f = Fixture::new();
    f.write("解压失败/fix.zip", b"not a zip", 10);
    let cfg = base();
    assert_eq!(
        jchtools_recursive_extract::engine::count_archives(&f.root, &cfg).unwrap(),
        0,
        "隔离目录内的包不计入确认框计数"
    );
    fs::rename(f.root.join("解压失败/fix.zip"), f.root.join("fix.zip")).unwrap();
    assert_eq!(
        jchtools_recursive_extract::engine::count_archives(&f.root, &cfg).unwrap(),
        1,
        "用户补救（移出隔离目录）后下次扫描自然重试"
    );
}

// 覆盖 R-03, S-04（性能默认值：哈希线程 6、磁盘预留 1 GiB、递归默认包含子目录、默认不缩小范围）
#[test]
fn performance_defaults_match_contract() {
    let cfg = Config::default();
    assert_eq!(cfg.hash_workers, 6, "R-03：默认 6（1–16 可配）");
    assert_eq!(
        cfg.reserve_bytes,
        1024 * 1024 * 1024,
        "磁盘预留默认 1 GiB（字节口径）"
    );
    assert!(cfg.recursive);
    assert!(cfg.include_hidden, "S-04：默认覆盖隐藏资料");
    assert!(cfg.include_system, "S-04：默认覆盖系统属性资料");
    assert_eq!(
        cfg.exclusions, "",
        "S-04：默认不设 glob 排除；Git 整树排除由引擎无条件执行，不靠默认排除表"
    );
}
// 覆盖 C-07, H-07, X-04, X-05, R-02（已移除的开关不得复活）
#[test]
fn removed_extraction_and_cleanup_switches_are_gone() {
    let value = serde_json::to_value(Config::default()).unwrap();
    for key in [
        "archive_delete",
        "extract_conflict",
        "conflict_delete",
        "clean_empty_dirs",
        "nested_archives",
        "cleanup_delete",
        // 2026-09 归类引擎重写（R-02/R-04）删除的键：旧任务库 JSON 剥除后加载，
        // 不得在配置里复活。
        "classify",
        "output_dir",
        "preserve_structure",
        "custom_categories",
        "merge_directories",
        "flatten_single_child",
        "duplicate_action",
        "duplicate_delete",
        "max_unpacked_gib",
        "max_file_gib",
        "large_threshold_gib",
        "reserve_gib",
    ] {
        assert!(value.get(key).is_none(), "已移除的配置不得复活：{key}");
    }
    for key in ["junk_delete", "temp_delete", "zero_delete"] {
        assert!(
            value.get(key).is_some(),
            "C-08：三类清理必须各自独立覆盖删除方式：{key}"
        );
    }
}

// 覆盖 S-02（旧任务库里的 recycle 取值按永久删除读取，不得让旧配置整体判非法）
#[test]
fn legacy_recycle_mode_loads_as_permanent() {
    let mut value = serde_json::to_value(Config::default()).unwrap();
    // duplicate_delete 已删除（C-04：副本处置随全局）；回收站历史值只可能出现在
    // 全局与三类清理的删除方式键上。
    for key in ["global_delete", "junk_delete", "temp_delete", "zero_delete"] {
        value[key] = serde_json::json!("recycle");
    }
    let cfg = Config::from_json_text(&value.to_string()).unwrap();
    assert_eq!(cfg.global_delete, DeleteMode::Permanent);
    assert_eq!(
        cfg.junk_delete.resolve(cfg.global_delete),
        DeleteMode::Permanent
    );
    assert_eq!(
        cfg.temp_delete.resolve(cfg.global_delete),
        DeleteMode::Permanent
    );
    assert_eq!(
        cfg.zero_delete.resolve(cfg.global_delete),
        DeleteMode::Permanent
    );
}
