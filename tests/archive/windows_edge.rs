use super::*;

// 平台门禁原因：触发条件本身是 Windows 路径长度语义（>260 字符 + LongPathsEnabled 默认 0 时
// 裸 Win32 调用失败），且植入隐藏属性需要 SetFileAttributesW，均无法在非 Windows 复现。
// 覆盖 S-04(扫描隐藏), X-08
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
#[cfg(windows)]
fn long_path_hidden_member_is_stripped_and_archive_completes() {
    // 管线级行为锚点：>260 字符且存储了隐藏属性的成员（7z 会还原属性），在默认
    // include_hidden=false 下必须被剥离成可见文件，且 complete 不得被误置——
    // 否则成员成扫描不可见的影子文件而原包被删除，用户视角即内容丢失。
    // 注意：本测试经 extract_run_at 的 root 已 canonicalize（verbatim），锁的是管线级契约；
    // 「普通路径 + 超长」的原始缺陷形态由 archive.rs 内对 normalize 的直测锚点锁定。
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::{SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN};
    let f = ArchiveFixture::new();
    let mut dir = f.input.clone();
    let seg = "l".repeat(40);
    for i in 0..7 {
        dir.push(format!("{seg}{i}"));
    } // 相对路径 ≈300 字符，远超 260
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("deep-hidden.txt");
    fs::write(&file, b"deep hidden payload").unwrap();
    // 测试自身植入属性必须走 \\?\ 前缀：LongPathsEnabled=0 的机器上裸调用同样会失败
    //（这正是本次整改所针对的产品缺陷，前置若用裸调用连夹具都搭不起来）。
    let mut wide: Vec<u16> = r"\\?\".encode_utf16().collect();
    wide.extend(file.as_os_str().encode_wide());
    wide.push(0);
    // SAFETY: wide 是以 NUL 结尾的 UTF-16 verbatim 路径；调用只读取该缓冲区。
    let ok = unsafe { SetFileAttributesW(wide.as_ptr(), FILE_ATTRIBUTE_HIDDEN) };
    assert!(ok != 0, "测试前置：植入隐藏属性失败");
    let meta = fs::symlink_metadata(&file).unwrap();
    assert_ne!(meta.file_attributes() & 2, 0, "测试前置：隐藏属性应已植入");
    f.archive(&f.root.join("deep.7z"), "-t7z");
    // 隐藏开关仍按 S-04 保留给用户显式调整：本用例验证关闭时成员属性被剥离（默认
    // 开启包含隐藏文件，属性本就无需剥离）。
    let cfg = Config {
        include_hidden: false,
        ..config()
    };
    let result = f.run(cfg);
    assert_eq!(result.summary.archives_failed, 0, "超长路径包不得计入失败");
    assert_eq!(result.summary.extracted, 1);
    let rel = dir.strip_prefix(&f.input).unwrap();
    let dest = f.root.join(rel).join("deep-hidden.txt");
    let dest_meta = fs::symlink_metadata(&dest)
        .unwrap_or_else(|e| panic!("成员应按原相对路径落盘 {dest:?}: {e}"));
    assert_eq!(
        dest_meta.file_attributes() & 2,
        0,
        "隐藏属性必须被剥离，否则成员成扫描不可见的影子文件"
    );
    assert!(
        !f.root.join("deep.7z").exists() && !f.root.join("解压失败/deep.7z").exists(),
        "complete 为真时原包按 X-05 永久删除；complete 被误置 false 的包会留在「解压失败」"
    );
}

// 覆盖 X-05/X-09/X-10：精确旧式 zip 尾卷为 .z01～.z99，.z001 不属于该卷族。
// 引擎能够完整打开 report.zip 不改变命名识别边界；非精确相似后缀必须原样保留。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn real_engine_standalone_zip_deletes_source_and_keeps_stale_z_sibling() {
    let f = ArchiveFixture::new();
    fs::write(f.input.join("one.txt"), b"real standalone payload").unwrap();
    fs::write(f.input.join("two.txt"), b"second member").unwrap();
    f.archive(&f.root.join("report.zip"), "-tzip");
    fs::write(
        f.root.join("report.z001"),
        b"stale fragment of a replaced split set",
    )
    .unwrap();
    let cfg = config();
    let result = f.run(cfg);
    assert_eq!(result.summary.archives_failed, 0);
    assert_eq!(result.summary.archives_ok, 1);
    assert!(
        f.root.join("one.txt").exists() && f.root.join("two.txt").exists(),
        "成员应正常解压落盘"
    );
    assert!(
        !f.root.join("report.zip").exists(),
        "成功原包按 X-05 永久删除"
    );
    assert!(
        f.root.join("report.z001").exists(),
        "非精确 .z001 后缀不是旧式 zip 尾卷，不得被移动或删除（X-09/X-10）"
    );
    assert_eq!(
        result.summary.deleted, 1,
        "删除集合只含主体自身：非精确 .z001 不得计入"
    );
    assert_eq!(
        fs::read(f.root.join("report.z001")).unwrap(),
        b"stale fragment of a replaced split set"
    );
}

// 覆盖 X-08/X-10/S-01：归档注释内的伪造卷数不能扩张 X-10 的实际输入卷集合。
// .z001～.z008 不匹配两位旧式 zip 尾卷，不能因为注释声称多卷而吸入比例分母，
// 也不能被删源或隔离；本例分母只有 report.zip 自身。
#[test]
#[ignore = "Requires explicitly provided real 7-Zip engine"]
fn real_engine_brace_in_comment_cannot_inflate_ratio_denominator() {
    fn add_zip_comment(path: &std::path::Path, comment: &[u8]) {
        let mut data = fs::read(path).unwrap();
        let signature = [0x50u8, 0x4b, 0x05, 0x06];
        let eocd = data
            .windows(4)
            .rposition(|window| window == signature)
            .expect("zip EOCD 签名");
        assert_eq!(
            data.len(),
            eocd + 22,
            "测试前置：期望无既有注释的 EOCD 结尾"
        );
        let length = u16::try_from(comment.len()).expect("注释长度须在 u16 内");
        data[eocd + 20] = (length & 0xff) as u8;
        data[eocd + 21] = (length >> 8) as u8;
        data.extend_from_slice(comment);
        fs::write(path, data).unwrap();
    }
    let f = ArchiveFixture::new();
    // 可压缩内容：声明总量 3 × 4096 = 12 KiB，实际包体只有几百字节。
    for i in 0..3 {
        fs::write(f.input.join(format!("z{i}.txt")), vec![0u8; 4096]).unwrap();
    }
    f.archive(&f.root.join("report.zip"), "-tzip");
    add_zip_comment(
        &f.root.join("report.zip"),
        b"benign first line
}
Volume Index = 0
Volumes = 9
",
    );
    // 三位 z 后缀是白名单之外的旁观文件；若改用 .z01～.z08，
    // 按 X-10 就会真实归组，分母理应包含它们，不能继续断言只计主体。
    for index in 1..=8 {
        fs::write(
            f.root.join(format!("report.z{index:03}")),
            vec![0u8; 100_000],
        )
        .unwrap();
    }
    let packed = fs::metadata(f.root.join("report.zip")).unwrap().len();
    assert!(packed * 2 < 12_288, "测试前置：包体应远小于声明总量");
    let cfg = Config {
        max_ratio: 2,
        ..config()
    };
    let result = f.run(cfg);
    assert_eq!(
        result.summary.archives_failed, 1,
        "分母只应计主体（{packed} 字节）：12 KiB 声明量必然超过比例上限；\
         注释伪造的 Volumes=9 不得把非精确后缀文件计入分母"
    );
    assert!(
        f.root.join("解压失败/report.zip").is_file(),
        "判超限的包移入「解压失败」（X-06）"
    );
    assert_eq!(result.summary.archives_ok, 0);
    assert_eq!(result.summary.deleted, 0, "超限包及旁观文件均不得删除");
    for index in 1..=8 {
        assert_eq!(
            fs::read(f.root.join(format!("report.z{index:03}"))).unwrap(),
            vec![0u8; 100_000],
            "非精确卷后缀旁观文件须原样留在原处（X-09/X-10）"
        );
    }
}
