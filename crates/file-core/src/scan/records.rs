//! 扫描并行中间数据记录：并行枚举产出、入库回放与主流程消费的共享类型
//! ScanChild/ScanKind/ScanNote/ScanSink 及锁助手 lock_sink。

use crate::model::Snapshot;
use std::{collections::HashSet, sync::Mutex};

/// 并行扫描收集到的一条子项：目录或文件。同一父目录的子项由单线程按枚举序追加，
/// 汇总后按父目录分组即可还原旧 walkdir 的深度先序（目录先于其子树、
/// 同目录内保持文件系统枚举顺序），插入顺序与旧实现一致。
pub(super) struct ScanChild {
    pub(super) parent: String,
    pub(super) name: String,
    pub(super) kind: ScanKind,
}
pub(super) enum ScanKind {
    Dir { lower: String },
    File { normal: String, snapshot: Snapshot },
}
/// 扫描期需要主线程补记的错误日志（工作线程不能触碰任务库连接）。
pub(super) struct ScanNote {
    pub(super) path: String,
    pub(super) message: String,
}
#[derive(Default)]
pub(super) struct ScanSink {
    pub(super) children: Vec<ScanChild>,
    pub(super) notes: Vec<ScanNote>,
    /// 盘上可见但未入盘点的内容所在的目录（被过滤条目、枚举/读取失败）：
    /// 空目录规划据此（planner 内向上传播）拒绝把该目录及其祖先当作空目录，
    /// 替代旧实现对每个候选目录重新走盘核对（has_unscanned_content）。
    pub(super) taint: HashSet<String>,
    /// 整树排除的 Git 目录（目录直接含 .git 的 rel）：只用于界面提示与 git_roots 表。
    pub(super) git_skips: Vec<String>,
    /// 扫描实际发现的隔离目录：只记录路径，不遍历内容。
    pub(super) quarantine_skips: Vec<String>,
    /// 已确认带完整所有权标记的解压工作区：只提示，不扫描内部。
    pub(super) workspace_skips: Vec<String>,
    /// 必要 Git 边界检查失败时拒绝继续扫描/处理，不能把未知目录当作已排除。
    pub(super) git_boundary_error: Option<String>,
}

pub(super) fn lock_sink(sink: &Mutex<ScanSink>) -> std::sync::MutexGuard<'_, ScanSink> {
    // 锁中毒只可能因持锁线程 panic；本模块持锁期间不 panic，恢复数据是安全回退。
    sink.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
