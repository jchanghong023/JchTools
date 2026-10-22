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

//! 两工具拆分（CONTRACT X/C 分区，2026-09-18 重建）的引擎级回归：
//! - C-01 目录整理分析阶段只读：不解压、不改目录（解压职责已移交「递归解压」工具）；
//! - C-09 / X-07 扫描默认排除所选目录根下的「解压失败」子目录；
//! - X-06 解压失败的原包移入「解压失败」子目录（含分卷兄弟卷）；
//! - X-05 成功原包及已佐证分卷在完整落盘后永久删除；失败、部分解开或取消时保留，
//!   仅同主干的无关文件不随包处置。
// 测试代码允许 unwrap/expect：断言失败即测试失败，属合理用法
// （与 clippy.toml 的 allow-*-in-tests 策略一致，集成测试 crate 不在其覆盖范围内）。
#![allow(clippy::unwrap_used, clippy::expect_used)]
use jchtools_directory_organizer::engine;
use jchtools_file_core::config::*;
use jchtools_file_core::control::Context;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[path = "split/composite_staging.rs"]
mod composite_staging;
#[path = "split/containers.rs"]
mod containers;
#[path = "split/disposal.rs"]
mod disposal;
#[path = "split/organizer.rs"]
mod organizer;
#[path = "split/quarantine.rs"]
mod quarantine;

/// events 表的测试观察入口：生产只写不读（读取 API 已随死代码清理移除），
/// 行格式与写入列序一致：`time | phase | result | source | target | reason`。
fn event_lines(db: &jchtools_file_core::db::Database, limit: usize) -> Vec<String> {
    let mut statement = db
        .conn
        .prepare(
            "SELECT time,phase,result,source,target,reason FROM events ORDER BY id DESC LIMIT ?1",
        )
        .unwrap();
    let rows = statement
        .query_map([i64::try_from(limit).unwrap()], |row| {
            let values = (0..6)
                .map(|i| row.get::<_, String>(i))
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(values.join(" | "))
        })
        .unwrap();
    rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
}

/// 假引擎：一个存在但不可执行/非 PE 的文件。任何解压命令都会失败，
/// 用于在无真实 7-Zip 的环境（CI / 本地默认）驱动「解压失败 → 隔离」路径。
fn fake_engine(dir: &Path) -> PathBuf {
    let path = dir.join("fake-7z.exe");
    fs::write(&path, b"not an executable").unwrap();
    path
}

fn fixture(_tag: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("data");
    fs::create_dir(&root).unwrap();
    tmp
}

fn state_of(tmp: &tempfile::TempDir) -> PathBuf {
    let state = tmp.path().join("state");
    fs::create_dir_all(&state).unwrap();
    state
}

fn write_with_mtime(path: &Path, bytes: &[u8], mtime: i64) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, bytes).unwrap();
    filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(mtime, 0)).unwrap();
}

/// 恒成功引擎：任何子命令都 0 退出且无输出（0 条目 → 解压「成功」）。
/// 与 dispose_failure_does_not_abort_remaining_archives 的内联版本同构，供分卷回归复用。
fn ok_engine(dir: &Path) -> PathBuf {
    #[cfg(windows)]
    let (path, bytes) = (
        dir.join("fake-ok.bat"),
        b"@exit /b 0
"
        .as_slice(),
    );
    #[cfg(not(windows))]
    let (path, bytes) = (
        dir.join("fake-ok.sh"),
        b"#!/bin/sh
exit 0
"
        .as_slice(),
    );
    fs::write(&path, bytes).unwrap();
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// 记录调用的恒成功引擎：每次调用把命令行追加到脚本同目录的 `fake-log.txt`，
/// 用于证明「哪些文件真的被引擎打开过」（不经引擎 = 完全不碰）。
fn logging_engine(dir: &Path) -> PathBuf {
    #[cfg(windows)]
    let (path, bytes) = (
        dir.join("fake-log.bat"),
        b"@echo off\r\n>> \"%~dp0fake-log.txt\" echo %*\r\nexit /b 0\r\n".as_slice(),
    );
    #[cfg(not(windows))]
    let (path, bytes) = (
        dir.join("fake-log.sh"),
        b"#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$(dirname \"$0\")/fake-log.txt\"\nexit 0\n"
            .as_slice(),
    );
    fs::write(&path, bytes).unwrap();
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// 恒成功引擎，且不带 -ba 的 `l` 子命令输出带多卷标志的档案头块（`--` 与
/// `----------` 之间）：模拟 7-Zip 对真多卷包的档案级 -slt 输出（zip 的
/// Volume Index 仅 IsMultiVol 时出现、rar 的 Is Volume 仅卷标志置位时出现），
/// 用于在无真实引擎环境驱动「档案级多卷佐证为真 → 宽命名兄弟卷整组处置」路径。
/// 带 -ba 的条目列表与解压子命令静默成功（0 条目）。
fn volume_index_engine(dir: &Path, volumes: usize) -> PathBuf {
    #[cfg(windows)]
    let (path, body) = (
        dir.join("fake-volume-list.bat"),
        concat!(
            "@echo off
",
            "if not \"%~1\"==\"l\" exit /b 0
",
            "if \"%~3\"==\"-ba\" exit /b 0
",
            "echo --
",
            "echo Type = rar
",
            "echo Volumes = @COUNT@\n",
            "echo Multivolume = +
",
            "echo Volume Index = 0
",
            "echo ----------
",
            "exit /b 0
",
        ),
    );
    #[cfg(not(windows))]
    let (path, body) = (
        dir.join("fake-volume-list.sh"),
        concat!(
            "#!/bin/sh
",
            "[ \"$1\" = \"l\" ] || exit 0
",
            "[ \"$3\" = \"-ba\" ] && exit 0
",
            "printf '%s\n' '--' 'Type = rar' 'Volumes = @COUNT@' 'Multivolume = +' 'Volume Index = 0' '----------'
",
            "exit 0
",
        ),
    );
    fs::write(&path, body.replace("@COUNT@", &volumes.to_string())).unwrap();
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// 伪造注释头假引擎：不带 -ba 的 `l` 输出带多卷键的档案注释块（`{`…`}` 内），
/// 模拟 crafted 档案把 `Volumes = 9` 等键写进 zip/rar 档案注释、借 7-Zip 原样
/// 输出伪造多卷佐证的对抗形态。修复后注释块内的键不得参与判定。
fn comment_forging_engine(dir: &Path) -> PathBuf {
    #[cfg(windows)]
    let (path, body) = (
        dir.join("fake-comment-forgery.bat"),
        concat!(
            "@echo off
",
            "if not \"%~1\"==\"l\" exit /b 0
",
            "if \"%~3\"==\"-ba\" exit /b 0
",
            "echo --
",
            "echo Type = rar
",
            "echo Comment = 
",
            "echo {
",
            "echo Volumes = 9
",
            "echo Volume Index = 0
",
            "echo Multivolume = +
",
            "echo }
",
            "echo ----------
",
            "exit /b 0
",
        ),
    );
    #[cfg(not(windows))]
    let (path, body) = (
        dir.join("fake-comment-forgery.sh"),
        concat!(
            "#!/bin/sh
",
            "[ \"$1\" = \"l\" ] || exit 0
",
            "[ \"$3\" = \"-ba\" ] && exit 0
",
            "printf '%s\n' '--' 'Type = rar' 'Comment = ' '{' 'Volumes = 9' 'Volume Index = 0' 'Multivolume = +' '}' '----------'
",
            "exit 0
",
        ),
    );
    fs::write(&path, body).unwrap();
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// 伪造注释头假引擎（嵌套 } 变体）：7-Zip 把档案注释以 {...} 原样逐行输出，
/// 注释内容本身可以含 } 行——若解析器遇到注释内的 } 就退出注释跳过，其后
/// 攻击者控制的伪造键会被当作档案头键解析。修复后 } 结束注释的同时必须结束
/// 档案头块（真实输出中 Comment 是头块最后一个字段，键不会出现在其后）。
fn comment_escape_forging_engine(dir: &Path) -> PathBuf {
    #[cfg(windows)]
    let (path, body) = (
        dir.join("fake-comment-escape.bat"),
        concat!(
            "@echo off
",
            "if not \"%~1\"==\"l\" exit /b 0
",
            "if \"%~3\"==\"-ba\" exit /b 0
",
            "echo --
",
            "echo Type = rar
",
            "echo Comment = 
",
            "echo {
",
            "echo }
",
            "echo Volume Index = 0
",
            "echo Volumes = 9
",
            "echo Multivolume = +
",
            "echo }
",
            "echo ----------
",
            "exit /b 0
",
        ),
    );
    #[cfg(not(windows))]
    let (path, body) = (
        dir.join("fake-comment-escape.sh"),
        concat!(
            "#!/bin/sh
",
            "[ \"$1\" = \"l\" ] || exit 0
",
            "[ \"$3\" = \"-ba\" ] && exit 0
",
            "printf '%s\n' '--' 'Type = rar' 'Comment = ' '{' '}' 'Volume Index = 0' 'Volumes = 9' 'Multivolume = +' '}' '----------'
",
            "exit 0
",
        ),
    );
    fs::write(&path, body).unwrap();
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// 任务以错误结束（没有 TaskResult）时，打开本次运行的任务库：摘要与行状态都在里面。
#[cfg(windows)]
fn task_db(state: &Path) -> jchtools_file_core::db::Database {
    let task_dirs: Vec<PathBuf> = fs::read_dir(state.join("tasks"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(task_dirs.len(), 1, "隔离夹具应仅创建本次运行的一个任务库");
    jchtools_file_core::db::Database::open(&task_dirs[0]).unwrap()
}

/// 任务库里停留在 running 的包行数（任务结束时必须为 0）。
#[cfg(windows)]
fn running_rows(db: &jchtools_file_core::db::Database) -> i64 {
    db.conn
        .query_row(
            "SELECT COUNT(*) FROM archives WHERE state='running'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}
