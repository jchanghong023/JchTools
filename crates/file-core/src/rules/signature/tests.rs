// 输入元数据只作提前退出优化；读取端也必须有界并拒绝超预算前缀。
#[test]
fn pdf_probe_bounds_reads_even_when_length_metadata_is_stale() {
    use std::io::Read as _;

    let mut oversized = std::io::repeat(b' ').take(super::MAX_TYPE_METADATA + 2);
    assert_eq!(super::pdf_extension(&mut oversized, 0), None);
    assert_eq!(oversized.limit(), 1, "最多读取预算加一个超限探测字节");

    let mut untouched = b"%PDF-".as_slice();
    assert_eq!(
        super::pdf_extension(&mut untouched, super::MAX_TYPE_METADATA + 1),
        None
    );
    assert_eq!(untouched, b"%PDF-", "已知超预算时不得加载正文");
}

// 覆盖 C-08 / 附录 B：OPC 根级关联按包内 URI 点段解析，不能越过包根。
#[test]
fn ooxml_main_relationship_resolves_internal_package_paths() {
    use std::io::{Cursor, Write as _};
    for (target, expected) in [
        ("word/document.xml", Some("docx")),
        ("/word/document.xml", Some("docx")),
        ("./word/document.xml", Some("docx")),
        ("word/../word/document.xml", Some("docx")),
        ("../word/document.xml", None),
        ("word/other.xml", None),
        ("//word/document.xml", None),
        ("word//document.xml", None),
        ("word/document.xml/", None),
        ("word/document.xml/.", None),
        ("word/document.xml/child/..", None),
        ("https://example.test/document.xml", None),
    ] {
        let relationships = format!(
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="{target}"/></Relationships>"#
        );
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, contents) in [
            (
                "[Content_Types].xml",
                r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#,
            ),
            (
                "word/document.xml",
                r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body/></w:document>"#,
            ),
            ("_rels/.rels", relationships.as_str()),
        ] {
            writer
                .start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(contents.as_bytes()).unwrap();
        }
        let encoded = writer.finish().unwrap().into_inner();
        let length = u64::try_from(encoded.len()).unwrap();
        let archive = zip::ZipArchive::new(Cursor::new(encoded)).unwrap();
        assert_eq!(super::zip_extension(archive, length), expected, "{target}");
    }
}

fn review_word_package(target: &str, part: &str, members: &[&str], mode: &str) -> Vec<u8> {
    use std::io::{Cursor, Write as _};
    let types = format!(
        r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/{part}" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#
    );
    let relationships = format!(
        r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="{target}" {mode}/></Relationships>"#
    );
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let entries = [
        ("[Content_Types].xml", types.as_str()),
        ("_rels/.rels", relationships.as_str()),
    ];
    for (name, contents) in entries.into_iter().chain(members.iter().map(|member| {
        (
            *member,
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body/></w:document>"#,
        )
    })) {
        writer
            .start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(contents.as_bytes()).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

// 覆盖 C-08 / CONTRACT 附录 B：OPC 部件等价按 ASCII 忽略大小写，
// 关联、Override 与真实 ZIP 成员的大小写不同仍须识别有效文档。
#[test]
fn review_fix_ooxml_part_names_are_ascii_case_insensitive() {
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
        let encoded = review_word_package(target, part, &[member], "");
        let length = u64::try_from(encoded.len()).unwrap();
        let archive = zip::ZipArchive::new(std::io::Cursor::new(encoded)).unwrap();
        assert_eq!(
            super::zip_extension(archive, length),
            Some("docx"),
            "Target={target}, Override={part}, member={member}"
        );
    }
}

// 覆盖 C-08 / CONTRACT 附录 B：包内绝对路径和相对路径的非首段允许冒号，
// 不能把合法部件名中的冒号误判为外部 URI scheme。
#[test]
fn review_fix_ooxml_internal_part_segment_allows_colon() {
    for target in ["/word/document:1.xml", "word/document:1.xml"] {
        let encoded = review_word_package(
            target,
            "word/document:1.xml",
            &["word/document:1.xml"],
            r#"TargetMode="Internal""#,
        );
        let length = u64::try_from(encoded.len()).unwrap();
        let archive = zip::ZipArchive::new(std::io::Cursor::new(encoded)).unwrap();
        assert_eq!(
            super::zip_extension(archive, length),
            Some("docx"),
            "{target}"
        );
    }
}

// 覆盖 C-08 / CONTRACT 附录 B / S-01：放行路径段冒号不得放行 scheme、
// authority、外部关联、query 或 fragment；这些声明不授权猜测类型。
#[test]
fn review_fix_ooxml_non_internal_uri_references_remain_rejected() {
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
        let encoded = review_word_package(target, member, &[member], mode);
        let length = u64::try_from(encoded.len()).unwrap();
        let archive = zip::ZipArchive::new(std::io::Cursor::new(encoded)).unwrap();
        assert_eq!(super::zip_extension(archive, length), None, "{target}");
    }
    // 缺失主部件和大小写等价的重复部件都不能授权 DOCX 或 ZIP 后缀。
    for members in [&[][..], &["word/document.xml", "WORD/document.xml"][..]] {
        let encoded = review_word_package("word/document.xml", "word/document.xml", members, "");
        let length = u64::try_from(encoded.len()).unwrap();
        let archive = zip::ZipArchive::new(std::io::Cursor::new(encoded)).unwrap();
        assert_eq!(super::zip_extension(archive, length), None, "{members:?}");
    }
}

fn wav_fixture(bits: u16, alignment: u16, byte_rate: u32, data: &[u8]) -> Vec<u8> {
    let mut encoded = b"RIFF".to_vec();
    encoded.extend_from_slice(&(36 + u32::try_from(data.len()).unwrap()).to_le_bytes());
    encoded.extend_from_slice(b"WAVEfmt \x10\0\0\0");
    encoded.extend_from_slice(&1u16.to_le_bytes());
    encoded.extend_from_slice(&1u16.to_le_bytes());
    encoded.extend_from_slice(&8_000u32.to_le_bytes());
    encoded.extend_from_slice(&byte_rate.to_le_bytes());
    encoded.extend_from_slice(&alignment.to_le_bytes());
    encoded.extend_from_slice(&bits.to_le_bytes());
    encoded.extend_from_slice(b"data");
    encoded.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
    encoded.extend_from_slice(data);
    encoded
}

// 覆盖 C-08 / 附录 B：完整 RIFF 边界不能豁免矛盾的 PCM 格式与不完整采样帧。
#[test]
fn wav_pcm_requires_consistent_sample_layout() {
    for (bits, alignment, byte_rate, data) in [
        (0, 2, 16_000, &[0, 0][..]),
        (16, 1, 8_000, &[0, 0][..]),
        (16, 2, 0, &[0, 0][..]),
        (16, 2, 16_000, &[0, 0, 0, 0, 0, 0][..]),
    ] {
        let mut encoded = wav_fixture(bits, alignment, byte_rate, data);
        if data.len() == 6 {
            // 改为立体声：6 字节不能组成完整的 4 字节采样帧。
            encoded[22..24].copy_from_slice(&2u16.to_le_bytes());
            encoded[28..32].copy_from_slice(&32_000u32.to_le_bytes());
            encoded[32..34].copy_from_slice(&4u16.to_le_bytes());
        }
        let length = u64::try_from(encoded.len()).unwrap();
        assert!(!super::valid_wav(&mut std::io::Cursor::new(encoded), length).unwrap());
    }
    let encoded = wav_fixture(16, 2, 16_000, &[0, 0]);
    let length = u64::try_from(encoded.len()).unwrap();
    assert!(super::valid_wav(&mut std::io::Cursor::new(encoded), length).unwrap());
    for (depth, alignment, byte_rate, expected) in [(16, 2, 16_000, false), (32, 4, 32_000, true)] {
        let mut encoded = wav_fixture(depth, alignment, byte_rate, &[0; 4]);
        encoded[20..22].copy_from_slice(&3u16.to_le_bytes()); // IEEE float
        encoded[16..20].copy_from_slice(&18u32.to_le_bytes());
        drop(encoded.splice(36..36, [0u8; 2])); // WAVEFORMATEX 的 cbSize = 0。
        drop(encoded.splice(38..38, b"fact\x04\0\0\0\x01\0\0\0".iter().copied()));
        let riff_size = u32::try_from(encoded.len() - 8).unwrap();
        encoded[4..8].copy_from_slice(&riff_size.to_le_bytes());
        let length = u64::try_from(encoded.len()).unwrap();
        assert_eq!(
            super::valid_wav(&mut std::io::Cursor::new(encoded), length).unwrap(),
            expected
        );
    }
}

// 覆盖 C-08 / 附录 B：真实无损 WebP 编码的最大宽/高都须经公开签名入口识别。
#[test]
fn webp_maximum_lossless_dimensions_are_detected() {
    let directory = tempfile::tempdir().expect("创建签名测试目录");
    let pixels = vec![73u8; 16_384 * 3];
    for (width, height) in [(16_384, 1), (1, 16_384)] {
        let mut encoded = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut encoded)
            .encode(&pixels, width, height, image::ExtendedColorType::Rgb8)
            .expect("合法的 16384 单元无损 WebP 应能编码");
        let path = directory.path().join(format!("{width}x{height}.bin"));
        std::fs::write(&path, &encoded).expect("写入真实 WebP 夹具");
        let detected = crate::rules::detect_extension(&path, &crate::control::Control::default())
            .expect("公开签名识别成功");
        assert_eq!(detected, Some("webp"), "{width}x{height}");
        let extension = detected.expect("必须确认 WebP 类型");
        assert!(crate::rules::extension_needs_fix("bin", extension));
        assert_eq!(
            crate::rules::category_for(&format!("photo.{extension}")),
            "图片"
        );
        assert_eq!(
            std::fs::read(&path).expect("重读夹具"),
            encoded,
            "签名识别只读"
        );
    }
}
