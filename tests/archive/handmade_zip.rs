use super::*;

// 覆盖 X-04
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn zip_with_duplicate_entries_extracts_both_via_auto_rename() {
    // 同名两条目（内容与大小都不同）都必须落盘，正式目标冲突按 H-07/X-04
    // 只给新文件主体加最小未占用序号，不把引擎临时区的改名格式当作产品契约。
    let f = ArchiveFixture::new();
    fs::write(
        f.root.join("dup.zip"),
        stored_zip(&[
            (b"dup.txt", b"first-10bytes"),
            (b"dup.txt", b"second-payload-16B"),
        ]),
    )
    .unwrap();
    let result = f.run(config());
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(result.summary.archives_ok, 1);
    assert_eq!(
        result.summary.extracted, 2,
        "两个同名条目都应完整落盘（X-04）"
    );
    // 正式落点遵守 H-07 的 `主体 (N).后缀`，与 7-Zip 临时区内部名称无关。
    assert_eq!(fs::read(f.root.join("dup.txt")).unwrap(), b"first-10bytes");
    assert_eq!(
        fs::read(f.root.join("dup (1).txt")).unwrap(),
        b"second-payload-16B"
    );
}

// 覆盖 H-07/X-04：相同大小的重复成员不丢失，真实 `_1` 文件名不被当作引擎改名。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn duplicate_members_preserve_literal_engine_style_names_and_existing_files() {
    let f = ArchiveFixture::new();
    fs::write(f.root.join("same.txt"), b"old-original").unwrap();
    fs::write(f.root.join("same (1).txt"), b"old-numbered").unwrap();
    fs::write(f.root.join("same_1.txt"), b"old-literal").unwrap();
    fs::write(
        f.root.join("duplicate.zip"),
        stored_zip(&[
            (b"same.txt", b"AAA"),
            (b"same.txt", b"BBB"),
            (b"same_1.txt", b"LLL"),
        ]),
    )
    .unwrap();
    let result = f.run(config());
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(result.summary.archives_ok, 1);
    assert_eq!(result.summary.extracted, 3);
    assert_eq!(fs::read(f.root.join("same.txt")).unwrap(), b"old-original");
    assert_eq!(
        fs::read(f.root.join("same (1).txt")).unwrap(),
        b"old-numbered"
    );
    assert_eq!(fs::read(f.root.join("same_1.txt")).unwrap(), b"old-literal");
    assert_eq!(fs::read(f.root.join("same (2).txt")).unwrap(), b"AAA");
    assert_eq!(fs::read(f.root.join("same (3).txt")).unwrap(), b"BBB");
    assert_eq!(fs::read(f.root.join("same_1 (1).txt")).unwrap(), b"LLL");
    assert!(!f.root.join("duplicate.zip").exists());
}
// 覆盖 X-08
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn gbk_filename_zip_extracts_without_data_loss() {
    // GBK 编码文件名（"报告.txt" 的 GBK 字节）且不带 UTF-8 标志的旧式 zip：
    // 文件名可能与本机代码页不一致而呈乱码，但内容必须完整落盘且路径组件安全。
    let f = ArchiveFixture::new();
    let payload = b"gbk-payload\n!";
    let gbk_name: &[u8] = &[0xB1, 0xA8, 0xB8, 0xE6, b'.', b't', b'x', b't']; // GBK"报告.txt"
    fs::write(f.root.join("gbk.zip"), stored_zip(&[(gbk_name, payload)])).unwrap();
    let result = f.run(config());
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(result.summary.archives_ok, 1);
    assert_eq!(result.summary.extracted, 1);
    let extracted: Vec<_> = fs::read_dir(&f.root)
        .unwrap()
        .map(|entry| entry.expect("读取解压结果目录项失败"))
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("txt")))
        .collect();
    assert_eq!(
        extracted.len(),
        1,
        "应恰好落盘一个 .txt 文件（名字可能呈乱码）"
    );
    assert_eq!(fs::read(&extracted[0]).unwrap(), payload);
}
// 覆盖 X-05
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn empty_zip_archive_extracts_cleanly() {
    // 只有 EOCD 的空 zip：合法归档，零成员零字节，按成功解压处理。
    let f = ArchiveFixture::new();
    fs::write(f.root.join("empty.zip"), stored_zip(&[])).unwrap();
    let cfg = config();
    let result = f.run(cfg);
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(result.summary.archives_ok, 1);
    assert_eq!(result.summary.extracted, 0);
    assert!(
        !f.root.join("empty.zip").exists(),
        "零成员的空包同样算完整解开：原包按 X-05 永久删除"
    );
    assert_eq!(result.summary.deleted, 1, "空包删除同样计入删除计数");
}
// 覆盖 X-05
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn empty_7z_with_only_a_directory_entry_restores_the_directory() {
    // 只含一个空目录条目的 7z：解压后应还原出目录本身。
    let f = ArchiveFixture::new();
    fs::create_dir(f.input.join("空目录")).unwrap();
    f.pack(&f.root.join("empty.7z"), &["-t7z"], &["空目录"]);
    let cfg = config();
    let result = f.run(cfg);
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(result.summary.archives_ok, 1);
    assert_eq!(result.summary.extracted, 0);
    assert!(
        f.root.join("空目录").is_dir(),
        "仅目录条目的 7z 也应还原出目录"
    );
    assert!(
        !f.root.join("empty.7z").exists(),
        "只含目录条目的包同样完整落盘（含空目录）：原包按 X-05 永久删除"
    );
}
// 覆盖 X-03
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn zstd_stream_archive_extracts_with_content() {
    // 7-Zip 只解不建 .zst，样本按 RFC 8878 手工构造：
    // magic + 单段帧头（1 字节帧内容大小）+ 一个"最后一块、原始块"，内容直接内联。
    let f = ArchiveFixture::new();
    let payload = b"zst payload here\n";
    let mut zst = b"\x28\xB5\x2F\xFD".to_vec(); // Zstandard magic
    zst.push(0x20); // 单段帧 + 帧内容大小 1 字节
    zst.push(u8::try_from(payload.len()).unwrap()); // Frame_Content_Size
    let block_header = 1u32 | u32::try_from(payload.len()).unwrap() << 3; // 最后一块 · 原始块 · 大小
    zst.extend_from_slice(&block_header.to_le_bytes()[..3]);
    zst.extend_from_slice(payload);
    fs::write(f.root.join("payload.txt.zst"), zst).unwrap();
    let result = f.run(config());
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(result.summary.archives_ok, 1);
    assert_eq!(fs::read(f.root.join("payload.txt")).unwrap(), payload);
}

// 覆盖 X-01/X-04/X-05/X-08/H-07：gzip 原名碰到源包时保留 .gz，后续仍按嵌套包处理。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn gzip_member_matching_source_keeps_extension_and_nested_processing() {
    let f = ArchiveFixture::new();
    let payload = b"recorded member payload\n";
    // RFC 1952 FNAME + RFC 1951 单个未压缩块，独立保留原始成员名，不依赖压缩器改名。
    let mut gzip = vec![0x1f, 0x8b, 8, 8, 0, 0, 0, 0, 0, 3];
    gzip.extend_from_slice(b"same.gz\0");
    gzip.push(1);
    let size = u16::try_from(payload.len()).unwrap();
    gzip.extend_from_slice(&size.to_le_bytes());
    gzip.extend_from_slice(&(!size).to_le_bytes());
    gzip.extend_from_slice(payload);
    gzip.extend_from_slice(&crc32(payload).to_le_bytes());
    gzip.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
    fs::write(f.root.join("same.gz"), gzip).unwrap();
    let result = f.run(Config {
        max_depth: 1,
        ..config()
    });
    assert_eq!(result.summary.archives_ok, 1, "外层完整落盘后独立成功");
    assert_eq!(
        result.summary.archives_failed, 1,
        "保留 .gz 的成员仍须进入嵌套处理并按深度上限隔离"
    );
    assert_eq!(result.summary.deleted, 1, "只删除完整成功的外层源包");
    assert_eq!(result.summary.archives_quarantined, 1);
    assert!(!f.root.join("same.gz").exists());
    assert!(
        !f.root.join("same").exists(),
        "不得剥掉记录的扩展名来绕过递归"
    );
    assert_eq!(
        fs::read(f.root.join("解压失败/same (1).gz")).unwrap(),
        payload,
        "碰到尚未删除的源包时，只改成员主体；后续隔离仍保留原始解码字节"
    );
}
