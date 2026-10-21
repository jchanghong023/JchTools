//! 拆分任务：UTF-8 安全边界规划（M-09/M-10）与分片临时文件落盘（M-11）。
use anyhow::{bail, Context as _, Result};
use std::{
    fs::File,
    io::{BufWriter, Read, Seek, SeekFrom, Write},
    path::Path,
};

use jchtools_file_core::control::Control;

use super::output::{conflicting_outputs, create_md_temp_output, split_names};
use super::progress::{log_md_item_result, MdProgress};

/// 拆分计划（M-09/M-10）：`bounds[i]` 是第 i 片的结束字节偏移（严格递增）。
#[derive(Debug)]
pub struct SplitPlan {
    pub bounds: Vec<u64>,
    /// 每片的最大字节数（用户设置的硬限制）
    pub limit: u64,
}

/// 规划分片边界：每片 ≤ limit 且不切断多字节字符（M-09/M-10）。
/// 限制小到无法同时满足两个要求时报错（此时不产生任何输出，由两遍法保证）。
/// 规划每 512 个边界响应暂停/取消，避免超大文件或极小分片限制阻塞停止（H-02/U-12）。
pub fn plan_splits(input: &Path, limit: u64, control: &Control) -> Result<SplitPlan> {
    let span = jchtools_foundation::logging::operation_span("md_tools", "split_plan");
    let _entered = span.enter();
    let started = std::time::Instant::now();
    tracing::debug!(event = "md_split_plan_started", limit, "MD拆分规划开始");
    let result: Result<SplitPlan> = (|| {
        control.checkpoint()?;
        let len = std::fs::metadata(input)
            .with_context(|| format!("读取输入文件属性失败：{}", input.display()))?
            .len();
        if limit == 0 {
            tracing::error!(
                event = "md_input_rejected",
                stage = "split_limit",
                error_type = "invalid_limit",
                error_code = "zero_limit",
                "MD拆分大小必须大于零"
            );
            bail!("拆分大小必须大于 0 字节");
        }
        if len == 0 {
            return Ok(SplitPlan {
                bounds: Vec::new(),
                limit,
            });
        }
        let mut file =
            File::open(input).with_context(|| format!("打开输入文件失败：{}", input.display()))?;
        let mut bounds = Vec::new();
        let mut start = 0u64;
        while start < len {
            if bounds.len() % 512 == 0 {
                control.checkpoint()?;
            }
            let target = start.saturating_add(limit).min(len);
            if target == len {
                bounds.push(len);
                break;
            }
            let cut = prev_boundary(&mut file, target, start)
                .with_context(|| format!("规划拆分边界失败：{}", input.display()))?;
            if cut <= start {
                tracing::error!(
                    event = "md_input_rejected",
                    stage = "split_boundary",
                    error_type = "invalid_limit",
                    error_code = "limit_smaller_than_character",
                    limit,
                    "MD拆分大小不足以容纳完整字符"
                );
                bail!(
            "拆分限制（{limit} 字节）过小：无法在不切断多字节字符的前提下分片（M-10）；请增大限制"
        );
            }
            bounds.push(cut);
            start = cut;
        }
        control.checkpoint()?;
        Ok(SplitPlan { bounds, limit })
    })();
    match &result {
        Ok(plan) => tracing::debug!(
            event = "md_split_plan_completed",
            parts = plan.bounds.len(),
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "MD拆分规划完成"
        ),
        Err(_) if control.is_cancelled() => tracing::info!(
            event = "md_split_plan_cancelled",
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "MD拆分规划已取消"
        ),
        Err(error) => {
            let io = error
                .chain()
                .find_map(|cause| cause.downcast_ref::<std::io::Error>());
            tracing::error!(event = "md_split_plan_failed", stage = "input_boundary_plan", error_type = if io.is_some() { "io" } else { "markdown_input" }, error_code = io.and_then(std::io::Error::raw_os_error), elapsed_ms = jchtools_foundation::logging::elapsed_ms(started), error = %jchtools_foundation::logging::safe_error(&format!("{error:#}")), "MD拆分规划失败");
        }
    }
    result
}

/// 取 ≤target 的最大 UTF-8 安全边界：位置 p 是边界 ⟺ 该处字节不是 UTF-8 连续字节
/// （`10xxxxxx`）。合法 UTF-8 的连续字节串至多 3 个，窗口取 8 字节足够；
/// 极端非法序列全部为连续字节时回退 start+1（字节内容仍不丢失，无损性不受影响）。
fn prev_boundary(file: &mut File, target: u64, start: u64) -> Result<u64> {
    // 窗口须包含 start 自身：start 处的字符若跨过 target（限制小于该字符的字节数），
    // 从 target 回溯遇到的第一个非连续字节正是 start，据此判定「限制过小」而非兜底切分。
    let lo = target.saturating_sub(8).max(start);
    let count = usize::try_from(target - lo + 1).unwrap_or(9).min(9);
    let mut buf = [0u8; 9];
    file.seek(SeekFrom::Start(lo))
        .with_context(|| "定位拆分边界失败".to_string())?;
    file.read_exact(&mut buf[..count])
        .with_context(|| "读取拆分边界字节失败".to_string())?;
    for offset in (0..count).rev() {
        if buf[offset] & 0xC0 != 0x80 {
            let position = lo + u64::try_from(offset).unwrap_or(0);
            if position > start {
                return Ok(position);
            }
            // 唯一的非连续字节就在 start：start 处的多字节字符长于剩余限制，
            // 任何 ≤target 的切分都会切断它——按限制过小报错（M-10），绝不兜底硬切。
            bail!("限制小于单个多字节字符的字节数，无法在字符边界分片");
        }
    }
    // 窗口内全部是连续字节：只可能出现在非法 UTF-8 序列（合法序列至多 3 个连续字节）；
    // 非法序列无字符语义，按单字节推进保证任务可完成，字节内容仍不丢失（无损性不受影响）。
    Ok(start + 1)
}

/// 按计划写出全部分片，返回写入的总字节数（带 Started/Completed 事件的入口）。
/// `overwrite=false` 且任一目标已存在时报错（调用方必须先完成冲突确认，M-11）。
/// 逐片分块复制，不在内存中持有整个文件（M-11 大文件实现）。
pub fn run_split_with_events(
    input: &Path,
    plan: &SplitPlan,
    out_dir: &Path,
    overwrite: bool,
    control: &Control,
    on_event: &dyn Fn(MdProgress) -> Result<()>,
) -> Result<u64> {
    let span = jchtools_foundation::logging::operation_span("md_tools", "split");
    let _entered = span.enter();
    // P-10：MD 拆分是关键功能任务，开始/结束统计必须落盘；取消按用户意图
    // 记 INFO，不与失败混级。
    tracing::info!(
        event = "md_split_started",
        parts = plan.bounds.len(),
        "MD 拆分任务开始"
    );
    let started = std::time::Instant::now();
    let mut stage = "output_validation";
    let result = split_task(
        input, plan, out_dir, overwrite, control, on_event, &mut stage,
    );
    let elapsed_ms = jchtools_foundation::logging::elapsed_ms(started);
    match &result {
        Ok(bytes) => tracing::info!(
            event = "md_split_completed",
            elapsed_ms,
            bytes_written = bytes,
            "MD 拆分任务完成"
        ),
        Err(_) if control.is_cancelled() => {
            tracing::info!(
                event = "md_split_cancelled",
                elapsed_ms,
                "MD 拆分任务已取消"
            );
        }
        Err(error) => tracing::error!(
            event = "md_split_failed",
            elapsed_ms,
            stage,
            error_type = "md_split_boundary",
            error_code = error
                .chain()
                .find_map(|cause| cause.downcast_ref::<std::io::Error>())
                .and_then(std::io::Error::raw_os_error),
            "MD 拆分任务失败"
        ),
    }
    result
}

fn split_task(
    input: &Path,
    plan: &SplitPlan,
    out_dir: &Path,
    overwrite: bool,
    control: &Control,
    on_event: &dyn Fn(MdProgress) -> Result<()>,
    stage: &mut &'static str,
) -> Result<u64> {
    let file_name = input
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let names = split_names(&file_name, plan.bounds.len());
    if !overwrite {
        let hits = conflicting_outputs(out_dir, &names);
        if !hits.is_empty() {
            tracing::error!(
                event = "md_output_rejected",
                component = "md_tools",
                operation = "split",
                stage = "output_conflict",
                error_type = "output_conflict",
                error_code = "overwrite_not_confirmed",
                conflicts = hits.len(),
                "MD拆分目标已存在，未确认覆盖，拒绝写入"
            );
            bail!(
                "目标目录已存在同名分片（{} 个，如 {}）；未确认覆盖，拒绝写入",
                hits.len(),
                hits.first()
                    .map_or(String::new(), |p| p.display().to_string())
            );
        }
    }
    *stage = "output_directory";
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("创建输出目录失败：{}", out_dir.display()))?;
    *stage = "input_open";
    let mut reader =
        File::open(input).with_context(|| format!("打开输入文件失败：{}", input.display()))?;
    let mut buffer = vec![0u8; 256 * 1024];
    let mut written = 0u64;
    let mut start = 0u64;
    *stage = "split_parts";
    for (index, end) in plan.bounds.iter().enumerate() {
        let item_span = tracing::info_span!("md_split_item", item_index = index + 1);
        let _item_entered = item_span.enter();
        let item_started = std::time::Instant::now();
        let mut item_stage = "callback";
        tracing::debug!(
            event = "md_split_item_started",
            bytes = end.saturating_sub(start),
            "MD分片开始"
        );
        let item_result: Result<()> = (|| {
            control.checkpoint()?;
            on_event(MdProgress::FileStarted(index + 1, plan.bounds.len()))?;
            let path = out_dir.join(&names[index]);
            // M-08/M-11：分片先写本任务独有的临时文件，成功后 rename 到目标。
            // rename 只替换目录项：确认覆盖的分片若与输入是同一实体的硬链接，
            // 输入名仍指向原字节（File::create 直接截断会破坏共享 inode 的另一
            // 个名字，即输入本身）；失败或取消时清理临时文件，不留半成品。
            item_stage = "output_create";
            let (temp_path, target_file) = create_md_temp_output(out_dir)?;
            let write_result = (|| -> Result<()> {
                let mut out = BufWriter::with_capacity(256 * 1024, target_file);
                item_stage = "input_seek";
                reader
                    .seek(SeekFrom::Start(start))
                    .with_context(|| format!("定位输入失败：{}", input.display()))?;
                let mut remain = end.saturating_sub(start);
                while remain > 0 {
                    // U-12：分片复制循环内逐块响应取消/暂停，超大单片也能及时停下
                    control.checkpoint()?;
                    let want = usize::try_from(remain)
                        .unwrap_or(buffer.len())
                        .min(buffer.len());
                    item_stage = "input_read";
                    let got = reader
                        .read(&mut buffer[..want])
                        .with_context(|| format!("读取输入文件失败：{}", input.display()))?;
                    if got == 0 {
                        bail!("读取输入意外结束：{}", input.display());
                    }
                    item_stage = "output_write";
                    out.write_all(&buffer[..got])
                        .with_context(|| format!("写分片失败：{}", path.display()))?;
                    written += u64::try_from(got).unwrap_or(0);
                    remain -= u64::try_from(got).unwrap_or(0);
                }
                item_stage = "output_flush";
                out.flush()
                    .with_context(|| format!("写分片失败（磁盘可能已满）：{}", path.display()))?;
                drop(out);
                item_stage = "output_commit";
                std::fs::rename(&temp_path, &path).with_context(|| {
                    format!(
                        "分片落盘失败（{} → {}）：目标可能被占用或权限不足，已清理临时文件",
                        temp_path.display(),
                        path.display()
                    )
                })
            })();
            if let Err(error) = write_result {
                if let Err(cleanup) = std::fs::remove_file(&temp_path) {
                    tracing::warn!(
                        event = "md_cleanup_failed",
                        stage = "split_rollback",
                        error_type = "io",
                        error_code = cleanup.raw_os_error(),
                        "MD拆分未完成临时文件清理失败"
                    );
                }
                return Err(error);
            }
            start = *end;
            item_stage = "callback";
            on_event(MdProgress::FileCompleted(index + 1, plan.bounds.len()))?;
            Ok(())
        })();
        log_md_item_result("split", item_stage, item_started, &item_result, control);
        item_result?;
    }
    Ok(written)
}

/// 按计划写出全部分片，返回写入的总字节数（兼容旧入口）。
/// `overwrite=false` 且任一目标已存在时报错（调用方必须先完成冲突确认，M-11）。
/// 每片处理前回调 `on_part(当前序号, 总数)`，并在分片边界响应取消与进度。
pub fn run_split(
    input: &Path,
    plan: &SplitPlan,
    out_dir: &Path,
    overwrite: bool,
    control: &Control,
    on_part: &dyn Fn(usize, usize) -> Result<()>,
) -> Result<u64> {
    run_split_with_events(
        input,
        plan,
        out_dir,
        overwrite,
        control,
        &|event| match event {
            MdProgress::FileStarted(index, total) => on_part(index, total),
            MdProgress::FileCompleted(..) => Ok(()),
        },
    )
}
