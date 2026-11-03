use crate::{IntervalUnit, RunLog, RunRecord, Snapshot, Task, TaskStatus};
use fs2::FileExt;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

type Result<T> = std::result::Result<T, String>;

pub(crate) fn now() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

fn minute_ms() -> u64 {
    #[cfg(feature = "test-hooks")]
    if std::env::var_os("JCHTOOLS_TEST_STATE_DIR")
        .is_some_and(|path| std::path::Path::new(&path).is_absolute())
    {
        if let Some(value) = std::env::var("JCHTOOLS_TEST_SCHEDULED_MINUTE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
        {
            return value;
        }
    }
    60_000
}

fn period(task: &Task) -> Result<i64> {
    let unit = match task.unit {
        IntervalUnit::Minutes => 1,
        IntervalUnit::Hours => 60,
    };
    task.interval
        .checked_mul(unit)
        .and_then(|v| v.checked_mul(minute_ms()))
        .and_then(|v| i64::try_from(v).ok())
        .filter(|v| *v > 0)
        .ok_or_else(|| "执行周期必须为有效的正整数".into())
}

pub(crate) fn open() -> Result<Connection> {
    let root = jchtools_foundation::xberg_settings::state_dir()?;
    std::fs::create_dir_all(&root).map_err(|_| "无法创建定时任务状态目录")?;
    let db = Connection::open(root.join("scheduled-tasks.sqlite3"))
        .map_err(|_| "无法打开定时任务数据库")?;
    db.busy_timeout(Duration::from_secs(5))
        .map_err(|_| "无法设置定时任务数据库等待时间")?;
    db.execute_batch(include_str!("schema.sql"))
        .map_err(|_| "无法初始化定时任务数据库")?;
    Ok(db)
}

fn decode(config: &str, id: i64, enabled: bool) -> Result<Task> {
    let mut task: Task = serde_json::from_str(config).map_err(|_| "定时任务配置损坏")?;
    task.id = id;
    task.enabled = enabled;
    Ok(task)
}

fn validate(task: &Task) -> Result<()> {
    if task.name.trim().is_empty() {
        return Err("任务名称不能为空".into());
    }
    if task.command.trim().is_empty() {
        return Err("执行命令不能为空".into());
    }
    period(task)?;
    if !task.shell.eq_ignore_ascii_case("cmd.exe")
        && !task.shell.eq_ignore_ascii_case("powershell.exe")
    {
        return Err("请选择 cmd.exe 或 powershell.exe".into());
    }
    if task.timeout_seconds == Some(0)
        || task
            .timeout_seconds
            .is_some_and(|v| v > i64::MAX.cast_unsigned())
    {
        return Err("超时时间必须为有效的正整数秒".into());
    }
    if task
        .environment
        .keys()
        .any(|key| key.is_empty() || key.contains('=') || key.contains('\0'))
        || task.environment.values().any(|value| value.contains('\0'))
    {
        return Err("环境变量名称或值无效".into());
    }
    Ok(())
}

pub(crate) fn save(task: &Task) -> Result<i64> {
    validate(task)?;
    let mut db = open()?;
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| "无法保存定时任务")?;
    let config = serde_json::to_string(task).map_err(|_| "无法编码定时任务配置")?;
    let id = if task.id == 0 {
        let next = if task.enabled {
            Some(now().saturating_add(period(task)?))
        } else {
            None
        };
        tx.execute(
            "INSERT INTO tasks(config, enabled, next_run) VALUES(?1,?2,?3)",
            params![config, task.enabled, next],
        )
        .map_err(|_| "无法新增定时任务")?;
        tx.last_insert_rowid()
    } else {
        let old: Option<(bool, Option<i64>)> = tx
            .query_row(
                "SELECT enabled,next_run FROM tasks WHERE id=?1",
                [task.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|_| "无法读取定时任务")?;
        let (enabled, old_next) = old.ok_or("定时任务不存在")?;
        let next = if task.enabled {
            if enabled {
                old_next.or(Some(now().saturating_add(period(task)?)))
            } else {
                Some(now().saturating_add(period(task)?))
            }
        } else {
            None
        };
        tx.execute(
            "UPDATE tasks SET config=?1,enabled=?2,next_run=?3 WHERE id=?4",
            params![config, task.enabled, next, task.id],
        )
        .map_err(|_| "无法保存定时任务")?;
        task.id
    };
    tx.commit().map_err(|_| "无法提交定时任务配置")?;
    Ok(id)
}

pub(crate) fn set_enabled(id: i64, enabled: bool) -> Result<()> {
    let mut db = open()?;
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| "无法更新任务启用状态")?;
    let row: Option<(String, bool, Option<i64>)> = tx
        .query_row(
            "SELECT config,enabled,next_run FROM tasks WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(|_| "无法读取定时任务")?;
    let (config, old, next) = row.ok_or("定时任务不存在")?;
    let task = decode(&config, id, enabled)?;
    let next = if enabled {
        if old {
            next
        } else {
            Some(now().saturating_add(period(&task)?))
        }
    } else {
        None
    };
    tx.execute(
        "UPDATE tasks SET enabled=?1,next_run=?2 WHERE id=?3",
        params![enabled, next, id],
    )
    .map_err(|_| "无法更新任务启用状态")?;
    tx.commit().map_err(|_| "无法提交任务启用状态".into())
}

pub(crate) fn delete(id: i64) -> Result<()> {
    let mut db = open()?;
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| "无法删除定时任务")?;
    let running: Option<Option<i64>> = tx
        .query_row("SELECT running_run FROM tasks WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .optional()
        .map_err(|_| "无法读取定时任务")?;
    if running.ok_or("定时任务不存在")?.is_some() {
        return Err("任务正在运行，请先停止本次执行".into());
    }
    let run_ids = {
        let mut stmt = tx
            .prepare("SELECT id FROM runs WHERE task_id=?1")
            .map_err(|_| "无法读取执行记录")?;
        let rows = stmt
            .query_map([id], |row| row.get::<_, i64>(0))
            .map_err(|_| "无法读取执行记录")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|_| "无法读取执行记录")?
    };
    tx.execute("DELETE FROM tasks WHERE id=?1", [id])
        .map_err(|_| "无法删除定时任务")?;
    tx.commit().map_err(|_| "无法提交任务删除")?;
    let mut failed = false;
    for run_id in run_ids {
        if remove_log(run_id).is_err() {
            failed = true;
            tracing::warn!(
                event = "scheduled_log_cleanup_failed",
                task_id = id,
                run_id,
                stage = "delete",
                "删除任务日志失败"
            );
        }
    }
    if failed {
        return Err("任务已删除，但部分执行日志清理失败".into());
    }
    Ok(())
}

pub(crate) fn request_run(id: i64) -> Result<()> {
    let db = open()?;
    let changed = db
        .execute(
            "UPDATE tasks SET manual=1 WHERE id=?1 AND running_run IS NULL AND manual=0",
            [id],
        )
        .map_err(|_| "无法请求立即执行")?;
    if changed == 0 {
        let exists = db
            .query_row("SELECT 1 FROM tasks WHERE id=?1", [id], |_| Ok(()))
            .optional()
            .map_err(|_| "无法读取定时任务")?
            .is_some();
        return Err(if exists {
            "任务正在运行或等待执行"
        } else {
            "定时任务不存在"
        }
        .into());
    }
    Ok(())
}

pub(crate) fn request_stop(id: i64) -> Result<()> {
    let db = open()?;
    let changed = db
        .execute(
            "UPDATE tasks SET stop=1 WHERE id=?1 AND running_run IS NOT NULL",
            [id],
        )
        .map_err(|_| "无法请求停止任务")?;
    if changed == 0 {
        return Err("任务不存在或当前没有正在执行的任务".into());
    }
    Ok(())
}

pub(crate) fn list() -> Result<Snapshot> {
    let db = open()?;
    let mut statement = db.prepare("SELECT t.id,t.config,t.enabled,t.next_run,t.running_run,(SELECT status FROM runs r WHERE r.task_id=t.id AND r.status<>'运行中' ORDER BY r.id DESC LIMIT 1) FROM tasks t ORDER BY t.id").map_err(|_| "无法读取任务列表")?;
    let rows = statement
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, bool>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })
        .map_err(|_| "无法读取任务列表")?;
    let mut tasks = Vec::new();
    for row in rows {
        let (id, config, enabled, next_run, run, last) = row.map_err(|_| "无法读取任务列表")?;
        let running = run.is_some();
        tasks.push(TaskStatus {
            task: decode(&config, id, enabled)?,
            running,
            current_status: if running { "运行中" } else { "空闲" }.into(),
            last_result: last.unwrap_or_default(),
            next_run,
        });
    }
    let (running, pid) = crate::service::status()?;
    Ok(Snapshot {
        running,
        pid,
        tasks,
    })
}

fn record(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunRecord> {
    let duration: Option<i64> = row.get(4)?;
    Ok(RunRecord {
        id: row.get(0)?,
        task_id: row.get(1)?,
        started_at: row.get(2)?,
        ended_at: row.get(3)?,
        duration_ms: duration.and_then(|v| u64::try_from(v).ok()),
        trigger: row.get(5)?,
        status: row.get(6)?,
        exit_code: row.get(7)?,
        command: row.get(8)?,
        working_directory: row.get(9)?,
        truncated: row.get(10)?,
    })
}
const RECORD_COLUMNS: &str =
    "id,task_id,started,ended,duration,trigger,status,exit_code,command,directory,truncated";

pub(crate) fn history(id: i64) -> Result<Vec<RunRecord>> {
    let db = open()?;
    let mut stmt = db
        .prepare(&format!(
            "SELECT {RECORD_COLUMNS} FROM runs WHERE task_id=?1 ORDER BY id DESC LIMIT 100"
        ))
        .map_err(|_| "无法读取执行历史")?;
    let rows = stmt
        .query_map([id], record)
        .map_err(|_| "无法读取执行历史")?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|_| "无法读取执行历史".into())
}

fn log_path(run_id: i64) -> Result<PathBuf> {
    Ok(jchtools_foundation::xberg_settings::state_dir()?
        .join("scheduled-tasks-logs")
        .join(format!("{run_id}.log")))
}

fn remove_log(run_id: i64) -> Result<()> {
    match std::fs::remove_file(log_path(run_id)?) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("无法清理执行日志".into()),
    }
}

fn decode_log(bytes: Vec<u8>, is_cmd: bool) -> String {
    let bytes = match String::from_utf8(bytes) {
        Ok(text) => return text,
        Err(error) => error.into_bytes(),
    };
    #[cfg(windows)]
    if is_cmd {
        use windows_sys::Win32::Globalization::{MultiByteToWideChar, CP_OEMCP};

        if let Ok(len) = i32::try_from(bytes.len()) {
            // SAFETY: The input is valid for `len` bytes; a null output asks for the required size.
            let needed = unsafe {
                MultiByteToWideChar(CP_OEMCP, 0, bytes.as_ptr(), len, std::ptr::null_mut(), 0)
            };
            if let Some(size) = usize::try_from(needed).ok().filter(|size| *size > 0) {
                let mut wide = vec![0_u16; size];
                // SAFETY: The input and output buffers have the lengths passed to Windows.
                let written = unsafe {
                    MultiByteToWideChar(CP_OEMCP, 0, bytes.as_ptr(), len, wide.as_mut_ptr(), needed)
                };
                if written == needed {
                    return String::from_utf16_lossy(&wide);
                }
            }
        }
    }
    #[cfg(not(windows))]
    let _ = is_cmd;
    String::from_utf8_lossy(&bytes).into_owned()
}

pub(crate) fn read_log(id: i64) -> Result<RunLog> {
    let db = open()?;
    let (record, config): (RunRecord, String) = db
        .query_row(
            &format!("SELECT {RECORD_COLUMNS},config FROM runs WHERE id=?1"),
            [id],
            |row| Ok((record(row)?, row.get(11)?)),
        )
        .optional()
        .map_err(|_| "无法读取执行日志")?
        .ok_or("执行记录不存在")?;
    let mut file = match File::open(log_path(id)?) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RunLog {
                record,
                stdout: String::new(),
                stderr: String::new(),
            });
        }
        Err(_) => return Err("无法读取执行日志".into()),
    };
    FileExt::lock_shared(&file).map_err(|_| "无法锁定执行日志")?;
    let result = (|| -> Result<(Vec<u8>, Vec<u8>)> {
        if file.metadata().map_err(|_| "无法读取执行日志")?.len() == 0 {
            return Ok((Vec::new(), Vec::new()));
        }
        let mut header = [0_u8; 8];
        file.read_exact(&mut header)
            .map_err(|_| "无法读取执行日志")?;
        let stdout_len = usize::try_from(u64::from_le_bytes(header)).map_err(|_| "执行日志损坏")?;
        let size = file.metadata().map_err(|_| "无法读取执行日志")?.len();
        let body_len = usize::try_from(size.checked_sub(8).ok_or("执行日志损坏")?)
            .map_err(|_| "执行日志损坏")?;
        if body_len > 1_000_000 || stdout_len > body_len {
            return Err("执行日志损坏".into());
        }
        let mut stdout = vec![0; stdout_len];
        let mut stderr = vec![0; body_len - stdout_len];
        file.read_exact(&mut stdout)
            .map_err(|_| "无法读取执行日志")?;
        file.read_exact(&mut stderr)
            .map_err(|_| "无法读取执行日志")?;
        Ok((stdout, stderr))
    })();
    let _ = FileExt::unlock(&file);
    let (stdout, stderr) = result?;
    #[cfg(windows)]
    let is_cmd = serde_json::from_str::<Task>(&config)
        .map_err(|_| "定时任务配置损坏")?
        .shell
        .eq_ignore_ascii_case("cmd.exe");
    #[cfg(not(windows))]
    let _ = config;
    #[cfg(not(windows))]
    let is_cmd = false;
    Ok(RunLog {
        record,
        stdout: decode_log(stdout, is_cmd),
        stderr: decode_log(stderr, is_cmd),
    })
}

pub(crate) fn recover() -> Result<()> {
    let mut db = open()?;
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| "无法恢复定时任务")?;
    {
        let mut stmt = tx
            .prepare("SELECT id,task_id,started FROM runs WHERE ended IS NULL AND status='运行中'")
            .map_err(|_| "无法读取中断记录")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(|_| "无法读取中断记录")?;
        for row in rows {
            let (run_id, task_id, started) = row.map_err(|_| "无法读取中断记录")?;
            tracing::warn!(
                event = "scheduled_run_interrupted",
                task_id,
                run_id,
                elapsed_ms = now().saturating_sub(started),
                category = "interrupted",
                "定时任务执行中断"
            );
        }
    }
    tx.execute(
        "UPDATE runs SET status='中断' WHERE ended IS NULL AND status='运行中'",
        [],
    )
    .map_err(|_| "无法恢复中断记录")?;
    tx.execute("UPDATE tasks SET running_run=NULL,stop=0", [])
        .map_err(|_| "无法恢复任务状态")?;
    {
        let mut stmt = tx.prepare("SELECT id,config,next_run FROM tasks WHERE enabled=1 AND next_run IS NOT NULL AND next_run<=?1").map_err(|_| "无法恢复执行计划")?;
        let rows = stmt
            .query_map([now()], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .map_err(|_| "无法恢复执行计划")?;
        for row in rows {
            let (id, config, next) = row.map_err(|_| "无法恢复执行计划")?;
            let task = decode(&config, id, true)?;
            let next = advance(next, period(&task)?, now());
            tx.execute(
                "UPDATE tasks SET next_run=?1 WHERE id=?2",
                params![next, id],
            )
            .map_err(|_| "无法恢复执行计划")?;
        }
    }
    tx.commit().map_err(|_| "无法提交任务恢复".into())
}

fn advance(next: i64, period: i64, current: i64) -> i64 {
    let count = current.saturating_sub(next) / period + 1;
    next.saturating_add(period.saturating_mul(count))
}

pub(crate) struct Start {
    pub task: Task,
    pub run_id: i64,
    pub started: i64,
}

pub(crate) fn poll() -> Result<Vec<Start>> {
    let mut db = open()?;
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| "无法调度定时任务")?;
    let current = now();
    let mut starts = Vec::new();
    let mut expired = Vec::new();
    {
        let mut stmt = tx.prepare("SELECT id,config,enabled,next_run,running_run,manual FROM tasks WHERE manual=1 OR (enabled=1 AND next_run<=?1)").map_err(|_| "无法读取执行计划")?;
        let rows = stmt
            .query_map([current], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, bool>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, bool>(5)?,
                ))
            })
            .map_err(|_| "无法读取执行计划")?;
        for row in rows {
            let (id, config, enabled, next, running, manual) =
                row.map_err(|_| "无法读取执行计划")?;
            let task = decode(&config, id, enabled)?;
            let due = enabled && next.is_some_and(|value| value <= current);
            if due {
                let updated = advance(next.ok_or("执行计划损坏")?, period(&task)?, current);
                tx.execute(
                    "UPDATE tasks SET next_run=?1 WHERE id=?2",
                    params![updated, id],
                )
                .map_err(|_| "无法推进执行计划")?;
            }
            if manual {
                tx.execute("UPDATE tasks SET manual=0 WHERE id=?1", [id])
                    .map_err(|_| "无法处理立即执行请求")?;
            }
            if running.is_some() || (!due && !manual) {
                continue;
            }
            let trigger = if manual { "手动" } else { "周期" };
            let snapshot = serde_json::to_string(&task).map_err(|_| "无法保存执行配置快照")?;
            tx.execute("INSERT INTO runs(task_id,started,trigger,status,command,directory,config) VALUES(?1,?2,?3,'运行中',?4,?5,?6)", params![id,current,trigger,task.command,task.working_directory,snapshot]).map_err(|_| "无法创建执行记录")?;
            let run_id = tx.last_insert_rowid();
            tx.execute(
                "UPDATE tasks SET running_run=?1,stop=0 WHERE id=?2",
                params![run_id, id],
            )
            .map_err(|_| "无法登记执行状态")?;
            {
                let mut stmt = tx
                    .prepare(
                        "SELECT id FROM runs WHERE task_id=?1 ORDER BY id DESC LIMIT -1 OFFSET 100",
                    )
                    .map_err(|_| "无法读取过期执行记录")?;
                let rows = stmt
                    .query_map([id], |row| row.get::<_, i64>(0))
                    .map_err(|_| "无法读取过期执行记录")?;
                for row in rows {
                    expired.push((id, row.map_err(|_| "无法读取过期执行记录")?));
                }
            }
            tx.execute("DELETE FROM runs WHERE task_id=?1 AND id NOT IN (SELECT id FROM runs WHERE task_id=?1 ORDER BY id DESC LIMIT 100)",[id]).map_err(|_| "无法清理过期执行记录")?;
            starts.push(Start {
                task,
                run_id,
                started: current,
            });
        }
    }
    tx.commit().map_err(|_| "无法提交执行计划")?;
    for (task_id, run_id) in expired {
        if remove_log(run_id).is_err() {
            tracing::warn!(
                event = "scheduled_log_cleanup_failed",
                task_id,
                run_id,
                stage = "retention",
                "清理过期执行日志失败"
            );
        }
    }
    Ok(starts)
}

pub(crate) fn stop_requested(task_id: i64, run_id: i64) -> Result<bool> {
    let db = open()?;
    db.query_row(
        "SELECT stop FROM tasks WHERE id=?1 AND running_run=?2",
        params![task_id, run_id],
        |r| r.get(0),
    )
    .optional()
    .map(|v| v.unwrap_or(true))
    .map_err(|_| "无法读取停止请求".into())
}

pub(crate) struct LogWriter {
    file: File,
    db: Connection,
    run_id: i64,
    truncated: bool,
}

impl LogWriter {
    pub(crate) fn new(run_id: i64) -> Result<Self> {
        let path = log_path(run_id)?;
        std::fs::create_dir_all(path.parent().ok_or("无法创建日志目录")?)
            .map_err(|_| "无法创建日志目录")?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|_| "无法创建执行日志")?;
        Ok(Self {
            file,
            db: open()?,
            run_id,
            truncated: false,
        })
    }

    pub(crate) fn write(&mut self, stdout: &[u8], stderr: &[u8], truncated: bool) -> Result<()> {
        FileExt::lock_exclusive(&self.file).map_err(|_| "无法锁定执行日志")?;
        let result = (|| -> std::io::Result<()> {
            self.file.seek(SeekFrom::Start(0))?;
            self.file.write_all(&(stdout.len() as u64).to_le_bytes())?;
            self.file.write_all(stdout)?;
            self.file.write_all(stderr)?;
            self.file
                .set_len(8 + stdout.len() as u64 + stderr.len() as u64)?;
            Ok(())
        })();
        let _ = FileExt::unlock(&self.file);
        result.map_err(|_| "无法保存执行日志")?;
        if truncated && !self.truncated {
            self.db
                .execute("UPDATE runs SET truncated=1 WHERE id=?1", [self.run_id])
                .map_err(|_| "无法保存执行日志截断状态")?;
            self.truncated = true;
        }
        Ok(())
    }
}

pub(crate) fn finish(
    task_id: i64,
    run_id: i64,
    started: i64,
    status: &str,
    exit_code: Option<i32>,
) -> Result<()> {
    let mut db = open()?;
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| "无法更新执行结果")?;
    let ended = now();
    tx.execute(
        "UPDATE runs SET ended=?1,duration=?2,status=?3,exit_code=?4 WHERE id=?5",
        params![
            ended,
            ended.saturating_sub(started),
            status,
            exit_code,
            run_id
        ],
    )
    .map_err(|_| "无法保存执行结果")?;
    tx.execute(
        "UPDATE tasks SET running_run=NULL,stop=0 WHERE id=?1 AND running_run=?2",
        params![task_id, run_id],
    )
    .map_err(|_| "无法更新任务状态")?;
    tx.commit().map_err(|_| "无法提交执行结果".into())
}
