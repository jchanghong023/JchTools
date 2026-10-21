use super::*;

// 覆盖 X-01 / X-10 / 附录 A / 附录 B：首位点文件没有归档扩展名，
// 也不能凭该主体生成数字尾卷族；真正带扩展名的隐藏名称仍正常识别。
#[test]
fn extensionless_dotfiles_do_not_form_archive_families() {
    for name in [
        ".zip", ".7z", ".rar", ".tar", ".gz", ".bz2", ".xz", ".zst", ".lzma", ".z", ".tgz",
        ".tbz2", ".txz", ".tzst", ".zip.000", ".zip.001", ".7z.001", ".rar.002",
    ] {
        assert!(!archive_name(name), "{name} 没有白名单归档扩展名");
        assert!(archive_entry_key(name).is_none(), "{name} 不得计为归档入口");
        assert!(!multipart_name(name), "{name} 不属于归档分卷");
        assert_eq!(category_for(name), "其他", "{name}");
        assert!(
            numbered_volume_missing_entry_groups(&[name.to_string()]).is_empty(),
            "{name}"
        );
    }
    for name in [".hidden.zip", ".hidden.7z.001", ".tar.gz", ".tar.gz.001"] {
        assert!(archive_name(name), "{name} 具有真正的白名单扩展名");
        assert!(archive_entry_key(name).is_some(), "{name}");
        assert_eq!(category_for(name), "压缩包", "{name}");
    }
    assert_eq!(
        numbered_volume_missing_entry_groups(&[".hidden.7z.002".to_string()]),
        vec![".hidden.7z.002".to_string()]
    );
}

// 覆盖附录 A/B/C、X-10：完整后缀、无扩展名和 Unicode 排除边界。
#[test]
fn review_rule_volume_boundary() {
    assert!(archive_name("x.rar.001"));
    assert_eq!(
        numbered_volume_missing_entry_groups(&["x.rar.002".into()]),
        vec!["x.rar.002".to_string()]
    );
    assert!(archive_name("x.part١.rar"));
    assert!(!multipart_name("x.part١.rar"));
}

#[test]
fn review_rule_unbounded_part_number() {
    let name = "x.part18446744073709551616.rar".to_string();
    assert_eq!(
        part_rar_missing_first_groups(std::slice::from_ref(&name)),
        vec![name]
    );
}

#[test]
fn review_rule_zero_part_is_ordinary_rar() {
    for name in ["x.part0.rar", "x.part000.rar"] {
        assert!(archive_name(name));
        assert!(!multipart_name(name));
        assert!(part_rar_missing_first_groups(&[name.to_string()]).is_empty());
    }
}

#[test]
fn review_rule_ordinal_volume_groups() {
    assert_eq!(
        part_rar_missing_first_groups(&["σ.part2.rar".into(), "ς.part3.rar".into()]).len(),
        1,
    );
    assert_eq!(
        count_tail_only_old_style_groups(&["σ.rar".into(), "ς.r00".into()]),
        0,
    );
}

#[test]
fn review_rule_category_boundaries() {
    assert_eq!(category_for("a.part1.rar.txt"), "文档");
    assert_eq!(category_for(".jpg"), "其他");
}

// 覆盖 X-10, X-01, X-09, H-04：数字尾卷族 = 白名单后缀（含复合）+ 规范编号（最少三位）；
// 族入口是 .001（.000 为可识别的非法起始编号）；白名单外与孤立编号一律不碰。
#[test]
fn numbered_volume_family_recognition_matches_x10() {
    // 族入口：白名单任一后缀 + .001（含此前漏认的复合与非 7z/zip 后缀）。
    for name in [
        "x.7z.001",
        "x.zip.001",
        "x.tar.gz.001",
        "x.tar.bz2.001",
        "x.tar.001",
        "x.tgz.001",
        "x.gz.001",
        "X.ZST.001",
    ] {
        assert!(
            archive_name(name),
            "{name} 是白名单数字尾卷族的入口，应识别"
        );
    }
    // X-10：末尾 .000 是该族的非法起始编号，同样按族识别入队——引擎打不开错误
    // 起始编号的分卷集，解压失败后整组隔离，不作为普通独立包处理。
    assert!(archive_name("x.7z.000"));
    assert!(archive_name("x.tar.gz.000"));
    // 白名单外格式的卷与无格式孤立编号完全不碰（X-09）。
    for name in [
        "data.001",
        "x.iso.001",
        "x.docx.001",
        "x.msi.001",
        "x.txt.001",
    ] {
        assert!(
            !archive_name(name),
            "{name} 不在白名单数字尾卷族内，不得识别"
        );
    }
    // 编号不足三位、多余前导零，或编号不是族入口（后续卷成组处理）时不是入口。
    for name in ["x.7z.1", "x.7z.01", "x.7z.0001", "x.7z.002", "x.tar.gz.002"] {
        assert!(
            !archive_name(name),
            "{name} 不是数字尾卷族入口，不得单独入队"
        );
    }
    // 纯白名单名不受影响。
    for name in ["a.zip", "a.7z", "f.tar.gz", "b.tgz", "a.rar", "e.part1.rar"] {
        assert!(archive_name(name), "{name} 仍在普通白名单内");
    }
}

// 覆盖 X-10, X-09：multipart_name 与 archive_name 的数字尾卷族口径一致。
#[test]
fn multipart_name_numbered_family_alignment_with_x10() {
    assert!(multipart_name("x.part1.rar"));
    assert!(multipart_name("x.part99.rar"));
    assert!(!multipart_name("x.rar"));
    assert!(multipart_name("x.7z.001"));
    assert!(multipart_name("x.tar.gz.001"));
    assert!(multipart_name("x.7z.000"));
    assert!(!multipart_name("x.7z.002"));
    // 白名单外与孤立编号不是组入口（X-09）。
    assert!(!multipart_name("data.001"));
    assert!(!multipart_name("x.iso.001"));
}

// 覆盖 X-10, 附录 E（回归：只有 a.part02/a.part03、缺 part1 的目录仍归为
// 一组并给出唯一代表；修复前这种目录被静默漏掉——入口侧没有任何包被识别，
// 缺首卷既不计数也不隔离。入口按卷号数值判定：`part1` 在场即有入口）
#[test]
fn part_rar_missing_first_groups_recognize_tail_only_sets() {
    let names = |items: &[&str]| -> Vec<String> {
        items.iter().map(std::string::ToString::to_string).collect()
    };
    // 附录 E 原例：缺 01 的一组，代表是最小卷号的余卷。
    assert_eq!(
        part_rar_missing_first_groups(&names(&["a.part02.rar", "a.part03.rar"])),
        vec!["a.part02.rar".to_string()]
    );
    // 同宽度的 part1 在场：正常入口组（archive_name 已入队），不是缺首卷。
    assert!(part_rar_missing_first_groups(&names(&["a.part01.rar", "a.part02.rar"])).is_empty());
    // 独立审查发现（X-10「位数超过最小宽度时自然增长」）：无补零完整 12 卷集
    // part1…part12 是同一族，宽 2 的 part10-12 不得拆成「缺 part01」的伪组
    // （曾导致计数/入队翻倍并可能整组误隔离）。
    let mut complete = vec!["a.part1.rar".to_string()];
    for index in 2..=12 {
        complete.push(format!("a.part{index}.rar"));
    }
    assert!(part_rar_missing_first_groups(&complete).is_empty());
    // 缺 part1 的自然增长集：仍是一组，代表是最小卷号 part2。
    let without_first: Vec<String> = complete[1..].to_vec();
    assert_eq!(
        part_rar_missing_first_groups(&without_first),
        vec!["a.part2.rar".to_string()]
    );
    // 补零 100 卷完整集（part01…part99 + part100 自然增长）同样不产生伪组。
    let mut padded = Vec::new();
    for index in 1..=100 {
        padded.push(format!("a.part{index:02}.rar"));
    }
    assert!(part_rar_missing_first_groups(&padded).is_empty());
    // part1（任意补零写法）在场即有入口：part02 与之同族，不另立缺首卷组。
    assert!(part_rar_missing_first_groups(&names(&[
        "a.part1.rar",
        "a.part02.rar",
        "a.part03.rar"
    ]))
    .is_empty());
    // 不同主干互不归组；同主干只计一组。
    assert_eq!(
        part_rar_missing_first_groups(&names(&["a.part2.rar", "a.part3.rar", "b.part02.rar"])),
        vec!["a.part2.rar".to_string(), "b.part02.rar".to_string()]
    );
    // 普通 .rar 与 report.partial.rar 不参与。
    assert!(part_rar_missing_first_groups(&names(&["solo.rar", "report.partial.rar"])).is_empty());
}

// 覆盖 X-10（回归：数字尾卷族只有 .NNN（N≥2）、缺 .001/.000 入口时按一组
// 计；入口在场（含 .000 非法起始）的组不属「缺入口」，白名单外孤立编号不归组）
#[test]
fn numbered_volume_missing_entry_groups_recognize_tail_only_sets() {
    let names = |items: &[&str]| -> Vec<String> {
        items.iter().map(std::string::ToString::to_string).collect()
    };
    assert_eq!(
        numbered_volume_missing_entry_groups(&names(&["b.7z.002", "b.7z.003"])),
        vec!["b.7z.002".to_string()]
    );
    // .001 入口在场：断号组由入口包成组处理（引擎报分卷不全），不是缺入口。
    assert!(numbered_volume_missing_entry_groups(&names(&["b.7z.001", "b.7z.003"])).is_empty());
    // .000 是可识别的非法起始（archive_name 已入队），同样不算缺入口。
    assert!(numbered_volume_missing_entry_groups(&names(&["b.7z.000", "b.7z.002"])).is_empty());
    // 入口自身与白名单外孤立编号不归组（X-09）。
    assert!(numbered_volume_missing_entry_groups(&names(&["c.zip.001"])).is_empty());
    assert!(numbered_volume_missing_entry_groups(&names(&["data.002"])).is_empty());
}

#[test]
fn numbered_volume_growth_missing_entry_groups_include_natural_width_tails() {
    let names = |items: &[&str]| -> Vec<String> {
        items.iter().map(std::string::ToString::to_string).collect()
    };
    assert_eq!(
        numbered_volume_missing_entry_groups(&names(&["b.7z.1000", "b.7z.1001"])),
        vec!["b.7z.1000".to_string()]
    );
    assert_eq!(
        numbered_volume_missing_entry_groups(&names(&[
            "b.7z.999",
            "b.7z.1000",
            "b.7z.1001",
            "c.tar.gz.1000",
            "c.tar.gz.1001",
            "d.zip.18446744073709551616",
        ])),
        vec![
            "b.7z.999".to_string(),
            "c.tar.gz.1000".to_string(),
            "d.zip.18446744073709551616".to_string(),
        ]
    );
    for entry in ["b.7z.001", "b.7z.000"] {
        assert!(
            numbered_volume_missing_entry_groups(&names(&["b.7z.1000", entry, "b.7z.1001",]))
                .is_empty()
        );
    }
    for name in [
        "data.1000",
        "x.iso.1000",
        "x.docx.1000",
        "x.7z.0001",
        "x.7z.01000",
        "x.7z.1",
        "x.7z.01",
        "x.7z.１０００",
        "x.7z.١٠٠٠",
        "x.7z.1000.bak",
    ] {
        assert!(
            numbered_volume_missing_entry_groups(&names(&[name])).is_empty(),
            "{name} 不属数字尾卷族，不得归组"
        );
    }
}

#[test]
fn numbered_volume_growth_classifies_canonical_whitelisted_tails() {
    for name in [
        "x.7z.999",
        "x.7z.1000",
        "x.zip.1001",
        "x.tar.gz.1000",
        "x.tar.bz2.1001",
        "x.tzst.18446744073709551616",
    ] {
        assert_eq!(category_for(name), "压缩包", "{name}");
        assert!(!archive_name(name), "{name} 不是独立入口");
        assert!(!multipart_name(name), "{name} 不是分卷入口");
        assert!(archive_entry_key(name).is_none(), "{name} 不得独立计包");
    }
    for name in [
        "x.7z.0001",
        "x.7z.01000",
        "x.7z.1",
        "x.7z.01",
        "x.7z.１０００",
        "x.7z.١٠٠٠",
        "x.7z.1000.bak",
        "x.iso.1000",
        "data.1000",
    ] {
        assert_eq!(category_for(name), "其他", "{name}");
    }
}

#[cfg(windows)]
#[test]
fn numbered_volume_growth_groups_keep_windows_ordinal_keys() {
    assert_eq!(
        numbered_volume_missing_entry_groups(&[
            "\u{1f80}.7z.1000".into(),
            "\u{1f88}.7z.1001".into(),
        ]),
        vec!["\u{1f80}.7z.1000".to_string()]
    );
    assert_eq!(
        numbered_volume_missing_entry_groups(&[
            "\u{0130}.7z.1000".into(),
            "i\u{0307}.7z.1001".into(),
        ])
        .len(),
        2
    );
}
