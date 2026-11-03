//! Persistent, per-user scheduled command service.
mod runner;
mod service;
mod storage;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IntervalUnit {
    Minutes,
    Hours,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    pub id: i64,
    pub name: String,
    pub command: String,
    pub working_directory: String,
    pub interval: u64,
    pub unit: IntervalUnit,
    pub enabled: bool,
    pub shell: String,
    pub environment: BTreeMap<String, String>,
    pub timeout_seconds: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct TaskStatus {
    pub task: Task,
    pub running: bool,
    pub current_status: String,
    pub last_result: String,
    pub next_run: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub running: bool,
    pub pid: Option<u32>,
    pub tasks: Vec<TaskStatus>,
}

#[derive(Clone, Debug)]
pub struct RunRecord {
    pub id: i64,
    pub task_id: i64,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub duration_ms: Option<u64>,
    pub trigger: String,
    pub status: String,
    pub exit_code: Option<i32>,
    pub command: String,
    pub working_directory: String,
    pub truncated: bool,
}

#[derive(Clone, Debug)]
pub struct RunLog {
    pub record: RunRecord,
    pub stdout: String,
    pub stderr: String,
}

pub fn list() -> Result<Snapshot, String> {
    storage::list()
}
pub fn save(task: &Task) -> Result<i64, String> {
    storage::save(task)
}
pub fn delete(task_id: i64) -> Result<(), String> {
    storage::delete(task_id)
}
pub fn set_enabled(task_id: i64, enabled: bool) -> Result<(), String> {
    storage::set_enabled(task_id, enabled)
}
pub fn run_now(task_id: i64) -> Result<(), String> {
    storage::request_run(task_id)
}
pub fn stop_run(task_id: i64) -> Result<(), String> {
    storage::request_stop(task_id)
}
pub fn history(task_id: i64) -> Result<Vec<RunRecord>, String> {
    storage::history(task_id)
}
pub fn read_log(run_id: i64) -> Result<RunLog, String> {
    storage::read_log(run_id)
}
pub fn ensure_started() -> Result<Snapshot, String> {
    service::ensure_started(std::env::current_exe().map_err(|_| "无法定位定时任务后台程序")?)
}
pub fn serve() -> Result<(), String> {
    service::serve()
}
pub fn stop_service() -> Result<(), String> {
    service::stop_service()
}
#[cfg(feature = "test-hooks")]
pub fn ensure_started_for_test(executable: std::path::PathBuf) -> Result<Snapshot, String> {
    service::ensure_started(executable)
}
