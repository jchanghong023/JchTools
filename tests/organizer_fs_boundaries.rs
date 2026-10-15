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

//! 文件系统边界回归（S-01 / S-04）：
//! - F01：所选根或其祖先含 junction / 其他 reparse point 时两工具拒绝开始（S-04 主句，
//!   修复前 normalize_root 先 canonicalize，reparse 身份丢失、链接目标树被处理）。
//! - F02：`.jchtools-link-*` 名称和旧修改时间不证明工具所有权；既有用户硬链接
//!   在范围外保持原位，在范围内按普通文件归类并保留，不作为独立副本删除。
//!
//! 全部数据在 tempfile 内生成；junction 用 `cmd /C mklink /J`（无需管理员），
//! 非链接 reparse 根用文件系统控制码设置在本次自有空目录上，不要求符号链接特权。
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::many_single_char_names
)]
use jchtools_file_core::config::Config;
use jchtools_file_core::control::Context;
use jchtools_file_core::control::Control;
use jchtools_directory_organizer::engine;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};
use tempfile::TempDir;

/// 用 `cmd /C mklink /J` 创建 junction（无需管理员权限）。
fn make_junction(link: &Path, target: &Path) -> std::io::Result<()> {
    let out = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "mklink /J 失败：{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )))
    }
}

/// 设置真实的非 Microsoft GUID reparse point；不安装处理驱动，不解析任何目标。
/// 此夹具覆盖 is_symlink 之外的 FILE_ATTRIBUTE_REPARSE_POINT 分支，
/// 而不是依赖 mklink /D 的主机符号链接特权，也不以构造失败跳过断言。
fn make_other_reparse_root(path: &Path) -> std::io::Result<()> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::{
        Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT},
        System::IO::DeviceIoControl,
    };

    fs::create_dir(path)?;
    let directory = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    // REPARSE_GUID_DATA_BUFFER：tag + 零长度/保留字段 + 此测试固定 GUID。
    // 非 Microsoft、非 name-surrogate tag，只有零长度数据；不冒充 symlink 或 junction。
    let mut data = [0u8; 24];
    data[..4].copy_from_slice(&0x0000_0042u32.to_le_bytes());
    data[8..].copy_from_slice(&[
        0x18, 0x3e, 0xca, 0x84, 0x9b, 0xe8, 0x46, 0x42, 0x90, 0x34, 0xf2, 0x17, 0x27, 0x26,
        0x68, 0x1c,
    ]);
    let mut returned = 0;
    // SAFETY: directory 在同步调用期间拥有有效目录句柄；data 是 24 字节的完整
    // GUID reparse 头；returned 可写，输出/overlapped 为 NULL，不保留任何缓冲区指针。
    let success = unsafe {
        DeviceIoControl(
            directory.as_raw_handle(),
            0x0009_00a4, // FSCTL_SET_REPARSE_POINT
            data.as_ptr().cast(),
            24,
            std::ptr::null_mut(),
            0,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    };
    if success == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// 状态目录下已创建的任务库数量（0 = 未创建任何计划）。
fn count_task_dirs(state: &Path) -> usize {
    let entries = match fs::read_dir(state.join("tasks")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(error) => panic!("读取任务目录失败，不能据此认定未创建计划：{error}"),
    };
    entries
        .map(|entry| entry.expect("读取任务目录项失败"))
        .count()
}

/// 创建带旧内部前缀、修改时间为 25 小时前的既有用户硬链接（链接数 ≥ 2）。
/// 夹具没有注册工具残留或记录本次所有权，名称和时龄都不授予清理权限。
fn make_stale_link_temp(dir: &Path, keeper: &Path, tag: &str) -> PathBuf {
    let path = dir.join(format!(".jchtools-link-{tag}"));
    fs::hard_link(keeper, &path).unwrap();
    let old = filetime::FileTime::from_system_time(
        std::time::SystemTime::now() - Duration::from_hours(25),
    );
    filetime::set_file_mtime(&path, old).unwrap();
    path
}

/// 为文件设置隐藏属性（Windows attrib；测试内联目录数据时使用）。
fn set_hidden(path: &Path) {
    let out = Command::new("attrib").arg("+h").arg(path).output().unwrap();
    assert!(
        out.status.success(),
        "测试前置：设置隐藏属性失败：{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

struct Fixture {
    temp: TempDir,
    root: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let state = temp.path().join("state");
        fs::create_dir(&root).unwrap();
        Self { temp, root, state }
    }
}

// ---------------------------------------------------------------------------
// F01 · S-04：所选根或其访问路径经过链接边界时拒绝开始
// ---------------------------------------------------------------------------

// 覆盖 S-04（回归：所选根本身是 junction 时目录整理必须拒绝开始；修复前
// canonicalize 解析掉 reparse 身份，任务在链接目标树里正常建库扫描）
#[test]
fn f01_prepare_rejects_root_junction() {
    let f = Fixture::new();
    let target = f.temp.path().join("real");
    let payload = target.join("资料.txt");
    fs::create_dir_all(&target).unwrap();
    fs::write(&payload, "内容").unwrap();
    let link = f.temp.path().join("入口");
    make_junction(&link, &target).unwrap();

    let error = engine::prepare_at(&link, Config::default(), Context::default(), &f.state)
        .err()
        .map(|e| format!("{e:#}"));
    let error = error.expect("junction 根必须被拒绝开始（S-04）");
    assert!(
        error.contains("S-04") && error.contains("符号链接"),
        "拒绝文案必须明示 S-04 语义：{error}"
    );
    assert_eq!(
        fs::read_to_string(&payload).unwrap(),
        "内容",
        "拒绝时链接目标文件不得删除或改写（S-04）"
    );
    assert_eq!(count_task_dirs(&f.state), 0, "拒绝时不创建计划、不扫描");
}

// 覆盖 S-04（回归：祖先组件是 junction 时同样拒绝；访问路径经过链接边界）
#[test]
fn f01_prepare_rejects_ancestor_junction() {
    let f = Fixture::new();
    let target = f.temp.path().join("real");
    let payload = target.join("sub").join("文件.txt");
    fs::create_dir_all(payload.parent().unwrap()).unwrap();
    fs::write(&payload, "内容").unwrap();
    let link = f.temp.path().join("链路");
    make_junction(&link, &target).unwrap();
    let selected = link.join("sub");

    let error = engine::prepare_at(&selected, Config::default(), Context::default(), &f.state)
        .err()
        .map(|e| format!("{e:#}"));
    let error = error.expect("祖先为 junction 的根必须被拒绝开始（S-04）");
    assert!(
        error.contains("S-04"),
        "拒绝文案必须明示 S-04 语义：{error}"
    );
    assert_eq!(
        fs::read_to_string(&payload).unwrap(),
        "内容",
        "拒绝时链接目标文件不得删除或改写（S-04）"
    );
    assert_eq!(count_task_dirs(&f.state), 0, "拒绝时不创建计划、不扫描");
}

// 覆盖 S-04（对照组：普通目录不受链接边界检查影响，正常分析）
#[test]
fn f01_prepare_accepts_plain_directory() {
    let f = Fixture::new();
    fs::write(f.root.join("a.txt"), b"a").unwrap();
    let task = engine::prepare_at(&f.root, Config::default(), Context::default(), &f.state)
        .expect("普通目录必须正常分析");
    assert_eq!(count_task_dirs(&f.state), 1);
    assert_eq!(task.summary.scanned, 1);
}

// 覆盖 S-04（回归：确认框清点入口同样拒绝 junction 根，清点不进入链接目标树）
#[test]
fn f01_count_archives_rejects_root_junction() {
    let f = Fixture::new();
    let target = f.temp.path().join("real");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("pack.zip"), b"fake").unwrap();
    let link = f.temp.path().join("入口");
    make_junction(&link, &target).unwrap();

    let error = jchtools_recursive_extract::engine::count_archives(&link, &Config::default())
        .err()
        .map(|e| format!("{e:#}"));
    let error = error.expect("junction 根的清点必须被拒绝（S-04）");
    assert!(
        error.contains("S-04"),
        "拒绝文案必须明示 S-04 语义：{error}"
    );
    assert_eq!(
        fs::read(target.join("pack.zip")).unwrap(),
        b"fake",
        "拒绝时链接目标文件不得删除或改写（S-04）"
    );
}

// 覆盖 S-04（回归：递归解压入口拒绝 junction 根，不创建任务库）
#[test]
fn f01_extract_run_rejects_root_junction() {
    let f = Fixture::new();
    let target = f.temp.path().join("real");
    fs::create_dir_all(&target).unwrap();
    let pack = target.join("pack.zip");
    fs::write(&pack, b"fake zip bytes").unwrap();
    let link = f.temp.path().join("入口");
    make_junction(&link, &target).unwrap();

    let error =
        jchtools_recursive_extract::engine::extract_run_at(&link, Config::default(), Context::default(), &f.state, None)
            .err()
            .map(|e| format!("{e:#}"));
    let error = error.expect("junction 根的解压必须被拒绝开始（S-04）");
    assert!(
        error.contains("S-04"),
        "拒绝必须发生在引擎阶段之前：{error}"
    );
    assert_eq!(count_task_dirs(&f.state), 0, "拒绝时不创建任务库");
    assert_eq!(
        fs::read(&pack).unwrap(),
        b"fake zip bytes",
        "拒绝时链接目标文件不得删除或改写（S-04）"
    );
}

// 覆盖 S-04（非 symlink / junction 的实际 reparse 根）：不能只检查 FileType::is_symlink。
// 夹具与链接边界断言均为必执行；不设置开发者模式、不提权、不依赖环境既有链接。
#[test]
fn f01_prepare_rejects_other_reparse_root() {
    use std::os::windows::fs::MetadataExt;

    let f = Fixture::new();
    let payload = f.root.join("资料.txt");
    fs::write(&payload, "内容").unwrap();
    let selected = f.temp.path().join("reparse入口");
    make_other_reparse_root(&selected)
        .expect("测试前置：必须能在自有空目录上设置非符号链接 reparse point");
    let before = fs::symlink_metadata(&selected).unwrap();
    assert_ne!(
        before.file_attributes() & 0x400,
        0,
        "夹具必须是真实 reparse point"
    );
    assert!(!before.file_type().is_symlink(), "覆盖非符号链接属性分支");

    let errors = [
        engine::prepare_at(&selected, Config::default(), Context::default(), &f.state)
            .err()
            .expect("其他 reparse 根必须在创建任务库前拒绝（S-04）"),
        jchtools_recursive_extract::engine::count_archives(&selected, &Config::default())
            .err()
            .expect("其他 reparse 根的清点必须拒绝（S-04）"),
        jchtools_recursive_extract::engine::extract_run_at(&selected, Config::default(), Context::default(), &f.state, None)
            .err()
            .expect("其他 reparse 根的解压必须在创建任务库前拒绝（S-04）"),
    ];
    for error in errors {
        assert!(
            format!("{error:#}").contains("S-04"),
            "拒绝必须来自 S-04 边界检查，而不是不能解析未知 reparse tag：{error:#}"
        );
    }
    assert_eq!(
        fs::read_to_string(&payload).unwrap(),
        "内容",
        "拒绝时普通资料不得删除或改写"
    );
    assert_eq!(count_task_dirs(&f.state), 0, "拒绝时不创建计划、不扫描");
    let after = fs::symlink_metadata(&selected).unwrap();
    assert_eq!(
        before.file_attributes(),
        after.file_attributes(),
        "不搬移或删除 reparse 根"
    );
    fs::remove_dir(&selected).unwrap();
}

// ---------------------------------------------------------------------------
// F02 · S-01 / C-04：既有用户硬链接的范围保护与正常归类
// ---------------------------------------------------------------------------

// 覆盖 S-01（回归：glob 排除子树内的 .jchtools-link-* 硬链接不得在执行前被清理；
// 修复前 apply 的独立全盘 WalkDir 不应用本次范围设置，静默删除）
#[test]
fn f02_keeps_link_temps_inside_glob_excluded_subtree() {
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("excl")).unwrap();
    let keeper = f.root.join("keeper.txt");
    fs::write(&keeper, "唯一内容").unwrap();
    let residue = make_stale_link_temp(&f.root.join("excl"), &keeper, "glob");

    let cfg = Config {
        exclusions: "excl".into(),
        ..Config::default()
    };
    let task = engine::prepare_at(&f.root, cfg, Context::default(), &f.state).unwrap();
    engine::apply(&task.directory, Context::default()).unwrap();

    assert!(
        residue.exists(),
        "排除子树内的 .jchtools-link-* 不在本次范围内，不得删除（S-01）"
    );
    assert_eq!(fs::read_to_string(&residue).unwrap(), "唯一内容");
}

// 覆盖 S-01（回归：关闭递归后深层目录不在扫描范围内，其中的残留不得清理）
#[test]
fn f02_keeps_link_temps_below_nonrecursive_scope() {
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("sub")).unwrap();
    let keeper = f.root.join("keeper.txt");
    fs::write(&keeper, "唯一内容").unwrap();
    let residue = make_stale_link_temp(&f.root.join("sub"), &keeper, "deep");

    let cfg = Config {
        recursive: false,
        ..Config::default()
    };
    let task = engine::prepare_at(&f.root, cfg, Context::default(), &f.state).unwrap();
    engine::apply(&task.directory, Context::default()).unwrap();

    assert!(residue.exists(), "非递归范围之外的深层残留不得删除（S-01）");
}

// 覆盖 S-01（回归：隐藏属性范围外的残留（未勾选包含隐藏）不得清理）
#[test]
fn f02_keeps_hidden_link_temps_when_hidden_excluded() {
    let f = Fixture::new();
    let keeper = f.root.join("keeper.txt");
    fs::write(&keeper, "唯一内容").unwrap();
    let residue = make_stale_link_temp(&f.root, &keeper, "hidden");
    set_hidden(&residue);

    let cfg = Config {
        include_hidden: false,
        ..Config::default()
    };
    let task = engine::prepare_at(&f.root, cfg, Context::default(), &f.state).unwrap();
    engine::apply(&task.directory, Context::default()).unwrap();

    assert!(
        residue.exists(),
        "隐藏范围外的 .jchtools-link-* 不得删除（S-01）"
    );
}

// 覆盖 S-01（回归：用户已请求停止时不清理残留——本次执行的删除授权尚未生效）
#[test]
fn f02_keeps_link_temps_when_stop_requested_before_apply() {
    let f = Fixture::new();
    let keeper = f.root.join("keeper.txt");
    fs::write(&keeper, "唯一内容").unwrap();
    let residue = make_stale_link_temp(&f.root, &keeper, "cancel");

    let task =
        engine::prepare_at(&f.root, Config::default(), Context::default(), &f.state).unwrap();
    let control = Control::default();
    control.cancel();
    let cancelled = Context {
        control: Arc::new(control),
        events: None,
    };
    let result = engine::apply(&task.directory, cancelled);
    assert!(result.is_err(), "已请求停止的任务必须失败收尾，不得报成功");
    assert!(
        residue.exists(),
        "已请求停止时不得清理残留（用户尚未授权本次执行的任何删除）"
    );
}

// 覆盖 C-01 / C-04 / C-05 / S-01；保留历史用例名，但夹具没有注册工具残留。
// 名称、时龄及另有硬链接均不是删除授权，范围内两个用户目录项应正常归类并保留。
#[test]
fn f02_removes_registered_stale_residue_within_scope() {
    let f = Fixture::new();
    let keeper = f.root.join("keeper.txt");
    let original = "唯一内容".as_bytes();
    fs::write(&keeper, original).unwrap();
    let residue = make_stale_link_temp(&f.root, &keeper, "true");

    let task =
        engine::prepare_at(&f.root, Config::default(), Context::default(), &f.state).unwrap();
    assert_eq!(task.summary.scanned, 2, "两个用户目录项都必须纳入扫描");
    assert_eq!(task.summary.planned_delete, 0, "既有硬链接不得按独立副本删除");
    assert_eq!(fs::read(&keeper).unwrap(), original, "分析必须只读");
    assert_eq!(fs::read(&residue).unwrap(), original, "分析不得清扫旧前缀目录项");
    engine::apply(&task.directory, Context::default()).unwrap();

    assert_eq!(fs::read(f.root.join("文档/keeper.txt")).unwrap(), original);
    assert_eq!(
        fs::read(f.root.join("其他").join(residue.file_name().unwrap())).unwrap(),
        original,
        "无已确认工具所有权的旧前缀硬链接应归类并完整保留"
    );
}

// 覆盖 C-01 / C-04 / C-05 / S-01；保留历史用例名，不要求无需求依据的清扫日志。
// 未注册的旧前缀夹具是用户资料；确认前原位完好，执行后两个目录项均正常归类。
#[test]
fn f02_analysis_logs_residue_notice_before_confirmation() {
    let f = Fixture::new();
    let keeper = f.root.join("keeper.txt");
    let original = "唯一内容".as_bytes();
    fs::write(&keeper, original).unwrap();
    let residue = make_stale_link_temp(&f.root, &keeper, "notice");

    let task =
        engine::prepare_at(&f.root, Config::default(), Context::default(), &f.state).unwrap();
    assert_eq!(task.summary.scanned, 2, "不得凭旧内部前缀排除用户目录项");
    assert_eq!(task.summary.planned_delete, 0, "既有硬链接不得按独立副本删除");
    assert_eq!(fs::read(&keeper).unwrap(), original, "确认前分析必须只读");
    assert_eq!(fs::read(&residue).unwrap(), original, "确认前不得清扫旧前缀目录项");

    engine::apply(&task.directory, Context::default()).unwrap();
    assert_eq!(fs::read(f.root.join("文档/keeper.txt")).unwrap(), original);
    assert_eq!(
        fs::read(f.root.join("其他").join(residue.file_name().unwrap())).unwrap(),
        original,
        "执行不得把未经注册的用户硬链接当作工具残留清扫"
    );
}

// 覆盖 S-04 / C-01 / X-08：仅带真实所有权标记的工作区被排除；
// 普通用户目录不能仅因名称恰好是工具工作目录而漏扫描。
#[test]
fn ordinary_workspace_named_directory_is_processed_without_touching_marked_workspace() {
    let f = Fixture::new();
    let wrapper = f.root.join(".jchtools-work");
    fs::create_dir_all(wrapper.join("用户资料")).unwrap();
    fs::write(wrapper.join("用户资料/report.txt"), b"user bytes").unwrap();
    let owner = "11111111-1111-4111-8111-111111111111";
    let workspace = wrapper.join(owner);
    fs::create_dir_all(workspace.join("content")).unwrap();
    fs::write(workspace.join("OWNER"), owner).unwrap();
    fs::write(workspace.join("content/keep.txt"), b"foreign workspace").unwrap();

    let task = engine::prepare_at(&f.root, Config::default(), Context::default(), &f.state).unwrap();
    assert_eq!(task.summary.scanned, 1, "用户资料必须纳入；外来工作区内容不得读取");
    engine::apply(&task.directory, Context::default()).unwrap();
    assert_eq!(fs::read(f.root.join("文档/report.txt")).unwrap(), b"user bytes");
    assert_eq!(fs::read(workspace.join("OWNER")).unwrap(), owner.as_bytes());
    assert_eq!(fs::read(workspace.join("content/keep.txt")).unwrap(), b"foreign workspace");
}

// 覆盖 S-04 / C-05：同名普通文件不是目录工作区，不得静默排除。
#[test]
fn ordinary_workspace_named_file_is_processed() {
    let f = Fixture::new();
    fs::write(f.root.join(".jchtools-work"), b"user file").unwrap();
    let task = engine::prepare_at(&f.root, Config::default(), Context::default(), &f.state).unwrap();
    assert_eq!(task.summary.scanned, 1);
    engine::apply(&task.directory, Context::default()).unwrap();
    assert_eq!(fs::read(f.root.join("其他/.jchtools-work")).unwrap(), b"user file");
}

// 覆盖 X-08 / S-04 / C-01：不能直接选择真实工作区或其后代绕过扫描排除；
// 拒绝发生在创建任务库之前，不修改外来工作区。
#[test]
fn prepare_rejects_selected_marked_workspace_and_descendants() {
    let f = Fixture::new();
    let owner = "11111111-1111-4111-8111-111111111111";
    let workspace = f.root.join(".JCHTOOLS-WORK").join(owner);
    let content = workspace.join("content");
    let nested = content.join("nested");
    fs::create_dir_all(&nested).unwrap();
    fs::write(workspace.join("OWNER"), owner).unwrap();
    fs::write(nested.join("keep.txt"), b"foreign workspace").unwrap();
    for selected in [&workspace, &content, &nested] {
        let error = engine::prepare_at(selected, Config::default(), Context::default(), &f.state)
            .expect_err("不能直接整理工作区或其后代");
        assert!(format!("{error:#}").contains("解压工作区"), "{error:#}");
        assert_eq!(count_task_dirs(&f.state), 0, "拒绝时不得创建计划");
    }
    assert_eq!(fs::read(workspace.join("OWNER")).unwrap(), owner.as_bytes());
    assert_eq!(fs::read(nested.join("keep.txt")).unwrap(), b"foreign workspace");
}
