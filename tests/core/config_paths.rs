use super::*;

// 覆盖 S-02
#[test]
fn defaults_valid_and_roundtrip() {
    let cfg = Config::default();
    cfg.validate_organizer().unwrap();
    cfg.validate_extract().unwrap();
    let text = serde_json::to_string(&cfg).unwrap();
    let back = Config::from_json_text(&text).unwrap();
    assert_eq!(
        serde_json::to_value(&cfg).unwrap(),
        serde_json::to_value(&back).unwrap()
    );
}
#[test]
fn reject_unknown_configuration() {
    assert!(Config::from_json_text(r#"{"delete_everything":true}"#).is_err());
}
// 覆盖 S-03, C-02（verify_bytes 已随合同第三批整体移除、旧值被剥除；同名版本取舍开关已按合同移除）
#[test]
fn legacy_config_with_removed_fields_still_loads() {
    let mut value = serde_json::to_value(Config::default()).unwrap();
    for (key, v) in [
        ("hash_algorithm", serde_json::json!("md5")),
        ("verify_bytes", serde_json::json!(false)),
        ("same_name_same_size", serde_json::json!(true)),
        ("same_size_keep", serde_json::json!("oldest")),
        ("same_name_different_size", serde_json::json!(true)),
        ("different_size_keep", serde_json::json!("oldest")),
        ("conflict_scope_directory", serde_json::json!(false)),
    ] {
        value.as_object_mut().unwrap().insert(key.into(), v);
    }
    Config::from_json_text(&value.to_string()).unwrap();
}
// 覆盖 R-01
#[test]
fn schema_matches_every_configuration_field() {
    let cfg = serde_json::to_value(Config::default()).unwrap();
    let schema: serde_json::Value =
        serde_json::from_str(include_str!("../../resources/rules.json")).unwrap();
    let keys = schema.as_array().unwrap();
    let fields = cfg.as_object().unwrap();
    // 有意不进规则表的字段：theme 在「关于」页；detect_type 是合并行的影子键
    // （「修正扩展名」一行同时驱动 detect_type 与 fix_extension，引擎/旧任务库仍读原值）。
    let hidden = ["theme", "detect_type"];
    assert_eq!(fields.len(), keys.len() + hidden.len());
    for row in keys {
        assert!(fields.get(row["key"].as_str().unwrap()).is_some());
    }
    for key in hidden {
        assert!(fields.contains_key(key), "{key} 应作为配置字段保留");
    }
}
// 覆盖 R-03, X-08, C-08
#[test]
fn coupled_validation_and_bounds() {
    let mut c = base();
    // R-03：detect_type 是 fix_extension 的影子键，由 from_json_text / set_json 强制同值，
    // 校验不再把「只开 fix_extension」判为非法（直接改字段也无法构造持久化的不同步状态）。
    c.fix_extension = true;
    c.detect_type = true;
    assert!(c.validate_organizer().is_ok());
    assert!(c.validate_extract().is_ok());
    let synced = Config::from_json_text(&serde_json::to_string(&c).unwrap()).unwrap();
    assert_eq!(
        synced.detect_type, synced.fix_extension,
        "影子键必须同值回读"
    );
    c.hash_workers = 0;
    assert!(c.validate_organizer().is_err());
    assert!(c.validate_extract().is_ok(), "整理线程数不属于解压配置");
    c.hash_workers = 2;
    c.reserve_bytes = u64::MAX;
    assert!(c.validate_extract().is_err());
    assert!(c.validate_organizer().is_ok(), "磁盘预留不属于整理配置");
}
// 覆盖 S-05
#[test]
fn unsafe_paths_rejected() {
    for value in [
        "../x",
        "x/../../outside",
        "C:/x",
        "/etc/passwd",
        r"\\server\share\x",
        "file:stream",
        "CON.txt",
        "a/NUL",
        "x. ",
        "a\n.txt",
        "",
    ] {
        assert!(fsutil::safe_relative(value).is_err(), "{value:?}");
    }
}
// 覆盖 S-05
#[test]
fn relative_tar_and_unicode_paths_accepted() {
    assert_eq!(
        fsutil::safe_relative("./报告/a.pdf").unwrap(),
        PathBuf::from("报告/a.pdf")
    );
    assert_eq!(
        fsutil::safe_relative(r"资料\图片.png").unwrap(),
        PathBuf::from("资料/图片.png")
    );
}
// 覆盖 S-05
#[test]
fn windows_reserved_names_rejected() {
    for s in ["con", "COM1", "LPT9.txt", "nul", "AUX.jpg", "COM¹.txt"] {
        assert!(fsutil::validate_component(s).is_err());
    }
    assert!(fsutil::validate_component("COM10.txt").is_ok());
    assert!(fsutil::validate_component("COM0.txt").is_ok());
}
// 覆盖 S-01 / 附录 B：合法 Unicode 空白不等于 Windows 禁用的尾随 ASCII 空格。
#[test]
fn component_validation_preserves_legal_unicode_whitespace() {
    for name in ["a\u{3000}", "a\u{a0}", "a b"] {
        assert!(fsutil::validate_component(name).is_ok(), "{name:?}");
        let relative = format!("parent/{name}");
        assert_eq!(
            fsutil::safe_relative(&relative).unwrap(),
            PathBuf::from(relative),
            "路径校验不得暗中规范化合法原名"
        );
    }
    for name in ["a.", "a ", "a\t"] {
        assert!(fsutil::validate_component(name).is_err(), "{name:?}");
    }
}
// 覆盖 S-01
#[test]
fn rename_never_overwrites() {
    let f = Fixture::new();
    let a = f.write("a", b"A", 1);
    let b = f.write("b", b"B", 2);
    assert!(fsutil::rename_noreplace(&a, &b).is_err());
    assert_eq!(fs::read(a).unwrap(), b"A");
    assert_eq!(fs::read(b).unwrap(), b"B");
}
// 覆盖 C-05, S-01
#[test]
fn unique_name_preserves_extension() {
    let f = Fixture::new();
    let p = f.write("report.pdf", b"a", 1);
    let q = fsutil::unique_target(&f.root, &p).unwrap();
    assert_eq!(q.file_name().unwrap(), "report (1).pdf");
}
// 覆盖 C-05, H-07
#[test]
fn unique_name_truncates_long_stem_to_component_limit() {
    // 回归：基础名接近 255 个 UTF-16 单元且目标被占用时，“名 (N).扩展”候选名会超限，
    // validate_component 令 unique_target 整体失败——归类冲突回退因此把整次任务搞失败。
    // 应截断 stem 生成合法候选名（扩展名保持不变，H-07 只允许改文件名主体）。
    let f = Fixture::new();
    // 基础名 251+4=255 恰好合法；加「 (1)」后 260 超限，截断只能落在 stem 上。
    let stem = "a".repeat(251);
    let p = f.write(&format!("{stem}.txt"), b"a", 1);
    let q = fsutil::unique_target(&f.root, &p).unwrap();
    let name = q.file_name().unwrap().to_str().unwrap().to_string();
    assert!(
        name.encode_utf16().count() <= 255,
        "分配名不得超 255 个 UTF-16 单元：{name}"
    );
    assert!(name.ends_with(" (1).txt"), "保持扩展名与序号后缀：{name}");
    assert_ne!(q, p);
    // 截断只发生在超限时：常规名不受影响。
    let r = f.write("b.pdf", b"b", 1);
    assert_eq!(
        fsutil::unique_target(&f.root, &r)
            .unwrap()
            .file_name()
            .unwrap(),
        "b (1).pdf"
    );
    // 复合扩展名与编号分卷整体保留：序号插在扩展名之前（H-07 例：资料.tar.gz → 资料 (1).tar.gz）。
    let gz = f.write("资料.tar.gz", b"g", 1);
    assert_eq!(
        fsutil::unique_target(&f.root, &gz)
            .unwrap()
            .file_name()
            .unwrap(),
        "资料 (1).tar.gz"
    );
    let vol = f.write("包.7z.001", b"v", 1);
    assert_eq!(
        fsutil::unique_target(&f.root, &vol)
            .unwrap()
            .file_name()
            .unwrap(),
        "包 (1).7z.001"
    );
}
// 覆盖 H-07（不得通过更换或追加扩展名腾名：无法保留扩展名时显式报错且不动原文件）
#[test]
fn unique_target_errors_when_extension_cannot_take_suffix() {
    let f = Fixture::new();
    // 扩展名 251 个单元：追加「 (1)」必然超 255 单元上限；截断扩展名等于改扩展名，禁止。
    let long_ext = "x".repeat(251);
    let p = f.write(&format!("a.{long_ext}"), b"a", 1);
    assert!(
        fsutil::unique_target(&f.root, &p).is_err(),
        "扩展名放不下序号时必须显式报错，不得截断/更换扩展名"
    );
    assert!(p.exists(), "分配失败不得改动原文件");
    // 常规扩展名照常分配：报错只针对放不下序号的超长扩展名。
    let ok = f.write("a.txt", b"a", 1);
    assert!(fsutil::unique_target(&f.root, &ok).is_ok());
}
// 覆盖 S-03
#[test]
fn hashes_known_vectors() {
    let f = Fixture::new();
    let p = f.write("a", b"abc", 1);
    let ctl = Control::default();
    assert_eq!(
        hashing::full_hash(&p, &ctl).unwrap(),
        "blake3:6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
    );
}
// 覆盖 C-10（取消后不再继续读取）
#[test]
fn cancelled_hash_does_not_read() {
    let f = Fixture::new();
    let p = f.write("a", b"abc", 1);
    let ctl = Control::default();
    ctl.cancel();
    assert!(hashing::full_hash(&p, &ctl).is_err());
    assert_eq!(ctl.read_bytes.load(Ordering::Relaxed), 0);
}
// 覆盖 C-08（副本后缀清理的识别口径与输出名转换：`(N)` 转 `_N`，其余标记移除）
#[test]
fn copy_suffixes_and_nonempty_name() {
    for s in ["报告 (1).pdf", "报告 - Copy.pdf", "报告 副本.pdf"] {
        assert_eq!(rules::copy_key(s), "报告.pdf", "{s}");
    }
    // 输出转换：`(N)` 转为 `_N`（去前导零、全零保留 0）；`- Copy` / `副本` 移除。
    //（连续多标记的转换由 rules::copy_marker_conversion_matches_appendix_b 锚定。）
    assert_eq!(rules::clean_copy_output("报告 (1).pdf"), "报告_1.pdf");
    assert_eq!(rules::clean_copy_output("报告 (01).pdf"), "报告_1.pdf");
    assert_eq!(rules::clean_copy_output("报告 (0).pdf"), "报告_0.pdf");
    // 全角括号不是副本标记（附录 B：ASCII 圆括号才算），名称保持原样。
    assert_eq!(rules::clean_copy_output("报告（2）.pdf"), "报告（2）.pdf");
    // 基础主体为空时用 `_`（附录 B）。
    assert_eq!(rules::clean_copy_output("(1).pdf"), "__1.pdf");
}
// 覆盖 X-01（支持格式与分卷首卷识别）
#[test]
fn archive_first_volume_detection() {
    assert!(rules::archive_name("A.part01.rar"));
    assert!(!rules::archive_name("A.part02.rar"));
    assert!(rules::archive_name("A.7z.001"));
    assert!(!rules::archive_name("A.7z.002"));
    assert!(rules::archive_name("A.tar.gz"));
}
// 覆盖 S-02（保留方式不删除任何东西）
#[test]
fn keep_never_deletes() {
    let f = Fixture::new();
    let p = f.write("a", b"a", 1);
    assert_eq!(
        platform::remove(&p, DeleteMode::Keep, &Control::default()).unwrap(),
        DeleteResult::Kept
    );
    assert!(p.exists());
}
// 覆盖 C-07（MUST NOT 递归删除有内容的目录）
#[test]
fn nonempty_directory_cannot_be_removed() {
    let f = Fixture::new();
    f.write("sub/a", b"a", 1);
    assert!(platform::remove(
        &f.root.join("sub"),
        DeleteMode::Permanent,
        &Control::default()
    )
    .is_err());
    assert!(f.root.join("sub/a").exists());
}
