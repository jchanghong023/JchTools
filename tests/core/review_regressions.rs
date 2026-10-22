use super::*;

// 覆盖 C-08 / CONTRACT 附录 B：移除副本标记不能重新切分并丢掉主体中的 .tar。
#[test]
fn review_f03_copy_cleanup_preserves_newly_exposed_compound_suffix() {
    let f = Fixture::new();
    let original = "report.tar - Copy.gz";
    let payload = b"archive payload is not decoded by directory organizer";
    f.write(original, payload, 10);
    let cfg = Config::default();
    let task = f.plan(cfg.clone());
    let moves = Database::open(&task.directory)
        .unwrap()
        .actions_page_filtered(0, 100, Some("move"))
        .unwrap();
    assert_eq!(moves.len(), 1);
    assert_eq!(moves[0].source, original);
    assert_eq!(moves[0].target.as_deref(), Some("压缩包/report.tar.gz"));
    let result = Fixture::apply(&task);
    assert_eq!(result.summary.errors, 0);
    assert_eq!(
        fs::read(f.root.join("压缩包/report.tar.gz")).unwrap(),
        payload
    );
    assert!(!f.root.join("压缩包/report.gz").exists());
    let again = f.plan(cfg);
    assert_eq!(again.summary.planned_move, 0);
    assert_eq!(again.summary.planned_delete, 0);
    assert_eq!(again.summary.errors, 0);
}

// 覆盖 C-08 / C-10 / C-16 / CONTRACT 附录 B：合法性处理暴露标记后须在本轮收敛。
#[test]
fn review_f04_legalization_exposed_copy_marker_is_normalized_in_first_plan() {
    let f = Fixture::new();
    f.write("report (1)..txt", b"report contents", 10);
    let cfg = Config::default();
    let task = f.plan(cfg.clone());
    let moves = Database::open(&task.directory)
        .unwrap()
        .actions_page_filtered(0, 100, Some("move"))
        .unwrap();
    assert_eq!(moves.len(), 1);
    assert_eq!(moves[0].target.as_deref(), Some("文档/report_1.txt"));
    let result = Fixture::apply(&task);
    assert_eq!(result.summary.errors, 0);
    assert_eq!(
        fs::read(f.root.join("文档/report_1.txt")).unwrap(),
        b"report contents"
    );
    let again = f.plan(cfg);
    assert_eq!(again.summary.planned_move, 0);
    assert_eq!(again.summary.planned_delete, 0);
    assert_eq!(again.summary.errors, 0);
}

// 覆盖 C-08 / CONTRACT 附录 B：WAVE_FORMAT_EXTENSIBLE 必须有完整扩展格式结构。
#[test]
fn review_f05_incomplete_extensible_wav_keeps_original_extension_and_category() {
    let f = Fixture::new();
    let mut format = Vec::new();
    format.extend_from_slice(&0xfffeu16.to_le_bytes());
    format.extend_from_slice(&1u16.to_le_bytes());
    format.extend_from_slice(&8_000u32.to_le_bytes());
    format.extend_from_slice(&16_000u32.to_le_bytes());
    format.extend_from_slice(&2u16.to_le_bytes());
    format.extend_from_slice(&16u16.to_le_bytes());
    let mut cases = Vec::new();
    for (name, extension) in [
        ("missing", Vec::new()),
        ("short", 0u16.to_le_bytes().to_vec()),
        ("truncated", 22u16.to_le_bytes().to_vec()),
    ] {
        let mut fmt = format.clone();
        fmt.extend_from_slice(&extension);
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&(u32::try_from(fmt.len()).unwrap() + 22).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&u32::try_from(fmt.len()).unwrap().to_le_bytes());
        wav.extend_from_slice(&fmt);
        wav.extend_from_slice(b"data\x02\0\0\0\0\0");
        let path = f.write(&format!("{name}.bin"), &wav, 10);
        cases.push((name, path, wav));
    }
    // 有效扩展 PCM 对照：cbSize=22、有效位宽、声道掩码与 PCM SubFormat GUID。
    let mut valid_fmt = format;
    valid_fmt.extend_from_slice(&22u16.to_le_bytes());
    valid_fmt.extend_from_slice(&16u16.to_le_bytes());
    valid_fmt.extend_from_slice(&4u32.to_le_bytes());
    valid_fmt.extend_from_slice(&[
        1, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71,
    ]);
    let mut valid = b"RIFF".to_vec();
    valid.extend_from_slice(&(u32::try_from(valid_fmt.len()).unwrap() + 22).to_le_bytes());
    valid.extend_from_slice(b"WAVEfmt ");
    valid.extend_from_slice(&u32::try_from(valid_fmt.len()).unwrap().to_le_bytes());
    valid.extend_from_slice(&valid_fmt);
    valid.extend_from_slice(b"data\x02\0\0\0\0\0");
    // 长度完整仍不足以确认类型：必要字段、GUID 与采样一致性都必须有效。
    for (name, offset, replacement) in [
        ("oversized-extra", 36, 23u16.to_le_bytes().to_vec()),
        ("zero-valid-bits", 38, 0u16.to_le_bytes().to_vec()),
        ("excess-valid-bits", 38, 17u16.to_le_bytes().to_vec()),
        ("channel-mismatch", 40, 3u32.to_le_bytes().to_vec()),
        (
            "reserved-channel",
            40,
            0x8000_0000u32.to_le_bytes().to_vec(),
        ),
        ("unknown-subformat", 44, vec![2]),
        ("bad-guid", 59, vec![0]),
        ("bad-float-width", 44, vec![3]),
        ("bad-byte-rate", 28, 1u32.to_le_bytes().to_vec()),
        ("bad-frame-size", 32, 1u16.to_le_bytes().to_vec()),
    ] {
        let mut malformed = valid.clone();
        malformed[offset..offset + replacement.len()].copy_from_slice(&replacement);
        let path = f.write(&format!("{name}.bin"), &malformed, 10);
        cases.push((name, path, malformed));
    }
    let mut float_fmt = valid_fmt.clone();
    float_fmt[8..12].copy_from_slice(&32_000u32.to_le_bytes());
    float_fmt[12..14].copy_from_slice(&4u16.to_le_bytes());
    float_fmt[14..16].copy_from_slice(&32u16.to_le_bytes());
    float_fmt[18..20].copy_from_slice(&32u16.to_le_bytes());
    float_fmt[24] = 3;
    let mut valid_float = b"RIFF".to_vec();
    valid_float.extend_from_slice(&(u32::try_from(float_fmt.len()).unwrap() + 24).to_le_bytes());
    valid_float.extend_from_slice(b"WAVEfmt ");
    valid_float.extend_from_slice(&u32::try_from(float_fmt.len()).unwrap().to_le_bytes());
    valid_float.extend_from_slice(&float_fmt);
    valid_float.extend_from_slice(b"data\x04\0\0\0\0\0\0\0");
    let float_path = f.write("valid-float.bin", &valid_float, 20);
    assert_eq!(
        rules::detect_extension(&float_path, &Control::default()).unwrap(),
        Some("wav")
    );
    let valid_path = f.write("valid.bin", &valid, 20);
    assert_eq!(
        rules::detect_extension(&valid_path, &Control::default()).unwrap(),
        Some("wav")
    );
    for (name, path, _) in &cases {
        assert_eq!(
            rules::detect_extension(path, &Control::default()).unwrap(),
            None,
            "{name}: 不完整扩展格式不能授权改后缀"
        );
    }
    let task = f.plan(Config {
        fix_extension: true,
        ..Config::default()
    });
    let moves = Database::open(&task.directory)
        .unwrap()
        .actions_page_filtered(0, 100, Some("move"))
        .unwrap();
    for (name, _, _) in &cases {
        let source = format!("{name}.bin");
        let target = format!("其他/{name}.bin");
        assert_eq!(
            moves
                .iter()
                .find(|action| action.source == source)
                .unwrap()
                .target
                .as_deref(),
            Some(target.as_str())
        );
    }
    let result = Fixture::apply(&task);
    assert_eq!(result.summary.errors, 0);
    for (name, _, wav) in cases {
        assert_eq!(
            fs::read(f.root.join(format!("其他/{name}.bin"))).unwrap(),
            wav
        );
        assert!(!f.root.join(format!("音频/{name}.wav")).exists());
    }
    assert_eq!(fs::read(f.root.join("音频/valid.wav")).unwrap(), valid);
    assert_eq!(
        fs::read(f.root.join("音频/valid-float.wav")).unwrap(),
        valid_float
    );
}

// 覆盖 C-08 / C-16 / C-21 / CONTRACT 附录 B：关闭名称规则后不修饰既有合法完整名。
#[test]
fn review_f17_disabled_name_cleanup_preserves_legal_double_dot_name() {
    let f = Fixture::new();
    let path = f.write("文档/report..pdf", b"existing legal document name", 10);
    let before = fsutil::snapshot(&path).unwrap();
    let cfg = Config {
        normalize_names: false,
        clean_copy_name: false,
        fix_extension: false,
        ..Config::default()
    };
    let task = f.plan(cfg.clone());
    assert_eq!(task.summary.errors, 0);
    assert_eq!(
        task.summary.planned_move, 0,
        "未派生新名称，不得删除主体尾点"
    );
    assert_eq!(task.summary.planned_delete, 0);
    let result = Fixture::apply(&task);
    assert_eq!(result.summary.errors, 0);
    assert_eq!(fs::read(&path).unwrap(), b"existing legal document name");
    assert_eq!(fsutil::snapshot(&path).unwrap().identity, before.identity);
    assert!(!f.root.join("文档/report.pdf").exists());
    let again = f.plan(cfg);
    assert_eq!(again.summary.planned_move, 0);
    assert_eq!(again.summary.planned_delete, 0);
}

// 覆盖 C-05 / C-10 / C-18 / C-21 / 附录 A/B：加来源前缀后按最终完整名重算大类。
#[test]
fn review_f18_prefixed_dotfiles_use_final_extension_category() {
    let f = Fixture::new();
    f.write("x/.pdf", b"left document", 10);
    f.write("y/.pdf", b"right document", 20);
    let cfg = Config::default();
    let task = f.plan(cfg.clone());
    let moves = Database::open(&task.directory)
        .unwrap()
        .actions_page_filtered(0, 100, Some("move"))
        .unwrap();
    assert_eq!(moves.len(), 2);
    for (source, target) in [("x/.pdf", "文档/x_.pdf"), ("y/.pdf", "文档/y_.pdf")] {
        assert_eq!(
            moves
                .iter()
                .find(|action| action.source == source)
                .unwrap()
                .target
                .as_deref(),
            Some(target)
        );
    }
    let result = Fixture::apply(&task);
    assert_eq!(result.summary.errors, 0);
    assert_eq!(
        fs::read(f.root.join("文档/x_.pdf")).unwrap(),
        b"left document"
    );
    assert_eq!(
        fs::read(f.root.join("文档/y_.pdf")).unwrap(),
        b"right document"
    );
    let again = f.plan(cfg);
    assert_eq!(again.summary.planned_move, 0);
    assert_eq!(again.summary.planned_delete, 0);
    assert_eq!(again.summary.errors, 0);
}

// 覆盖 C-10 / C-21：独立观察完整成功执行后的第二轮，不以首轮目标断言遮住幂等错误。
#[test]
fn review_f18_prefixed_dotfiles_are_idempotent_after_successful_apply() {
    let f = Fixture::new();
    f.write("x/.pdf", b"left document", 10);
    f.write("y/.pdf", b"right document", 20);
    let cfg = Config::default();
    let result = Fixture::apply(&f.plan(cfg.clone()));
    assert_eq!(result.summary.errors, 0);
    let again = f.plan(cfg);
    assert_eq!(again.summary.errors, 0);
    assert_eq!(again.summary.planned_move, 0);
    assert_eq!(again.summary.planned_delete, 0);
}

// 覆盖 S-06 / C-08：同一物理文件的两条临时硬链接都入删除计划时只计一份候选大小。
#[test]
fn review_f19_cleanup_hardlinks_candidate_bytes_count_once() {
    let f = Fixture::new();
    let payload = b"one logical payload";
    let source = f.write("one.tmp", payload, 10);
    let alias = f.root.join("two.tmp");
    fs::hard_link(&source, &alias).expect("本用例要求支持硬链接的文件系统");
    let left = fsutil::snapshot(&source).unwrap();
    let right = fsutil::snapshot(&alias).unwrap();
    assert_eq!(left.identity, right.identity);
    assert_eq!(left.links, 2);
    assert_eq!(right.links, 2);
    let task = f.plan(Config {
        clean_temp: true,
        ..Config::default()
    });
    assert_eq!(task.summary.errors, 0);
    assert_eq!(task.summary.planned_delete, 2);
    assert_eq!(
        task.summary.candidate_bytes,
        u64::try_from(payload.len()).unwrap()
    );
}

// 覆盖 S-06 / C-08：实际成功删除两条硬链接应报告两项、一份逻辑大小，不是 0 B。
#[test]
fn review_f19_cleanup_hardlinks_permanent_bytes_count_once() {
    let f = Fixture::new();
    let payload = b"one logical payload";
    let source = f.write("one.tmp", payload, 10);
    fs::hard_link(&source, f.root.join("two.tmp")).expect("本用例要求支持硬链接的文件系统");
    let task = f.plan(Config {
        clean_temp: true,
        ..Config::default()
    });
    assert_eq!(task.summary.planned_delete, 2);
    let result = Fixture::apply(&task);
    assert_eq!(result.summary.errors, 0);
    assert_eq!(result.summary.deleted, 2);
    assert!(!source.exists());
    assert!(!f.root.join("two.tmp").exists());
    assert_eq!(
        result.summary.permanent_bytes,
        u64::try_from(payload.len()).unwrap()
    );
}

// 覆盖 C-17 / C-18：点文件的最终类别改变后仍须避开该类别内既有占用。
#[test]
fn review_f18_final_category_conflicts_preserve_existing_file() {
    let f = Fixture::new();
    let existing = f.write("文档/x_.pdf", b"existing document", 10);
    let before = fsutil::snapshot(&existing).unwrap();
    f.write("x/.pdf", b"left document", 20);
    f.write("y/.pdf", b"right document", 30);
    let cfg = Config::default();
    let task = f.plan(cfg.clone());
    let actions = Database::open(&task.directory)
        .unwrap()
        .actions_page_filtered(0, 100, Some("move"))
        .unwrap();
    assert_eq!(actions.len(), 2);
    for action in &actions {
        let target = action.target.as_deref().unwrap();
        let (category, name) = target.split_once('/').unwrap();
        assert_eq!(category, rules::category_for(&name.to_lowercase()));
        assert_ne!(fsutil::fold_rel(target), fsutil::fold_rel("文档/x_.pdf"));
    }
    let result = Fixture::apply(&task);
    assert_eq!(result.summary.errors, 0);
    assert_eq!(fs::read(&existing).unwrap(), b"existing document");
    assert_eq!(
        fsutil::snapshot(&existing).unwrap().identity,
        before.identity
    );
    for action in actions {
        let payload = if action.source == "x/.pdf" {
            b"left document".as_slice()
        } else {
            b"right document".as_slice()
        };
        assert_eq!(
            fs::read(f.root.join(action.target.unwrap())).unwrap(),
            payload
        );
    }
    let again = f.plan(cfg);
    assert_eq!(again.summary.planned_move, 0);
    assert_eq!(again.summary.planned_delete, 0);
    assert_eq!(again.summary.errors, 0);
}

// 覆盖 S-06：可靠身份记账跨执行页共享，但新一轮分析不能沿用旧身份集合。
#[test]
fn review_f19_cleanup_hardlinks_across_batches_count_once() {
    let f = Fixture::new();
    let payload = b"one logical payload across multiple execution pages";
    let source = f.write("link-000.tmp", payload, 10);
    for index in 1..130 {
        fs::hard_link(&source, f.root.join(format!("link-{index:03}.tmp")))
            .expect("本用例要求支持硬链接的文件系统");
    }
    let cfg = Config {
        clean_temp: true,
        ..Config::default()
    };
    let task = f.plan(cfg.clone());
    let repeated = f.plan(cfg.clone());
    let size = u64::try_from(payload.len()).unwrap();
    assert_eq!(task.summary.planned_delete, 130);
    assert_eq!(task.summary.candidate_bytes, size);
    assert_eq!(repeated.summary.candidate_bytes, size);
    let result = Fixture::apply(&task);
    assert_eq!(result.summary.deleted, 130);
    assert_eq!(result.summary.permanent_bytes, size);
    assert_eq!(result.summary.errors, 0);
    assert_eq!(fs::read_dir(&f.root).unwrap().count(), 0);
    let again = f.plan(cfg);
    assert_eq!(again.summary.candidate_bytes, 0);
    assert_eq!(again.summary.planned_delete, 0);
    assert_eq!(again.summary.planned_move, 0);
}
