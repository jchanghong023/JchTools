use super::dedup::{count_logical_bytes, try_detect_extension};
use jchtools_file_core::{control::Control, model::Snapshot};
use std::collections::HashSet;

#[test]
    #[rustfmt::skip]
    fn signature_detection_cancellation_is_not_swallowed() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("valid.txt");
        std::fs::write(
            &source,
            include_bytes!("../../../../tests/markdown_fixtures/test_hello_world.png"),
        )
        .unwrap();
        let control = Control::default();
        control.cancel();
        assert!(
            try_detect_extension(&source, &control).is_err(),
            "真实 detector 返回的用户取消必须传播，不能按未知类型继续并标记 ready"
        );
    }

#[test]
    #[rustfmt::skip]
    fn signature_detection_unreliable_content_keeps_existing_extension() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("unreliable.txt");
        let bytes = b"\x89PNG\r\n\x1a\n\0\0\0\0";
        std::fs::write(&source, bytes).unwrap();
        let control = Control::default();
        assert_eq!(try_detect_extension(&source, &control).unwrap(), None);
        assert_eq!(std::fs::read(&source).unwrap(), bytes);
        assert_eq!(source.extension().unwrap(), "txt");
        assert!(!control.is_cancelled());
    }

#[test]
    #[rustfmt::skip]
    fn review_f19_logical_size_uses_only_reliable_identity() {
        let mut identities = HashSet::new();
        let mut snapshot = Snapshot {
            size: 19,
            modified_ns: 0,
            created_ns: None,
            identity: if cfg!(windows) { "1:0:42" } else { "1:42" }.into(),
            links: 2,
        };
        assert_eq!(count_logical_bytes(Some(&snapshot), &mut identities), 19);
        assert_eq!(count_logical_bytes(Some(&snapshot), &mut identities), 0);
        for identity in ["", "unknown", "0:0:0", "invalid:1:2", "0:0"] {
            snapshot.identity = identity.into();
            assert_eq!(count_logical_bytes(Some(&snapshot), &mut identities), 19, "{identity}");
            assert_eq!(count_logical_bytes(Some(&snapshot), &mut identities), 19, "{identity}");
        }
        assert_eq!(count_logical_bytes(None, &mut identities), 0);
    }
