use super::boundaries::*;
use super::names::*;
use super::observe::*;
use super::placement::*;
use super::queue::*;
use super::seven_zip::*;
use super::space::*;
use super::staging::*;
use super::volumes::*;
use anyhow::Context;
use jchtools_file_core::{
    config::Config,
    control::{Context as TaskContext, Control},
    db::Database,
    fsutil, rules,
    task::{enqueue_archive, Job, QUARANTINE_DIR_NAME},
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

mod engine_cases;
mod merge_cases;
mod placement_cases;
mod staging_cases;
mod volume_cases;

/// F03 夹具：所选根下已有「解压失败」容器且容器是 Git 项目（.git 由 kind 决定
    /// 目录或文件形态），再放一个坏包，调用隔离并断言按「隔离失败」原地保留。
    #[rustfmt::skip]
    fn quarantine_git_container_case(kind: &str) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let container = root.join(QUARANTINE_DIR_NAME);
        if kind == "dir" {
            fs::create_dir_all(container.join(".git")).unwrap();
        } else {
            fs::create_dir_all(&container).unwrap();
            fs::write(container.join(".git"), b"gitdir: elsewhere\n").unwrap();
        }
        let pack = root.join("pack.zip");
        fs::write(&pack, b"broken archive bytes").unwrap();
        let mut job = Job {
            root: root.clone(),
            config: Config::default(),
            context: TaskContext::default(),
            db: Database::create(&temp.path().join("state")).unwrap(),
            summary: jchtools_file_core::model::Summary::default(),
        };
        let error = quarantine(&mut job, "pack.zip", "测试：无法解开的包").unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains("Git"),
            "隔离失败原因必须说明 Git 整树保护（H-06）：{text}"
        );
        assert!(pack.is_file(), "坏包必须原地保留（隔离失败，不腾挪不覆盖）");
        assert!(
            container.join(".git").exists(),
            "Git 项目树不得被触碰（H-06）"
        );
        // 原包没有移入容器：容器内除 .git 外不得出现任何新条目。
        let extra: Vec<_> = fs::read_dir(&container)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != ".git")
            .collect();
        assert!(
            extra.is_empty(),
            "隔离失败时不得向 Git 容器写入任何条目：{extra:?}"
        );
    }

// Windows 项目既有脚本引擎夹具惯例：只模拟输出，检查真实落盘/删源接口。
#[rustfmt::skip]
    fn review_script_engine(root: &Path, listing: &str, contents: &str) -> SevenZip {
        let path = root.join("review-engine.cmd");
        let body = format!(
            "@echo off\r\nif \"%~1\"==\"l\" (\r\n{listing}\r\nexit /b 0\r\n)\r\n\
             :args\r\nif \"%~1\"==\"\" exit /b 1\r\nset \"arg=%~1\"\r\n\
             if \"%arg:~0,2%\"==\"-o\" goto extract\r\nshift\r\ngoto args\r\n\
             :extract\r\nset \"output=%arg:~2%\"\r\n{contents}\r\nexit /b 0\r\n"
        );
        fs::write(&path, body).unwrap();
        SevenZip::with_executable(&path).unwrap()
    }

#[rustfmt::skip]
    fn review_job(root: &Path, state: &Path, max_ratio: u64) -> Job {
        Job {
            root: root.to_path_buf(),
            config: Config {
                reserve_bytes: 0,
                max_ratio,
                ..Config::default()
            },
            context: TaskContext::default(),
            db: Database::create(state).unwrap(),
            summary: jchtools_file_core::model::Summary::default(),
        }
    }

#[rustfmt::skip]
    struct ReviewF11PublishFault {
        target: PathBuf,
        code: i32,
        staged_source: Option<PathBuf>,
    }

#[rustfmt::skip]
    std::thread_local! {
        static REVIEW_F11_PUBLISH_FAULT: std::cell::RefCell<Option<ReviewF11PublishFault>> =
            const { std::cell::RefCell::new(None) };
    }

#[rustfmt::skip]
    struct ReviewF11PublishGuard;

#[rustfmt::skip]
    impl Drop for ReviewF11PublishGuard {
        fn drop(&mut self) {
            REVIEW_F11_PUBLISH_FAULT.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }

// 仅命中本线程、本次 tempfile 内的规划落点；成功读到真实暂存内容才允许注入。
// 返回原生 io::Error，不构造 StopExtraction，也不修改引擎输出或退出状态。
#[rustfmt::skip]
    pub(super) fn review_f11_publish_fault(source: &Path, target: &Path) -> Option<anyhow::Error> {
        REVIEW_F11_PUBLISH_FAULT.with(|slot| {
            let mut slot = slot.borrow_mut();
            let fault = slot.as_mut()?;
            if target != fault.target {
                return None;
            }
            assert_eq!(fs::read(source).unwrap(), b"decoded by real 7-Zip");
            assert!(fault.staged_source.is_none(), "发布故障必须只命中一次");
            fault.staged_source = Some(source.to_path_buf());
            Some(std::io::Error::from_raw_os_error(fault.code).into())
        })
    }

#[rustfmt::skip]
    fn review_f11_real_queue_publish_failure(code: i32, must_stop: bool) {
        let executable = PathBuf::from(
            std::env::var_os("JCHTOOLS_TEST_7ZIP")
                .expect("设置 JCHTOOLS_TEST_7ZIP 为真实 7z.exe 的绝对路径"),
        );
        assert!(executable.is_absolute() && executable.is_file());
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let input = temp.path().join("input");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&input).unwrap();
        fs::write(input.join("payload.txt"), b"decoded by real 7-Zip").unwrap();
        fs::write(input.join("later.txt"), b"later archive payload").unwrap();
        for (archive, member) in [("first.zip", "payload.txt"), ("later.zip", "later.txt")] {
            let status = Command::new(&executable)
                .current_dir(&input)
                .args(["a", "-y", "-tzip"])
                .arg(root.join(archive))
                .arg(member)
                .status()
                .unwrap();
            assert!(status.success(), "真实 7-Zip 必须先成功创建独占小包");
        }
        let first_bytes = fs::read(root.join("first.zip")).unwrap();
        let later_bytes = fs::read(root.join("later.zip")).unwrap();
        // 强制走生产的冲突改名发布分支，并证明既有文件不会因故障被改写。
        fs::write(root.join("payload.txt"), b"existing user payload").unwrap();
        let mut job = review_job(&root, &temp.path().join("state"), 0);
        assert_eq!(job.config.reserve_bytes, 0);
        enqueue_archive(&job, &root.join("first.zip"), 0).unwrap();
        enqueue_archive(&job, &root.join("later.zip"), 0).unwrap();
        REVIEW_F11_PUBLISH_FAULT.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(ReviewF11PublishFault {
                target: root.join("payload (1).txt"),
                code,
                staged_source: None,
            });
        });
        let _guard = ReviewF11PublishGuard;
        let result = extract_queued(&mut job, || SevenZip::with_executable(&executable));
        REVIEW_F11_PUBLISH_FAULT.with(|slot| {
            let slot = slot.borrow();
            let source = slot.as_ref().unwrap().staged_source.as_ref()
                .expect("真实引擎必须成功解码并经过暂存校验、到达生产发布边界");
            assert!(source.starts_with(root.join(".jchtools-work")));
            assert!(source.ends_with(Path::new("content").join("payload.txt")));
        });
        assert_eq!(fs::read(root.join("payload.txt")).unwrap(), b"existing user payload");
        assert!(!root.join("payload (1).txt").exists());
        let later_state: String = job.db.conn.query_row(
            "SELECT state FROM archives WHERE rel='later.zip'",
            [], |row| row.get(0),
        ).unwrap();
        if must_stop {
            assert!(
                result.is_err(),
                "原生输出 I/O 错误 {code} 必须停止真实队列；后续状态={later_state}，隔离数={}",
                job.summary.archives_quarantined,
            );
            let error = result.unwrap_err();
            assert!(error.chain().any(|cause| cause.downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.raw_os_error() == Some(code))), "{error:#}");
            assert!(stops_extraction(&error), "{error:#}");
            assert_eq!(fs::read(root.join("first.zip")).unwrap(), first_bytes);
            assert_eq!(fs::read(root.join("later.zip")).unwrap(), later_bytes);
            assert_eq!(later_state, "pending", "后续包不得开始列清单或解码");
            assert!(!root.join("later.txt").exists());
            assert!(!root.join(QUARANTINE_DIR_NAME).exists());
            assert_eq!(job.summary.archives_quarantined, 0);
            assert_eq!(job.summary.deleted, 0);
        } else {
            result.expect("权限拒绝不可被吞成任务级空间停止");
            assert_eq!(fs::read(root.join(QUARANTINE_DIR_NAME).join("first.zip")).unwrap(), first_bytes);
            assert_eq!(job.summary.archives_quarantined, 1);
            assert_eq!(job.summary.archives_failed, 1);
            assert_eq!(job.summary.archives_ok, 1);
            assert_eq!(later_state, "done");
            assert_eq!(fs::read(root.join("later.txt")).unwrap(), b"later archive payload");
            assert!(!root.join("later.zip").exists());
        }
    }
