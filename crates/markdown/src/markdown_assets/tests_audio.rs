    use super::load_manifest;
    use super::tests_support::{redirect_component_env, sha256_bytes};
    use jchtools_assets::asset_util::AssetDownloader;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;

    struct AudioFixture {
        receipt: super::ComponentReceipt,
        archive: Vec<u8>,
        files: Vec<(String, Vec<u8>)>,
        requests: Vec<String>,
        cancel_download: bool,
    }

    impl AudioFixture {
        fn new() -> Self {
            let files: Vec<(String, Vec<u8>)> = [
                ("ffmpeg/avcodec-63.dll", b"codec".as_slice()),
                ("ffmpeg/LICENSE.txt", b"audio license".as_slice()),
                ("ffmpeg/avformat-63.dll", b"format".as_slice()),
                ("ffmpeg/avutil-61.dll", b"utility".as_slice()),
                ("ffmpeg/swresample-7.dll", b"resample".as_slice()),
                ("models/vad/silero_vad.onnx", b"vad".as_slice()),
                ("models/sense_voice_zh_en_ja_ko_yue_2024_07_17/model.int8.onnx", b"voice".as_slice()),
                ("models/sense_voice_zh_en_ja_ko_yue_2024_07_17/tokens.txt", b"tokens".as_slice()),
            ].into_iter().map(|(path, bytes)| (path.into(), bytes.to_vec())).collect::<Vec<_>>();
            let member = |path: &str, bytes: &[u8]| super::AssetFile {
                path: path.into(), size_bytes: bytes.len() as u64, sha256: sha256_bytes(bytes),
            };
            let components = ["document", "snapshot"].into_iter().map(|id| {
                super::FeatureComponent {
                    id: id.into(), archive: format!("xberg-{id}-{}.zip", super::XBERG_TARGET),
                    requires: Vec::new(), size_bytes: 1, sha256: sha256_bytes(b"base zip"),
                    members: vec![member("xberg.exe", b"base engine"), member("LICENSE", b"base license")],
                }
            }).collect();
            let mut fixture = Self {
                receipt: super::ComponentReceipt {
                    owner: "JchTools-bundled-xberg-v1".into(), tag: "v2099.1".into(),
                    source: super::XBERG_LATEST_RELEASE_URL.into(),
                    manifest: super::ReleaseAsset {
                        archive: super::XBERG_COMPONENT_MANIFEST.into(),
                        url: format!("https://github.com/jchanghong023/xberg/releases/download/v2099.1/{}", super::XBERG_COMPONENT_MANIFEST),
                        size_bytes: 1, sha256: sha256_bytes(b"manifest"),
                    },
                    components,
                    audio_assets: super::AudioAsset {
                        component: super::FeatureComponent {
                            id: "audio-assets".into(), archive: super::AUDIO_ARCHIVE.into(),
                            requires: Vec::new(), size_bytes: 1, sha256: String::new(),
                            members: files.iter().map(|(path, bytes)| member(path, bytes)).collect(),
                        },
                        url: format!("https://github.com/jchanghong023/xberg/releases/download/v2099.1/{}", super::AUDIO_ARCHIVE),
                    },
                },
                archive: Vec::new(), files, requests: Vec::new(), cancel_download: false,
            };
            fixture.rebuild_archive();
            fixture
        }

        fn rebuild_archive(&mut self) {
            use std::io::{Cursor, Write};
            let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
            for (path, bytes) in &self.files {
                zip.start_file(
                    format!("{}/{path}", super::XBERG_ARCHIVE_PREFIX),
                    zip::write::SimpleFileOptions::default(),
                ).unwrap();
                zip.write_all(bytes).unwrap();
            }
            self.archive = zip.finish().unwrap().into_inner();
            self.receipt.audio_assets.component.size_bytes = self.archive.len() as u64;
            self.receipt.audio_assets.component.sha256 = sha256_bytes(&self.archive);
        }

        fn prepare(&self, root: &Path) {
            fs::create_dir_all(root).unwrap();
            for component in &self.receipt.components {
                for member in &component.members {
                    let path = root.join(&member.path);
                    fs::create_dir_all(path.parent().unwrap()).unwrap();
                    fs::write(path, if member.path == "xberg.exe" { b"base engine".as_slice() } else { b"base license".as_slice() }).unwrap();
                }
            }
            self.write_receipt(root);
        }

        fn write_receipt(&self, root: &Path) {
            fs::write(root.join(super::XBERG_RELEASE_RECEIPT), serde_json::to_vec(&self.receipt).unwrap()).unwrap();
        }

        fn install(&mut self) -> Result<PathBuf, String> {
            super::download_audio_runtime_with_downloader(&AtomicBool::new(false), |_| {}, self)
        }

        fn write_audio(&self, root: &Path) {
            for (relative, bytes) in &self.files {
                let path = root.join(relative);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, bytes).unwrap();
            }
        }

        fn prepare_migration_base(&self, root: &Path) {
            self.prepare(root);
            for member in load_manifest().unwrap().xberg.members {
                if super::optional_audio_path(&member.path) {
                    continue;
                }
                let path = root.join(&member.path);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                if !path.exists() {
                    fs::write(path, b"base").unwrap();
                }
            }
            for relative in [
                "models/snapshot-ocr/det.onnx", "models/snapshot-ocr/rec.onnx",
                "models/snapshot-ocr/dict/dict.txt",
            ] {
                let path = root.join(relative);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, b"snapshot").unwrap();
            }
        }
    }

    impl AssetDownloader for AudioFixture {
        fn download(
            &mut self, url: &str, destination: &Path, _: u64, _: &str,
            cancel: &AtomicBool, _: &mut dyn FnMut(String),
        ) -> Result<(), String> {
            self.requests.push(url.into());
            fs::write(destination, &self.archive).map_err(|error| format!("夹具写入失败：{error}"))?;
            if self.cancel_download {
                cancel.store(true, std::sync::atomic::Ordering::Release);
            }
            Ok(())
        }
    }

    #[test]
    fn audio_download_uses_only_receipt_pin_and_reuses_complete_install() {
        let guard = redirect_component_env();
        let root = guard.root.path().join("bundle");
        let mut fixture = AudioFixture::new();
        fixture.prepare(&root);
        jchtools_foundation::xberg_settings::save(&root).unwrap();
        let original = jchtools_foundation::xberg_settings::required().unwrap();
        assert_eq!(fixture.install().unwrap(), original);
        assert_eq!(fixture.requests, [fixture.receipt.audio_assets.url.clone()]);
        assert_eq!(fs::read(root.join("xberg.exe")).unwrap(), b"base engine");
        fixture.install().unwrap();
        assert_eq!(fixture.requests.len(), 1, "完整音频资产不得重复联网");
        assert_eq!(jchtools_foundation::xberg_settings::required().unwrap(), original);
        super::verify_component(&root, &fixture.receipt.audio_assets.component, &AtomicBool::new(false)).unwrap();
    }

    #[test]
    fn audio_integrity_cancel_and_hostile_archives_preserve_existing_files() {
        let guard = redirect_component_env();
        for failure in ["digest", "size", "cancel", "traversal", "base_overwrite", "extra", "member_digest", "missing"] {
            let root = guard.root.path().join(failure);
            let mut fixture = AudioFixture::new();
            fixture.prepare(&root);
            fs::create_dir_all(root.join("ffmpeg")).unwrap();
            fs::write(root.join("ffmpeg/LICENSE.txt"), b"audio license").unwrap();
            fs::write(root.join("ffmpeg/avcodec-63.dll"), b"old incomplete codec").unwrap();
            jchtools_foundation::xberg_settings::save(&root).unwrap();
            let original = jchtools_foundation::xberg_settings::required().unwrap();
            match failure {
                "digest" => fixture.archive[0] ^= 1,
                "size" => { fixture.archive.pop(); }
                "cancel" => fixture.cancel_download = true,
                "traversal" => fixture.files.push(("../escape".into(), b"bad".to_vec())),
                "base_overwrite" => fixture.files.push(("xberg.exe".into(), b"bad".to_vec())),
                "extra" => fixture.files.push(("ffmpeg/unlisted.dll".into(), b"bad".to_vec())),
                "member_digest" => fixture.files[0].1 = b"bad".to_vec(),
                "missing" => { fixture.files.pop(); }
                _ => unreachable!(),
            }
            if !matches!(failure, "digest" | "size" | "cancel") {
                fixture.rebuild_archive();
                fixture.write_receipt(&root);
            }
            assert!(fixture.install().is_err(), "{failure}");
            assert_eq!(fs::read(root.join("xberg.exe")).unwrap(), b"base engine", "{failure}");
            assert_eq!(fs::read(root.join("ffmpeg/LICENSE.txt")).unwrap(), b"audio license", "{failure}");
            assert_eq!(fs::read(root.join("ffmpeg/avcodec-63.dll")).unwrap(), b"old incomplete codec", "{failure}");
            assert_eq!(jchtools_foundation::xberg_settings::required().unwrap(), original);
        }
    }

    #[test]
    fn audio_receipt_rejects_unpinned_urls_and_base_member_aliases_before_network() {
        let guard = redirect_component_env();
        for path in ["../xberg.exe", "xberg.exe", "sherpa-onnx/onnxruntime.dll", "ffmpeg/../xberg.exe", "ffmpeg/CON.dll", "ffmpeg/file.dll.", "ffmpeg/file:ads"] {
            let root = guard.root.path().join(uuid::Uuid::new_v4().to_string());
            let mut fixture = AudioFixture::new();
            fixture.prepare(&root);
            fixture.receipt.audio_assets.component.members[0].path = path.into();
            fixture.write_receipt(&root);
            jchtools_foundation::xberg_settings::save(&root).unwrap();
            assert!(fixture.install().is_err(), "{path}");
            assert!(fixture.requests.is_empty());
            assert_eq!(fs::read(root.join("xberg.exe")).unwrap(), b"base engine");
        }
        let root = guard.root.path().join("wrong-release");
        let mut fixture = AudioFixture::new();
        fixture.prepare(&root);
        fixture.receipt.audio_assets.url = fixture.receipt.audio_assets.url.replace("v2099.1", "latest");
        fixture.write_receipt(&root);
        jchtools_foundation::xberg_settings::save(&root).unwrap();
        assert!(fixture.install().is_err());
        assert!(fixture.requests.is_empty());
    }

    #[test]
    fn audio_member_commit_failure_restores_invalid_and_keeps_valid_members() {
        let guard = redirect_component_env();
        let root = guard.root.path().join("target");
        let source = guard.root.path().join("source");
        let staging = guard.root.path().join("staging");
        let fixture = AudioFixture::new();
        fixture.prepare(&root);
        fixture.write_audio(&source);
        fs::create_dir(&staging).unwrap();
        fs::create_dir_all(root.join("ffmpeg")).unwrap();
        fs::write(root.join("ffmpeg/avcodec-63.dll"), b"old bytes").unwrap();
        fs::write(root.join("ffmpeg/LICENSE.txt"), b"audio license").unwrap();
        let error = super::install_audio_members(
            &root, &source, &fixture.receipt.audio_assets.component.members, &staging,
            &AtomicBool::new(false), || Err("配置提交失败".into()),
        ).unwrap_err();
        assert_eq!(error, "配置提交失败");
        assert_eq!(fs::read(root.join("ffmpeg/avcodec-63.dll")).unwrap(), b"old bytes");
        assert_eq!(fs::read(root.join("ffmpeg/LICENSE.txt")).unwrap(), b"audio license");
        assert!(!root.join("models/vad/silero_vad.onnx").exists());
        assert_eq!(fs::read(source.join("ffmpeg/avcodec-63.dll")).unwrap(), b"codec");
    }

    #[test]
    fn review_f34_unsaved_legacy_migration_keeps_old_source_and_reports_warning() {
        let guard = redirect_component_env();
        let fixture = AudioFixture::new();
        let old = guard.root.path().join("old");
        let bundle = guard.root.path().join("bundle");
        fixture.prepare_migration_base(&old);
        fixture.prepare_migration_base(&bundle);
        fs::write(guard.root.path().join("xberg-runtime-path.txt"), old.to_str().unwrap()).unwrap();
        std::env::set_var("JCHTOOLS_TEST_BUNDLED_XBERG_DIR", &bundle);
        let database = jchtools_foundation::xberg_settings::open(
            &jchtools_foundation::xberg_settings::state_dir().unwrap(),
        ).unwrap();
        database.execute_batch(
            "CREATE TRIGGER reject_legacy BEFORE INSERT ON app_settings
             WHEN NEW.key='xberg_directory'
             BEGIN SELECT RAISE(ABORT,'injected legacy migration write failure'); END;",
        ).unwrap();
        let mut progress = Vec::new();
        let root = super::migrate_bundled_runtime_with_activation(
            &AtomicBool::new(false), |message| progress.push(message),
            |_, _| panic!("旧文本迁入尚未保存，不能激活新来源"),
        ).unwrap();
        assert_eq!(root, old);
        assert!(progress.iter().any(|message| message.contains("未保存")));
        assert!(jchtools_foundation::xberg_settings::settings().unwrap().migration_warning.is_some());
        assert_eq!(database.query_row(
            "SELECT COUNT(*) FROM app_settings WHERE key IN ('xberg_directory','xberg_source')",
            [], |row| row.get::<_, i64>(0),
        ).unwrap(), 0);
        assert!(guard.root.path().join("xberg-runtime-path.txt").is_file());
    }

    #[test]
    fn bundled_migration_copies_only_compatible_members_and_preserves_source() {
        let guard = redirect_component_env();
        let fixture = AudioFixture::new();
        let old = guard.root.path().join("old");
        let bundle = guard.root.path().join("bundle");
        fixture.prepare_migration_base(&old);
        fixture.write_audio(&old);
        jchtools_xberg::xberg_runtime::validate_assets(&old, "media").unwrap();
        fixture.prepare_migration_base(&bundle);
        fs::write(old.join("unrelated-private.bin"), b"never copy").unwrap();
        jchtools_foundation::xberg_settings::save(&old).unwrap();
        std::env::set_var("JCHTOOLS_TEST_BUNDLED_XBERG_DIR", &bundle);
        let result = super::migrate_bundled_runtime_with_activation(
            &AtomicBool::new(false), |_| {}, |root, preserve_media| {
                assert!(preserve_media);
                jchtools_foundation::xberg_settings::save_bundled_directory(root)
            },
        ).unwrap();
        assert_eq!(result, fs::canonicalize(&bundle).unwrap());
        assert_eq!(jchtools_foundation::xberg_settings::required().unwrap(), result);
        super::verify_component(&bundle, &fixture.receipt.audio_assets.component, &AtomicBool::new(false)).unwrap();
        super::verify_component(&old, &fixture.receipt.audio_assets.component, &AtomicBool::new(false)).unwrap();
        assert!(!bundle.join("unrelated-private.bin").exists());
        assert_eq!(fs::read(bundle.join("xberg.exe")).unwrap(), b"base engine");
    }

    #[test]
    fn bundled_migration_commit_or_cancel_failure_keeps_old_configuration_and_audio() {
        let guard = redirect_component_env();
        let fixture = AudioFixture::new();
        let old = guard.root.path().join("old");
        let bundle = guard.root.path().join("bundle");
        fixture.prepare_migration_base(&old);
        fixture.write_audio(&old);
        jchtools_xberg::xberg_runtime::validate_assets(&old, "media").unwrap();
        fixture.prepare_migration_base(&bundle);
        jchtools_foundation::xberg_settings::save(&old).unwrap();
        let original = jchtools_foundation::xberg_settings::required().unwrap();
        std::env::set_var("JCHTOOLS_TEST_BUNDLED_XBERG_DIR", &bundle);
        let db = jchtools_foundation::xberg_settings::open(
            &jchtools_foundation::xberg_settings::state_dir().unwrap(),
        ).unwrap();
        db.execute_batch(
            "CREATE TRIGGER reject_bundle BEFORE INSERT ON app_settings
             WHEN NEW.key='xberg_source' AND NEW.value='bundled'
             BEGIN SELECT RAISE(ABORT,'injected failure'); END;",
        ).unwrap();
        let mut diagnostics = Vec::new();
        let result = super::migrate_bundled_runtime_with_activation(
            &AtomicBool::new(false), |message| diagnostics.push(message),
            |root, preserve_media| {
                assert!(preserve_media);
                jchtools_foundation::xberg_settings::save_bundled_directory(root)
            },
        ).unwrap();
        assert_eq!(result, original);
        assert_eq!(jchtools_foundation::xberg_settings::required().unwrap(), original);
        assert!(diagnostics.iter().any(|message| message.contains("继续使用原运行目录")));
        assert!(!bundle.join("ffmpeg/avcodec-63.dll").exists());
        super::verify_component(&old, &fixture.receipt.audio_assets.component, &AtomicBool::new(false)).unwrap();
        let result = super::migrate_bundled_runtime_with_activation(
            &AtomicBool::new(true), |_| {}, |_, _| panic!("取消不能切源"),
        ).unwrap();
        assert_eq!(result, original);
    }

    #[test]
    fn bundled_migration_retains_ready_old_media_until_target_is_compatible() {
        let guard = redirect_component_env();
        let fixture = AudioFixture::new();
        let old = guard.root.path().join("old");
        let bundle = guard.root.path().join("bundle");
        fixture.prepare_migration_base(&old);
        fixture.write_audio(&old);
        fixture.prepare_migration_base(&bundle);
        fs::write(old.join("ffmpeg/avcodec-63.dll"), b"incompatible but present").unwrap();
        jchtools_xberg::xberg_runtime::validate_assets(&old, "media").unwrap();
        jchtools_foundation::xberg_settings::save(&old).unwrap();
        let original = jchtools_foundation::xberg_settings::required().unwrap();
        std::env::set_var("JCHTOOLS_TEST_BUNDLED_XBERG_DIR", &bundle);
        let mut called = false;
        let result = super::migrate_bundled_runtime_with_activation(
            &AtomicBool::new(false), |_| {}, |_, _| { called = true; Ok(()) },
        ).unwrap();
        assert!(!called, "已就绪旧媒体不兼容时不能切源");
        assert_eq!(result, original);
        assert_eq!(jchtools_foundation::xberg_settings::required().unwrap(), original);
        assert!(!bundle.join("ffmpeg").exists());
        assert_eq!(fs::read(old.join("ffmpeg/avcodec-63.dll")).unwrap(), b"incompatible but present");
        fixture.write_audio(&bundle);
        let result = super::migrate_bundled_runtime_with_activation(
            &AtomicBool::new(false), |_| {}, |root, preserve_media| {
                assert!(preserve_media);
                jchtools_foundation::xberg_settings::save_bundled_directory(root)
            },
        ).unwrap();
        assert_eq!(result, fs::canonicalize(&bundle).unwrap());
        jchtools_xberg::xberg_runtime::validate_assets(&result, "media").unwrap();
        assert_eq!(fs::read(old.join("ffmpeg/avcodec-63.dll")).unwrap(), b"incompatible but present");
    }

    #[test]
    fn bundled_default_closes_notice_readiness_without_manual_initialization() {
        let guard = redirect_component_env();
        let fixture = AudioFixture::new();
        let bundle = guard.root.path().join("bundle");
        fixture.prepare_migration_base(&bundle);
        std::env::set_var("JCHTOOLS_TEST_BUNDLED_XBERG_DIR", &bundle);
        assert!(!guard.root.path().join("licenses/THIRD_PARTY_NOTICES.md").exists());
        let root = super::migrate_bundled_runtime_with_activation(
            &AtomicBool::new(false), |_| {}, |root, preserve_media| {
                assert!(!preserve_media);
                jchtools_foundation::xberg_settings::save_bundled_directory(root)
            },
        ).unwrap();
        assert_eq!(root, fs::canonicalize(&bundle).unwrap());
        assert_eq!(jchtools_foundation::xberg_settings::settings().unwrap().source,
            jchtools_foundation::xberg_settings::Source::Bundled);
        super::readiness().unwrap();
        assert!(guard.root.path().join("licenses/THIRD_PARTY_NOTICES.md").is_file());
        assert!(super::validate_media().unwrap_err().contains("可选音频资产"));
        if crate::markdown::platform_preflight().is_ok() {
            crate::markdown::readiness_for_groups(&[
                crate::markdown::FormatGroup::Pdf, crate::markdown::FormatGroup::Images,
                crate::markdown::FormatGroup::Media,
            ]).unwrap();
        }
    }
