use super::*;

// 覆盖 C-08 / 附录 B：孤立魔数不能证明文件有效结构或授权修正扩展名。
#[test]
fn fix_extension_rejects_short_magic_without_valid_structure() {
    let f = Fixture::new();
    let samples: [(&str, &[u8]); 4] = [
        ("png.txt", b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"),
        ("zip.txt", b"PK\x03\x04\0\0\0\0"),
        ("seven.txt", b"\x37\x7a\xbc\xaf\x27\x1c\0\0\0\0"),
        ("rar.txt", b"Rar!\x1a\x07\0payload"),
    ];
    for (name, bytes) in samples {
        f.write(name, bytes, 10);
    }
    let cfg = Config {
        fix_extension: true,
        detect_type: true,
        ..Config::default()
    };
    let task = f.plan(cfg);
    let db = Database::open(&task.directory).unwrap();
    let moves = db.actions_page_filtered(0, 100, Some("move")).unwrap();
    assert_eq!(moves.len(), samples.len());
    for (name, _) in samples {
        let expected = format!("文档/{name}");
        let action = moves.iter().find(|action| action.source == name).unwrap();
        assert_eq!(
            action.target.as_deref(),
            Some(expected.as_str()),
            "{action:?}"
        );
    }
    drop(db);
    Fixture::apply(&task);
    for (name, bytes) in samples {
        assert_eq!(fs::read(f.root.join("文档").join(name)).unwrap(), bytes);
    }
}

fn valid_zip_fixture(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, bytes) in entries {
        writer
            .start_file(
                *name,
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
        std::io::Write::write_all(&mut writer, bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn valid_word_fixture(main_content_type: &str) -> Vec<u8> {
    let content_types = format!(
        r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="{main_content_type}"/></Types>"#
    );
    valid_zip_fixture(&[
        ("[Content_Types].xml", content_types.as_bytes()),
        ("_rels/.rels", br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#),
        ("word/document.xml", br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p/></w:body></w:document>"#),
    ])
}

fn valid_empty_rar_fixture() -> Vec<u8> {
    let mut bytes = b"Rar!\x1a\x07\0".to_vec();
    for header in [
        &[0x73, 0, 0, 13, 0, 0, 0, 0, 0, 0, 0][..],
        &[0x7b, 0, 0, 7, 0][..],
    ] {
        let mut crc = flate2::Crc::new();
        crc.update(header);
        bytes.extend_from_slice(&crc.sum().to_le_bytes()[..2]);
        bytes.extend_from_slice(header);
    }
    bytes
}

fn rar5_fixture(main_header: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(21 + main_header.len());
    bytes.extend_from_slice(b"Rar!\x1a\x07\x01\0");
    for header in [main_header, &[5, 0, 0][..]] {
        assert!(header.len() < 128, "本夹具只使用单字节 size vint");
        let size = u8::try_from(header.len()).unwrap();
        let mut crc = flate2::Crc::new();
        crc.update(&[size]);
        crc.update(header);
        bytes.extend_from_slice(&crc.sum().to_le_bytes());
        bytes.push(size);
        bytes.extend_from_slice(header);
    }
    bytes
}

fn seven_zip_fixture(next_header: &[u8]) -> Vec<u8> {
    let mut crc = flate2::Crc::new();
    crc.update(next_header);
    let mut start_header = [0u8; 20];
    start_header[8..16].copy_from_slice(&(next_header.len() as u64).to_le_bytes());
    start_header[16..20].copy_from_slice(&crc.sum().to_le_bytes());
    crc.reset();
    crc.update(&start_header);
    let mut bytes = b"\x37\x7a\xbc\xaf\x27\x1c\0\x04".to_vec();
    bytes.extend_from_slice(&crc.sum().to_le_bytes());
    bytes.extend_from_slice(&start_header);
    bytes.extend_from_slice(next_header);
    bytes
}

fn flac_fixture(subframe: &[u8]) -> Vec<u8> {
    let frame_size = u32::try_from(9 + subframe.len()).unwrap().to_be_bytes();
    let mut info = [0u8; 34];
    info[..4].copy_from_slice(&[0, 16, 0, 16]);
    info[4..7].copy_from_slice(&frame_size[1..]);
    info[7..10].copy_from_slice(&frame_size[1..]);
    info[10..18].copy_from_slice(&((44_100u64 << 44) | (15u64 << 36) | 16).to_be_bytes());
    let header = [0xff, 0xf8, 0x69, 0x08, 0, 15];
    let mut crc8 = 0u8;
    for byte in header {
        crc8 ^= byte;
        for _ in 0..8 {
            crc8 = if crc8 & 0x80 != 0 {
                (crc8 << 1) ^ 7
            } else {
                crc8 << 1
            };
        }
    }
    let mut bytes = Vec::with_capacity(51 + subframe.len());
    bytes.extend_from_slice(b"fLaC\x80\0\0\x22");
    bytes.extend_from_slice(&info);
    let frame_start = bytes.len();
    bytes.extend_from_slice(&header);
    bytes.push(crc8);
    bytes.extend_from_slice(subframe);
    let mut crc16 = 0u16;
    for byte in &bytes[frame_start..] {
        crc16 ^= u16::from(*byte) << 8;
        for _ in 0..8 {
            crc16 = if crc16 & 0x8000 != 0 {
                (crc16 << 1) ^ 0x8005
            } else {
                crc16 << 1
            };
        }
    }
    bytes.extend_from_slice(&crc16.to_be_bytes());
    bytes
}

// 覆盖 C-08 / 附录 B：可靠识别不以旧后缀是否在大类映射中为前提。
#[test]
fn fix_extension_corrects_valid_png_with_unknown_or_missing_extension() {
    let f = Fixture::new();
    let png = include_bytes!("../markdown_fixtures/test_hello_world.png");
    f.write("photo.bin", png, 10);
    f.write("image", png, 20);
    f.write(".hidden", png, 30);
    let cfg = Config {
        fix_extension: true,
        detect_type: true,
        include_hidden: true,
        ..Config::default()
    };
    let task = f.plan(cfg);
    let db = Database::open(&task.directory).unwrap();
    let moves = db.actions_page_filtered(0, 100, Some("move")).unwrap();
    let targets: Vec<_> = ["photo.bin", "image", ".hidden"]
        .into_iter()
        .map(|source| {
            moves
                .iter()
                .find(|action| action.source == source)
                .and_then(|action| action.target.as_deref())
        })
        .collect();
    assert_eq!(
        targets,
        [
            Some("图片/photo.png"),
            Some("图片/image.png"),
            Some("图片/.hidden.png"),
        ]
    );
    drop(db);
    Fixture::apply(&task);
    assert_eq!(fs::read(f.root.join("图片/photo.png")).unwrap(), png);
    assert_eq!(fs::read(f.root.join("图片/image.png")).unwrap(), png);
    assert_eq!(fs::read(f.root.join("图片/.hidden.png")).unwrap(), png);
}

// 覆盖 C-08：相邻 OOXML 家族后缀并非合法同义形式，依实际包类型修正。
#[test]
fn fix_extension_corrects_valid_docx_mislabeled_as_docm() {
    let f = Fixture::new();
    let docx = valid_word_fixture(
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
    );
    f.write("文档.docm", &docx, 10);
    let cfg = Config {
        fix_extension: true,
        detect_type: true,
        ..Config::default()
    };
    let task = f.plan(cfg);
    let db = Database::open(&task.directory).unwrap();
    let moves = db.actions_page_filtered(0, 100, Some("move")).unwrap();
    assert_eq!(moves.len(), 1);
    assert_eq!(moves[0].target.as_deref(), Some("文档/文档.docx"));
    drop(db);
    Fixture::apply(&task);
    assert_eq!(fs::read(f.root.join("文档/文档.docx")).unwrap(), docx);
    assert!(!exists_somewhere(&f.root, "文档.docm"));
}

// 覆盖 C-08 / 附录 B：OOXML 必须由包级 officeDocument 关联指向有效主部件，
// 仅有 MIME 声明与主部件 XML 不授权改后缀。
#[test]
fn review_signature_ooxml_requires_package_office_document_relationship() {
    let f = Fixture::new();
    let types = br#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;
    let main = br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body/></w:document>"#;
    let wrong = br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/missing.xml"/></Relationships>"#;
    let external = br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" TargetMode="External" Target="word/document.xml"/></Relationships>"#;
    let strict = br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://purl.oclc.org/ooxml/officeDocument/relationships/officeDocument" Target="/word/document.xml"/></Relationships>"#;
    let mut samples = Vec::new();
    for (name, relationship, detected, target) in [
        ("missing.bin", None, None, "其他/missing.bin"),
        ("wrong.bin", Some(&wrong[..]), None, "其他/wrong.bin"),
        (
            "external.bin",
            Some(&external[..]),
            None,
            "其他/external.bin",
        ),
        (
            "strict.txt",
            Some(&strict[..]),
            Some("docx"),
            "文档/strict.docx",
        ),
    ] {
        let mut entries = vec![
            ("[Content_Types].xml", &types[..]),
            ("word/document.xml", &main[..]),
        ];
        if let Some(relationship) = relationship {
            entries.push(("_rels/.rels", relationship));
        }
        let encoded = valid_zip_fixture(&entries);
        let path = f.write(name, &encoded, 10);
        assert_eq!(
            rules::detect_extension(&path, &Control::default()).unwrap(),
            detected,
            "{name}"
        );
        samples.push((name, target, encoded));
    }
    let valid = valid_word_fixture(
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
    );
    let path = f.write("valid.txt", &valid, 20);
    assert_eq!(
        rules::detect_extension(&path, &Control::default()).unwrap(),
        Some("docx")
    );
    samples.push(("valid.txt", "文档/valid.docx", valid));
    let task = f.plan(Config {
        fix_extension: true,
        ..Config::default()
    });
    let moves = Database::open(&task.directory)
        .unwrap()
        .actions_page_filtered(0, 100, Some("move"))
        .unwrap();
    for (source, target, _) in &samples {
        assert_eq!(
            moves
                .iter()
                .find(|action| action.source == *source)
                .unwrap()
                .target
                .as_deref(),
            Some(*target)
        );
    }
    let result = Fixture::apply(&task);
    assert_eq!(result.summary.errors, 0);
    for (_, target, encoded) in samples {
        assert_eq!(fs::read(f.root.join(target)).unwrap(), encoded);
    }
}

fn review_word_relationship_fixture(
    target: &str,
    part: &str,
    members: &[&str],
    mode: &str,
) -> Vec<u8> {
    let content_types = format!(
        r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/{part}" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#
    );
    let relationships = format!(
        r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="{target}" {mode}/></Relationships>"#
    );
    let mut entries = vec![
        ("[Content_Types].xml", content_types.as_bytes()),
        ("_rels/.rels", relationships.as_bytes()),
    ];
    for member in members {
        entries.push((
            *member,
            &br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p/></w:body></w:document>"#[..],
        ));
    }
    valid_zip_fixture(&entries)
}

fn review_assert_word_relationship_flow(encoded: &[u8], expected: Option<&str>) {
    let f = Fixture::new();
    let source = f.write("input.bin", encoded, 10);
    assert_eq!(
        rules::detect_extension(&source, &Control::default()).unwrap(),
        expected
    );
    let task = f.plan(Config {
        fix_extension: true,
        detect_type: true,
        ..Config::default()
    });
    let target = if expected == Some("docx") {
        "文档/input.docx"
    } else {
        "其他/input.bin"
    };
    let moves = Database::open(&task.directory)
        .unwrap()
        .actions_page_filtered(0, 100, Some("move"))
        .unwrap();
    assert_eq!(moves.len(), 1);
    assert_eq!(moves[0].source, "input.bin");
    assert_eq!(moves[0].target.as_deref(), Some(target));
    // C-01：prepare 只读；即使计划将修正后缀，此时原始输入也必须不变。
    assert_eq!(fs::read(&source).unwrap(), encoded);
    assert!(!f.root.join(target).exists());
    let result = Fixture::apply(&task);
    assert_eq!(result.summary.errors, 0);
    assert_eq!(fs::read(f.root.join(target)).unwrap(), encoded);
    assert!(!source.exists());
    if expected.is_none() {
        assert!(!exists_somewhere(&f.root, "input.docx"));
        assert!(!exists_somewhere(&f.root, "input.zip"));
    }
}

// 覆盖 C-08 / CONTRACT 附录 B / S-01：OPC ASCII 忽略大小写的部件等价
// 必须贯穿 detect_extension → prepare → apply，按有效 DOCX 改名归类且不改内容。
#[test]
fn review_fix_ooxml_case_equivalent_parts_classify_and_rename_docx() {
    for (target, part, member) in [
        (
            "WORD/document.xml",
            "word/document.xml",
            "word/document.xml",
        ),
        (
            "word/document.xml",
            "WORD/document.xml",
            "word/document.xml",
        ),
        (
            "word/document.xml",
            "word/document.xml",
            "WORD/document.xml",
        ),
    ] {
        let encoded = review_word_relationship_fixture(target, part, &[member], "");
        review_assert_word_relationship_flow(&encoded, Some("docx"));
    }
}

// 覆盖 C-08 / CONTRACT 附录 B / S-01：绝对/相对包内路径的非 scheme 段
// 可含冒号，公开整理链路仍应修正为 DOCX；ZIP 内部部件不被解压或改写。
#[test]
fn review_fix_ooxml_colon_part_paths_classify_and_rename_docx() {
    for target in ["/word/document:1.xml", "word/document:1.xml"] {
        let encoded = review_word_relationship_fixture(
            target,
            "word/document:1.xml",
            &["word/document:1.xml"],
            r#"TargetMode="Internal""#,
        );
        review_assert_word_relationship_flow(&encoded, Some("docx"));
    }
}

// 覆盖 C-08 / CONTRACT 附录 B / S-01：外部、scheme、authority、query、
// fragment 不能授权内容签名改名；公开整理链路保留输入字节与原 .bin 后缀。
#[test]
fn review_fix_ooxml_invalid_uri_references_preserve_extension_and_bytes() {
    for (target, member, mode) in [
        ("word:document.xml", "word:document.xml", ""),
        ("https://example.test/document.xml", "word/document.xml", ""),
        ("//word/document:1.xml", "word/document:1.xml", ""),
        (
            "word/document:1.xml",
            "word/document:1.xml",
            r#"TargetMode="External""#,
        ),
        (
            "word/document.xml?version=1",
            "word/document.xml?version=1",
            "",
        ),
        ("word/document.xml#body", "word/document.xml#body", ""),
    ] {
        let encoded = review_word_relationship_fixture(target, member, &[member], mode);
        review_assert_word_relationship_flow(&encoded, None);
    }
    // 缺失真实主部件和大小写等价的重复成员仍须保持 .bin 与全部字节。
    for members in [&[][..], &["word/document.xml", "WORD/document.xml"][..]] {
        let encoded =
            review_word_relationship_fixture("word/document.xml", "word/document.xml", members, "");
        review_assert_word_relationship_flow(&encoded, None);
    }
}

// 覆盖附录 B：CRC 正确不能使非法 7z Header property 变成有效结构。
#[test]
fn review_signature_7z_invalid_property_rejects_complete_crc_header() {
    let f = Fixture::new();
    let invalid = seven_zip_fixture(&[1, 0xff, 0]);
    let valid = seven_zip_fixture(&[1, 0]); // kHeader、kEnd：有效空包。
    f.write("invalid.bin", &invalid, 10);
    f.write("valid.txt", &valid, 20);
    let task = f.plan(Config {
        fix_extension: true,
        detect_type: true,
        ..Config::default()
    });
    let db = Database::open(&task.directory).unwrap();
    let moves = db.actions_page_filtered(0, 100, Some("move")).unwrap();
    let targets: Vec<_> = ["invalid.bin", "valid.txt"]
        .into_iter()
        .map(|source| {
            moves
                .iter()
                .find(|action| action.source == source)
                .and_then(|action| action.target.as_deref())
        })
        .collect();
    assert_eq!(targets, [Some("其他/invalid.bin"), Some("压缩包/valid.7z")]);
    drop(db);
    Fixture::apply(&task);
    assert_eq!(fs::read(f.root.join("其他/invalid.bin")).unwrap(), invalid);
    assert_eq!(fs::read(f.root.join("压缩包/valid.7z")).unwrap(), valid);
}

// 覆盖附录 B：完整 RAR5 header CRC 不能豁免非法 extra TLV。
#[test]
fn review_signature_rar5_invalid_extra_rejects_complete_crc_header() {
    let f = Fixture::new();
    let invalid = rar5_fixture(&[1, 1, 1, 0, 0xff]);
    let valid = rar5_fixture(&[1, 0, 0]);
    f.write("invalid.bin", &invalid, 10);
    f.write("valid.txt", &valid, 20);
    let task = f.plan(Config {
        fix_extension: true,
        detect_type: true,
        ..Config::default()
    });
    let db = Database::open(&task.directory).unwrap();
    let moves = db.actions_page_filtered(0, 100, Some("move")).unwrap();
    let targets: Vec<_> = ["invalid.bin", "valid.txt"]
        .into_iter()
        .map(|source| {
            moves
                .iter()
                .find(|action| action.source == source)
                .and_then(|action| action.target.as_deref())
        })
        .collect();
    assert_eq!(
        targets,
        [Some("其他/invalid.bin"), Some("压缩包/valid.rar")]
    );
    drop(db);
    Fixture::apply(&task);
    assert_eq!(fs::read(f.root.join("其他/invalid.bin")).unwrap(), invalid);
    assert_eq!(fs::read(f.root.join("压缩包/valid.rar")).unwrap(), valid);
    assert!(!f.root.join("压缩包/invalid.rar").exists());
}

// 覆盖附录 B：FLAC frame CRC 正确不能使非法 subframe 成为有效音频结构。
#[test]
fn review_signature_flac_invalid_subframe_rejects_complete_crc_frame() {
    let f = Fixture::new();
    let invalid = flac_fixture(&[0xff, 0xff, 0]);
    let valid = flac_fixture(&[0, 0, 0]);
    f.write("invalid.bin", &invalid, 10);
    f.write("valid.txt", &valid, 20);
    let task = f.plan(Config {
        fix_extension: true,
        detect_type: true,
        ..Config::default()
    });
    let db = Database::open(&task.directory).unwrap();
    let moves = db.actions_page_filtered(0, 100, Some("move")).unwrap();
    let targets: Vec<_> = ["invalid.bin", "valid.txt"]
        .into_iter()
        .map(|source| {
            moves
                .iter()
                .find(|action| action.source == source)
                .and_then(|action| action.target.as_deref())
        })
        .collect();
    assert_eq!(targets, [Some("其他/invalid.bin"), Some("音频/valid.flac")]);
    drop(db);
    Fixture::apply(&task);
    assert_eq!(fs::read(f.root.join("其他/invalid.bin")).unwrap(), invalid);
    assert_eq!(fs::read(f.root.join("音频/valid.flac")).unwrap(), valid);
    assert!(!f.root.join("音频/invalid.flac").exists());
}

// 覆盖 C-08 / 附录 B：PCM 位宽为零不授权改后缀；合法 WAV 仍按实际类型归类且内容不变。
#[test]
fn review_signature_wav_invalid_pcm_preserves_original_extension() {
    let f = Fixture::new();
    let mut valid = b"RIFF\x26\0\0\0WAVEfmt \x10\0\0\0".to_vec();
    valid.extend_from_slice(&1u16.to_le_bytes()); // PCM
    valid.extend_from_slice(&1u16.to_le_bytes()); // 单声道
    valid.extend_from_slice(&8_000u32.to_le_bytes());
    valid.extend_from_slice(&16_000u32.to_le_bytes());
    valid.extend_from_slice(&2u16.to_le_bytes());
    valid.extend_from_slice(&16u16.to_le_bytes());
    valid.extend_from_slice(b"data\x02\0\0\0\0\0");
    let mut invalid = valid.clone();
    invalid[34..36].copy_from_slice(&0u16.to_le_bytes());
    let invalid_path = f.write("invalid.bin", &invalid, 10);
    let valid_path = f.write("valid.txt", &valid, 20);
    assert_eq!(
        rules::detect_extension(&invalid_path, &Control::default()).unwrap(),
        None
    );
    assert_eq!(
        rules::detect_extension(&valid_path, &Control::default()).unwrap(),
        Some("wav")
    );
    let task = f.plan(Config {
        fix_extension: true,
        ..Config::default()
    });
    let moves = Database::open(&task.directory)
        .unwrap()
        .actions_page_filtered(0, 100, Some("move"))
        .unwrap();
    for (source, target) in [
        ("invalid.bin", "其他/invalid.bin"),
        ("valid.txt", "音频/valid.wav"),
    ] {
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
    assert_eq!(fs::read(f.root.join("其他/invalid.bin")).unwrap(), invalid);
    assert_eq!(fs::read(f.root.join("音频/valid.wav")).unwrap(), valid);
    assert!(!f.root.join("音频/invalid.wav").exists());
}

// 覆盖 C-08（按内容签名修正错误扩展名：改名列入计划可见）
#[test]
fn fix_extension_plans_rename_to_detected_type() {
    let f = Fixture::new();
    let png = include_bytes!("../markdown_fixtures/test_hello_world.png");
    f.write("photo.txt", png, 10);
    let mut cfg = base();
    cfg.detect_type = true;
    cfg.fix_extension = true;
    let task = f.plan(cfg);
    let db = Database::open(&task.directory).unwrap();
    let moves = db.actions_page_filtered(0, 10, Some("move")).unwrap();
    drop(db);
    let expected = "图片/photo.png".to_string();
    assert_eq!(moves.len(), 1, "错误扩展名应产生改名计划");
    assert_eq!(
        moves[0].target.as_deref(),
        Some(expected.as_str()),
        "计划里可见改名为内容识别出的真实类型（并按修正后扩展名归类到「图片」）"
    );
    Fixture::apply(&task);
    assert!(f.root.join(&expected).exists());
}

// 覆盖 C-08（只修正「错误」扩展名：同类容器与别名扩展名不得按更粗的识别结果改粗）
#[test]
fn fix_extension_keeps_specialized_container_and_alias_extensions() {
    let f = Fixture::new();
    // 完整 ZIP 中的类型标识区分宏启用文档与模板，不能只看 word/ 路径。
    f.write(
        "报告.docm",
        &valid_word_fixture("application/vnd.ms-word.document.macroEnabled.main+xml"),
        10,
    );
    f.write(
        "模板.dotx",
        &valid_word_fixture(
            "application/vnd.openxmlformats-officedocument.wordprocessingml.template.main+xml",
        ),
        15,
    );
    // 未承诺的容器后缀不能因外层 ZIP 被改粗。
    f.write("包.whl", &valid_zip_fixture(&[("data.txt", b"wheel")]), 18);
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut gzip, b"<svg/>").unwrap();
    f.write("图标.svgz", &gzip.finish().unwrap(), 19);
    // 同义别名：htm→html、mid→midi 都不算「错误扩展名」。
    f.write(
        "index.htm",
        b"<!DOCTYPE html><html><body>hi</body></html>",
        20,
    );
    f.write(
        "歌曲.mid",
        b"MThd\0\0\0\x06\0\0\0\x01\0\x60MTrk\0\0\0\x04\0\xff\x2f\0",
        22,
    );
    let mut cfg = base();
    cfg.detect_type = true;
    cfg.fix_extension = true;
    let task = f.plan(cfg);
    let db = Database::open(&task.directory).unwrap();
    let moves = db.actions_page_filtered(0, 10, Some("move")).unwrap();
    drop(db);
    // C-05 归类恒开启：这些文件都有归类移动；断言收敛为「文件名逐字节不变」——
    // 正确扩展名不得按更粗的识别结果改名。
    assert!(
        moves.iter().all(|a| {
            let source_name = Path::new(&a.source)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            let target_name = a
                .target
                .as_deref()
                .and_then(|t| Path::new(t).file_name())
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            source_name == target_name
        }),
        "这些都是正确扩展名，不得按更粗的识别结果改名：{moves:?}"
    );
    assert_eq!(moves.len(), 6, "六个文件都应只有归类移动：{moves:#?}");
    Fixture::apply(&task);
    for name in [
        "报告.docm",
        "模板.dotx",
        "包.whl",
        "图标.svgz",
        "index.htm",
        "歌曲.mid",
    ] {
        assert!(exists_somewhere(&f.root, name), "{name} 必须保留原名");
    }
}

// 覆盖 C-08 / 附录 B（已知格式后缀 + 承诺可修正的 ZIP / 7z / RAR 内容时改名并归「压缩包」；
// 无法由签名证明后缀错误的后缀与 X-10 卷尾后缀保留）
#[test]
fn fix_extension_corrects_promised_containers_but_not_volume_tails() {
    let f = Fixture::new();
    // 已知格式后缀 + 真实压缩包内容：识别结果为 zip / 7z / rar，应按识别结果改名。
    f.write("备份.txt", &valid_zip_fixture(&[("data.txt", b"zip")]), 10);
    f.write("数据.mp4", &seven_zip_fixture(&[1, 0]), 12);
    f.write("老包.png", &valid_empty_rar_fixture(), 14);
    // X-10 数字尾卷：整组后缀不可拆，识别到 zip 也不得改名（内容与其它样本不同，避免去重）。
    f.write(
        "分卷.zip.001",
        &valid_zip_fixture(&[("part.bin", b"volume")]),
        16,
    );
    // 未承诺的 ODF graphics 类型即使 ZIP 完整也保留，不降级成 zip。
    f.write(
        "图纸.odg",
        &valid_zip_fixture(&[
            ("mimetype", b"application/vnd.oasis.opendocument.graphics"),
            ("content.xml", b"<document-content/>"),
        ]),
        18,
    );
    let mut cfg = base();
    cfg.detect_type = true;
    cfg.fix_extension = true;
    let task = f.plan(cfg);
    let db = Database::open(&task.directory).unwrap();
    let moves = db.actions_page_filtered(0, 10, Some("move")).unwrap();
    drop(db);
    let planned: Vec<(String, String)> = moves
        .iter()
        .map(|a| (a.source.clone(), a.target.clone().unwrap_or_default()))
        .collect();
    for (source, expected) in [
        ("备份.txt", "压缩包/备份.zip".to_string()),
        ("数据.mp4", "压缩包/数据.7z".to_string()),
        ("老包.png", "压缩包/老包.rar".to_string()),
        ("分卷.zip.001", "压缩包/分卷.zip.001".to_string()),
        ("图纸.odg", "其他/图纸.odg".to_string()),
    ] {
        let actual = planned.iter().find(|(s, _)| s == source).map(|(_, t)| t);
        assert_eq!(
            actual.map(String::as_str),
            Some(expected.as_str()),
            "{source} 应计划为 {expected}；实际 {planned:#?}"
        );
    }
    Fixture::apply(&task);
    for (_, target) in planned {
        assert!(f.root.join(target).is_file(), "计划目标必须实际落盘");
    }
}
