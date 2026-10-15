use rspolib::{mofile, pofile, AsBytes, FileOptions};

fn message_bytes(msgid: &[u8], msgstr: &[u8], big_endian: bool) -> Vec<u8> {
    let mut bytes = Vec::new();
    for word in [
        rspolib::MAGIC,
        0,
        1,
        28,
        36,
        0,
        0,
        msgid.len() as u32,
        44,
        msgstr.len() as u32,
        45 + msgid.len() as u32,
    ] {
        bytes.extend_from_slice(&if big_endian {
            word.to_be_bytes()
        } else {
            word.to_le_bytes()
        });
    }
    bytes.extend_from_slice(msgid);
    bytes.push(0);
    bytes.extend_from_slice(msgstr);
    bytes.push(0);
    bytes
}

#[test]
fn mo_context_preserves_message_identity_and_roundtrip() {
    for big_endian in [false, true] {
        for (msgid, msgstr, plural) in [
            (b"button\x04Open".as_slice(), b"Ouvrir".as_slice(), false),
            (
                b"button\x04file\0files".as_slice(),
                b"fichier\0fichiers".as_slice(),
                true,
            ),
        ] {
            let file = mofile(message_bytes(msgid, msgstr, big_endian)).unwrap();
            let entry = &file.entries[0];
            assert_eq!(entry.msgctxt.as_deref(), Some("button"));
            assert_eq!(entry.msgid, if plural { "file" } else { "Open" });
            assert_eq!(entry.msgid_plural.as_deref(), plural.then_some("files"));
            assert!(file.find_by_msgid_msgctxt(&entry.msgid, "button").is_some());
            if plural {
                assert_eq!(entry.msgstr_plural, ["fichier", "fichiers"]);
            } else {
                assert_eq!(entry.msgstr.as_deref(), Some("Ouvrir"));
            }
            let reparsed = mofile(file.as_bytes().into_owned()).unwrap();
            assert_eq!(reparsed.entries, file.entries);
        }
    }
}

#[test]
fn borrowed_file_options_preserve_binary_content() {
    let options = FileOptions::from(message_bytes(b"a", b"b", false));
    let borrowed = mofile(&options).unwrap();
    let owned = mofile(options.clone()).unwrap();
    assert_eq!(borrowed.entries, owned.entries);
    assert!(options.byte_content.is_some());
}

#[test]
fn mo_invalid_path_returns_error() {
    assert!(mofile("\0").is_err());
    assert!(mofile(".").is_err());
}

#[test]
fn po_malformed_quoted_fields_return_error() {
    let mut failures = Vec::new();
    for content in [
        "msgid \"",
        "msgid 中\"",
        "msgid bad\"",
        "msgid \"a\"\nmsgstr \"",
        "msgid \"a\"\nmsgstr \"b\"\n\"",
        "#| msgid \"\nmsgid \"a\"\nmsgstr \"b\"",
        "#| msgid 中\"\nmsgid \"a\"\nmsgstr \"b\"",
        "msgid \"a\"\nmsgid_plural \"as\"\nmsgstr[0] missing",
        "msgid \"a\"\nmsgid_plural \"as\"\nmsgstr[0] \"",
        "msgid \"a\"\nmsgid_plural \"as\"\nmsgstr[0] 中\"",
        "msgstr \"orphan\"",
    ] {
        if !matches!(std::panic::catch_unwind(|| pofile(content)), Ok(Err(_))) {
            failures.push(content);
        }
    }
    assert!(
        failures.is_empty(),
        "错误输入应返回 SyntaxError：{failures:?}"
    );
    let valid = pofile("#| msgid \"旧值\"\nmsgid \"新值\"\nmsgstr \"译文\"").unwrap();
    assert_eq!(valid.entries[0].msgid, "新值");
    assert_eq!(valid.entries[0].previous_msgid.as_deref(), Some("旧值"));
    assert_eq!(valid.entries[0].msgstr.as_deref(), Some("译文"));
}
