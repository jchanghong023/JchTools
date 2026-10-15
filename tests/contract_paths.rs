#![warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    future_incompatible,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]

//! 覆盖 H-07 / X-04：冲突只改文件名主体，保留原扩展名和复合扩展名。
#![allow(clippy::unwrap_used, clippy::expect_used)]
use jchtools_file_core::fsutil;
use std::fs;

#[test]
fn archive_collision_preserves_compound_extension_and_existing_files() {
    let root = tempfile::tempdir().unwrap();
    let original = root.path().join("资料.tar.gz");
    let first = root.path().join("资料 (1).tar.gz");
    fs::write(&original, b"original").unwrap();
    fs::write(&first, b"first").unwrap();
    let target = fsutil::unique_target(root.path(), &original).unwrap();
    assert_eq!(target.file_name().unwrap(), "资料 (2).tar.gz");
    assert_eq!(fs::read(&original).unwrap(), b"original");
    assert_eq!(fs::read(&first).unwrap(), b"first");
}
#[test]
fn git_marker_name_is_case_insensitive() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join(".GIT"), b"gitdir: elsewhere").unwrap();
    assert!(fsutil::is_git_root(root.path()).unwrap());
}

// 覆盖 H-07 / X-04：合法 Unicode 空白属于原扩展名，冲突改名不得隐式修剪。
#[test]
fn archive_collision_preserves_unicode_whitespace_in_extension() {
    let root = tempfile::tempdir().unwrap();
    for whitespace in ['\u{00a0}', '\u{2003}', '\u{3000}'] {
        let name = format!("资料.txt{whitespace}");
        fsutil::validate_component(&name).unwrap();
        let original = root.path().join(&name);
        fs::write(&original, b"existing").unwrap();
        let target = fsutil::unique_target(root.path(), &original).unwrap();
        assert_eq!(
            target.file_name().unwrap().to_str().unwrap(),
            format!("资料 (1).txt{whitespace}"),
            "只允许修改主体，扩展名必须逐字符保留"
        );
        assert_eq!(fs::read(&original).unwrap(), b"existing");
    }
}

// 覆盖 S-05：受保护目录来自 Windows 自身，不信任可被启动环境改写的 SystemRoot。
// Windows API 边界只在 Windows 产品平台存在；子进程隔离避免修改其他用例的环境。
#[cfg(windows)]
#[test]
fn system_directory_protection_ignores_overridden_systemroot() {
    const CHILD: &str = "JCHTOOLS_SYSTEM_DIRECTORY_PROTECTION_EXPECTED";
    if let Some(expected) = std::env::var_os(CHILD) {
        let expected = std::path::PathBuf::from(expected);
        assert_eq!(fsutil::protected_root().unwrap(), expected);
        let error = fsutil::normalize_root(&expected).expect_err("真实系统目录必须被拒绝");
        assert!(format!("{error:#}").contains("Windows 系统目录"));
        return;
    }
    let fake_system_root = tempfile::tempdir().unwrap();
    let expected = fsutil::protected_root().unwrap();
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "system_directory_protection_ignores_overridden_systemroot", "--nocapture"])
        .env(CHILD, expected)
        .env("SystemRoot", fake_system_root.path());
    let output = jchtools_file_core::process::run_with_timeout(
        &mut command,
        std::time::Duration::from_secs(30),
    ).unwrap();
    assert!(
        output.status.success(),
        "环境隔离的系统目录保护断言失败：{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

// P2-09 / H-01 / C-08：有界的真实 PDF 输入预算回归，只消费隔离临时目录。
#[cfg(windows)]
mod review_remediation {
    use super::*;
    use jchtools_directory_organizer::engine;
    use jchtools_file_core::{
        config::Config, control::Context, db::Database, model::ActionKind,
    };
    use std::{
        io::{Read as _, Seek as _, Write as _},
        path::Path,
    };

    const PDF_INPUT_BUDGET: u64 = 16 * 1024 * 1024;
    const LARGE_PDF_PADDING: u64 = 17 * 1024 * 1024;

    // 一页空白 PDF，内容流仅含 q/Q 与合法空白；没有压缩、ObjStm 或 XRef 解压。
    // 按实际文件位置写交叉引用表，padding 以固定 8KiB 块写入，不构造大明文 Vec。
    fn write_pdf(path: &Path, padding: u64) {
        let mut file = fs::File::create(path).unwrap();
        file.write_all(b"%PDF-1.4\n").unwrap();
        let mut offsets = [0u64; 4];
        for (index, body) in [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R >>",
        ].into_iter().enumerate() {
            offsets[index] = file.stream_position().unwrap();
            writeln!(file, "{} 0 obj\n{body}\nendobj", index + 1).unwrap();
        }
        offsets[3] = file.stream_position().unwrap();
        writeln!(file, "4 0 obj\n<< /Length {} >>\nstream\nq", padding + 4).unwrap();
        let block = [b' '; 8192];
        let mut remaining = padding;
        while remaining > 0 {
            let count = usize::try_from(remaining.min(u64::try_from(block.len()).unwrap())).unwrap();
            file.write_all(&block[..count]).unwrap();
            remaining -= u64::try_from(count).unwrap();
        }
        file.write_all(b"Q\nendstream\nendobj\n").unwrap();
        let xref = file.stream_position().unwrap();
        file.write_all(b"xref\n0 5\n0000000000 65535 f \n").unwrap();
        for offset in offsets {
            writeln!(file, "{offset:010} 00000 n ").unwrap();
        }
        writeln!(file, "trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF").unwrap();
    }

    // 先按 vendored RC4/空用户密码模式加密已有 ObjStm，再追加合法增量 XRef，
    // 将 6 0 标记为 5 0 内的压缩对象。Catalog/Pages 保持普通对象，避免缺少
    // Catalog 让预算被吞的问题假绿。压缩器按块写空白，最大只展开约 17MiB。
    fn write_encrypted_object_stream_pdf(path: &Path, padding: u64) {
        use flate2::{Compression, write::ZlibEncoder};
        use lopdf::{Document, EncryptionState, EncryptionVersion, Object, Permissions, Stream, dictionary};

        let mut document = Document::with_version("1.7");
        let catalog = document.add_object(lopdf::dictionary! {
            "Type" => "Catalog", "Pages" => Object::Reference((2, 0)),
        });
        document.add_object(lopdf::dictionary! {
            "Type" => "Pages", "Kids" => vec![Object::Reference((3, 0))], "Count" => 1,
        });
        document.add_object(lopdf::dictionary! {
            "Type" => "Page", "Parent" => Object::Reference((2, 0)),
            "MediaBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
            "Contents" => Object::Reference((4, 0)),
        });
        document.add_object(Stream::new(lopdf::dictionary! {}, b"q\nQ\n".to_vec()));
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(b"6 0 42").unwrap();
        let block = [b' '; 8192];
        let mut remaining = padding;
        while remaining > 0 {
            let count = usize::try_from(remaining.min(u64::try_from(block.len()).unwrap())).unwrap();
            encoder.write_all(&block[..count]).unwrap();
            remaining -= u64::try_from(count).unwrap();
        }
        document.add_object(Stream::new(lopdf::dictionary! {
            "Type" => "ObjStm", "N" => 1, "First" => 4, "Filter" => "FlateDecode",
        }, encoder.finish().unwrap()));
        assert_eq!(document.new_object_id(), (6, 0));
        document.trailer.set("Root", catalog);
        let identifier = vec![0x42u8; 16];
        document.trailer.set("ID", Object::Array(vec![
            Object::string_literal(identifier.clone()),
            Object::string_literal(identifier.clone()),
        ]));
        let state = EncryptionState::try_from(EncryptionVersion::V2 {
            document: &document,
            owner_password: "owner",
            user_password: "",
            key_length: 128,
            permissions: Permissions::all(),
        }).unwrap();
        document.encrypt(&state).unwrap();
        let encrypt_id = document.trailer.get(b"Encrypt").unwrap().as_reference().unwrap();
        // 普通 writer 会丢弃 ObjStm；取出已由正式加密 API 加密的内容，
        // 在增量段原样序列化，并为容器和成员分别写正常/压缩 XRef 项。
        let encrypted_container = document.objects.remove(&(5, 0)).unwrap();
        let encrypted_stream = encrypted_container.as_stream().unwrap();
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        let marker = b"startxref\n";
        let marker_position = bytes.windows(marker.len()).rposition(|part| part == marker).unwrap();
        let previous_xref: u64 = std::str::from_utf8(&bytes[marker_position + marker.len()..])
            .unwrap().split_ascii_whitespace().next().unwrap().parse().unwrap();
        bytes.push(b'\n');
        let container_offset = u32::try_from(bytes.len()).unwrap();
        writeln!(
            bytes,
            "5 0 obj\n<< /Type /ObjStm /N 1 /First 4 /Filter /FlateDecode \
             /Length {} >>\nstream",
            encrypted_stream.content.len(),
        ).unwrap();
        bytes.extend_from_slice(&encrypted_stream.content);
        writeln!(bytes, "\nendstream\nendobj").unwrap();
        let xref_offset = u32::try_from(bytes.len()).unwrap();
        let xref_id = document.max_id + 1;
        let identifier = hex::encode(identifier);
        writeln!(bytes, "{xref_id} 0 obj").unwrap();
        writeln!(
            bytes,
            "<< /Type /XRef /Size {} /W [1 4 2] /Index [5 2 {xref_id} 1] \
             /Length 21 /Root 1 0 R /Encrypt {} {} R /ID [<{identifier}><{identifier}>] \
             /Prev {previous_xref} >>\nstream",
            xref_id + 1, encrypt_id.0, encrypt_id.1,
        ).unwrap();
        bytes.push(1);
        bytes.extend_from_slice(&container_offset.to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes());
        bytes.push(2);
        bytes.extend_from_slice(&5u32.to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes());
        bytes.push(1);
        bytes.extend_from_slice(&xref_offset.to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes());
        writeln!(bytes, "\nendstream\nendobj\nstartxref\n{xref_offset}\n%%EOF").unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn load_encrypted_fixture_without_limit(path: &Path) -> lopdf::Document {
        let document = lopdf::Document::load_from_with_options(
            fs::File::open(path).unwrap(),
            lopdf::LoadOptions { strict: true, ..lopdf::LoadOptions::default() },
        ).expect("合法加密夹具必须以空密码、严格模式、无限预算完整打开");
        assert!(document.was_encrypted());
        assert!(!document.is_encrypted());
        assert!(document.catalog().is_ok());
        assert_eq!(document.get_pages().len(), 1);
        assert_eq!(document.get_object((6, 0)).unwrap().as_i64().unwrap(), 42);
        assert!(matches!(
            document.reference_table.entries.get(&6),
            Some(lopdf::xref::XrefEntry::Compressed { container: 5, index: 0 })
        ), "必须实际读取压缩对象，不能靠扫描恢复绕开加密对象流分支");
        assert!(matches!(
            document.reference_table.entries.get(&1),
            Some(lopdf::xref::XrefEntry::Normal { .. })
        ));
        document
    }

    fn assert_object_stream_limit(document: &lopdf::Document, limit: usize) {
        let stream = document.get_object((5, 0)).unwrap().as_stream().unwrap();
        match lopdf::ObjectStream::new_with_limit(stream, Some(limit)) {
            Err(lopdf::Error::Decompress(lopdf::DecompressError::MemoryLimitExceeded {
                limit: actual,
            })) => assert_eq!(actual, limit),
            Err(error) => panic!("夹具必须触发解压预算而非密码或格式错误：{error:?}"),
            Ok(_) => panic!("夹具的合法对象流必须超过 {limit} 字节预算"),
        }
    }

    fn file_hash(path: &Path) -> blake3::Hash {
        let mut file = fs::File::open(path).unwrap();
        let mut hasher = blake3::Hasher::new();
        let mut block = [0u8; 8192];
        loop {
            let count = file.read(&mut block).unwrap();
            if count == 0 {
                return hasher.finalize();
            }
            hasher.update(&block[..count]);
        }
    }

    fn assert_pdf_prepare_and_apply(padding: u64, expected_extension: &str) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("data");
        fs::create_dir(&root).unwrap();
        let source = root.join("sample.wrong");
        write_pdf(&source, padding);
        let source_length = fs::metadata(&source).unwrap().len();
        if padding == LARGE_PDF_PADDING {
            assert!(source_length > PDF_INPUT_BUDGET);
            assert!(source_length < 18 * 1024 * 1024, "夹具体积必须有界");
        } else {
            assert!(source_length < PDF_INPUT_BUDGET);
        }
        {
            // 独立确认两个夹具均被当前严格 PDF 解析器接受，避免畸形输入造成假绿。
            // 这不是生产预算入口：大夹具本身仍是有效、可识别的普通 PDF。
            let document = lopdf::Document::load_from_with_options(
                fs::File::open(&source).unwrap(),
                lopdf::LoadOptions {
                    strict: true,
                    max_decompressed_size: Some(usize::try_from(PDF_INPUT_BUDGET).unwrap()),
                    ..lopdf::LoadOptions::default()
                },
            ).unwrap();
            assert!(document.catalog().is_ok());
            assert_eq!(document.get_pages().len(), 1);
        }
        assert_pdf_path_prepare_and_apply(&directory, &root, &source, expected_extension);
    }

    fn assert_pdf_path_prepare_and_apply(
        directory: &tempfile::TempDir,
        root: &Path,
        source: &Path,
        expected_extension: &str,
    ) {
        let source_length = fs::metadata(source).unwrap().len();
        let original_hash = file_hash(source);
        let prepared = engine::prepare_at(
            root,
            Config {
                fix_extension: true,
                detect_type: true,
                dedup_same_name: false,
                dedup_copy_names: false,
                dedup_other_names: false,
                clean_junk: false,
                clean_temp: false,
                clean_zero: false,
                clean_copy_name: false,
                normalize_names: false,
                hash_workers: 1,
                ..Config::default()
            },
            Context::default(),
            &directory.path().join("state"),
        ).unwrap();
        assert_eq!(prepared.summary.scanned, 1);
        assert_eq!(prepared.summary.scanned_bytes, source_length);
        assert_eq!(prepared.summary.errors, 0);
        assert_eq!(prepared.summary.planned_move, 1, "无法确认类型仍须正常归类");
        assert_eq!(fs::metadata(source).unwrap().len(), source_length);
        assert_eq!(file_hash(source), original_hash, "分析阶段不得改动源内容");
        let database = Database::open_existing(&prepared.directory).unwrap();
        assert_eq!(database.get::<String>("status").unwrap(), "ready");
        let actions = database.actions_page(0, 100).unwrap();
        let moves: Vec<_> = actions.iter().filter(|action| {
            action.kind == ActionKind::Move && action.source == "sample.wrong"
        }).collect();
        assert_eq!(moves.len(), 1);
        assert!(moves[0].selected);
        let target = moves[0].target.as_deref().expect("归类动作必须有目标");
        let destination = root.join(target);
        assert_eq!(
            destination.extension().unwrap(),
            expected_extension,
            "预算耗尽只保留扩展名，不得阻止正常归类计划"
        );
        let action_id = moves[0].id;
        drop(database);

        let applied = engine::apply(&prepared.directory, Context::default()).unwrap();
        assert_eq!(applied.summary.errors, 0);
        assert_eq!(applied.summary.moved, 1);
        assert!(!source.exists(), "源文件应仅由正常归类移动");
        assert_eq!(fs::metadata(&destination).unwrap().len(), source_length);
        assert_eq!(file_hash(&destination), original_hash, "执行必须保留完整源内容");
        let database = Database::open_existing(&prepared.directory).unwrap();
        assert_eq!(database.action(action_id).unwrap().state, "done");
        assert_eq!(
            destination.extension().unwrap(),
            expected_extension,
            "普通 PDF 应修正为 pdf；超过 16MiB 识别预算的有效 PDF 必须保留 wrong"
        );
    }

    #[test]
    fn review_remediation_pdf_ordinary_prepare_and_apply_keeps_content() {
        assert_pdf_prepare_and_apply(0, "pdf");
    }

    // 旧实现没有输入预算，会把这个合法的 17MiB PDF 改为 pdf，与 wrong 断言相反。
    // 此用例约束完整输入预算；加载期另限单个解压流，但均不证明 PDF 对象累计内存上界。
    #[test]
    fn review_remediation_pdf_input_budget_preserves_extension_and_applies() {
        assert_pdf_prepare_and_apply(LARGE_PDF_PADDING, "wrong");
    }

    fn assert_encrypted_pdf_prepare_and_apply(padding: u64, expected_extension: &str) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("data");
        fs::create_dir(&root).unwrap();
        let source = root.join("sample.wrong");
        write_encrypted_object_stream_pdf(&source, padding);
        assert!(fs::metadata(&source).unwrap().len() < 128 * 1024,
            "加密对象流夹具输入必须远小于 16MiB 输入预算");
        let document = load_encrypted_fixture_without_limit(&source);
        let limit = usize::try_from(PDF_INPUT_BUDGET).unwrap();
        if padding == LARGE_PDF_PADDING {
            assert_object_stream_limit(&document, limit);
        } else {
            let bounded = lopdf::Document::load_from_with_options(
                fs::File::open(&source).unwrap(),
                lopdf::LoadOptions {
                    strict: true,
                    max_decompressed_size: Some(limit),
                    ..lopdf::LoadOptions::default()
                },
            ).expect("普通加密 PDF 在应用单流预算内必须完整打开");
            assert_eq!(bounded.get_object((6, 0)).unwrap().as_i64().unwrap(), 42);
        }
        drop(document);
        assert_pdf_path_prepare_and_apply(&directory, &root, &source, expected_extension);
    }

    #[test]
    fn review_remediation_pdf_encrypted_ordinary_prepare_and_apply_keeps_content() {
        assert_encrypted_pdf_prepare_and_apply(0, "pdf");
    }

    // 输入只有数十 KiB；仅合法 ObjStm 的展开超过 16MiB。
    // 加密分支吞 MemoryLimitExceeded 时会错误规划为 pdf，而非保留 wrong。
    #[test]
    fn review_remediation_pdf_encrypted_object_stream_budget_preserves_extension_and_applies() {
        assert_encrypted_pdf_prepare_and_apply(LARGE_PDF_PADDING, "wrong");
    }

    #[test]
    fn review_remediation_pdf_encrypted_object_stream_load_propagates_memory_limit() {
        const LIMIT: usize = 1024;
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("encrypted.pdf");
        write_encrypted_object_stream_pdf(&source, 4096);
        let document = load_encrypted_fixture_without_limit(&source);
        assert_object_stream_limit(&document, LIMIT);
        drop(document);
        let result = lopdf::Document::load_from_with_options(
            fs::File::open(&source).unwrap(),
            lopdf::LoadOptions {
                strict: true,
                max_decompressed_size: Some(LIMIT),
                ..lopdf::LoadOptions::default()
            },
        );
        match result {
            Err(lopdf::Error::Decompress(lopdf::DecompressError::MemoryLimitExceeded {
                limit,
            })) => assert_eq!(limit, LIMIT),
            Err(error) => panic!("必须上抛对象流解压预算错误，而非密码或格式错误：{error:?}"),
            Ok(_) => panic!("加密 PDF 加载不得吞掉对象流的 MemoryLimitExceeded"),
        }
    }
}

