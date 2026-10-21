//! 合并任务：流式逐文件逐行改写（M-04~M-07）与临时文件原子落盘。
use anyhow::{bail, Context as _, Result};
use std::{
    fs::File,
    io::{BufRead, BufReader, BufWriter, Read, Write},
    path::Path,
};

use jchtools_file_core::control::Control;

use super::heading::HeadingShift;
use super::output::{create_md_temp_output, output_entry_exists};
use super::progress::{log_md_item_result, MdProgress};
use super::scan::{reject_output_aliasing_input, MergeEntry};

/// 合并统计（M-07）。
#[derive(Debug)]
pub struct MergeStats {
    /// 参与合并的文件数
    pub files: usize,
}

/// 合并的行循环每处理多少行响应一次取消/暂停（U-12：文件内也要可停，不能只在文件边界）。
const MERGE_CHECKPOINT_LINES: usize = 512;

/// 按 M-04~M-07 把 `entries` 流式合并写入 `output`（带 Started/Completed 事件的入口）。
/// `overwrite=false` 且输出已存在时报错（调用方必须先完成覆盖确认）。
/// 写入先落在本任务独有的临时文件上，全部成功后改名到目标（M-07：失败/取消不留
/// 半成品输出；输出与任一输入是同一实体时在任何写入前拒绝，要求更换输出名）。
pub fn merge_markdown_with_events(
    entries: &[MergeEntry],
    output: &Path,
    overwrite: bool,
    control: &Control,
    on_event: &dyn Fn(MdProgress) -> Result<()>,
) -> Result<MergeStats> {
    let span = jchtools_foundation::logging::operation_span("md_tools", "merge");
    let _entered = span.enter();
    // P-10：MD 合并是关键功能任务，开始/结束统计必须落盘；取消按用户意图
    // 记 INFO，不与失败混级。
    tracing::info!(
        event = "md_merge_started",
        files = entries.len(),
        "MD 合并任务开始"
    );
    let started = std::time::Instant::now();
    let mut stage = "output_validation";
    let result = merge_task(entries, output, overwrite, control, on_event, &mut stage);
    let elapsed_ms = jchtools_foundation::logging::elapsed_ms(started);
    match &result {
        Ok(stats) => tracing::info!(
            event = "md_merge_completed",
            elapsed_ms,
            files = stats.files,
            "MD 合并任务完成"
        ),
        Err(_) if control.is_cancelled() => {
            tracing::info!(
                event = "md_merge_cancelled",
                elapsed_ms,
                "MD 合并任务已取消"
            );
        }
        Err(error) => tracing::error!(
            event = "md_merge_failed",
            elapsed_ms,
            stage,
            error_type = "md_merge_boundary",
            error_code = error
                .chain()
                .find_map(|cause| cause.downcast_ref::<std::io::Error>())
                .and_then(std::io::Error::raw_os_error),
            "MD 合并任务失败"
        ),
    }
    result
}

fn merge_task(
    entries: &[MergeEntry],
    output: &Path,
    overwrite: bool,
    control: &Control,
    on_event: &dyn Fn(MdProgress) -> Result<()>,
    stage: &mut &'static str,
) -> Result<MergeStats> {
    // 同实体拒绝先于覆盖确认：硬链接/别名输出无论如何确认都不允许写
    reject_output_aliasing_input(output, entries, control)?;
    if !overwrite && output_entry_exists(output) {
        tracing::error!(
            event = "md_output_rejected",
            component = "md_tools",
            operation = "merge",
            stage = "output_conflict",
            error_type = "output_conflict",
            error_code = "overwrite_not_confirmed",
            conflicts = 1_u64,
            "MD合并输出已存在，未确认覆盖，拒绝写入"
        );
        bail!(
            "输出文件已存在：{}（未确认覆盖，拒绝写入）",
            output.display()
        );
    }
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    *stage = "output_directory";
    std::fs::create_dir_all(parent)
        .with_context(|| format!("创建输出目录失败：{}", parent.display()))?;
    *stage = "output_create";
    let (temp_path, target) = create_md_temp_output(parent)?;
    *stage = "merge_entries";
    match write_merge_entries(entries, target, control, on_event) {
        Ok(()) => {
            *stage = "output_commit";
            match std::fs::rename(&temp_path, output) {
                Ok(()) => Ok(MergeStats {
                    files: entries.len(),
                }),
                Err(error) => {
                    if let Err(cleanup) = std::fs::remove_file(&temp_path) {
                        tracing::warn!(
                            event = "md_cleanup_failed",
                            stage = "merge_commit_rollback",
                            error_type = "io",
                            error_code = cleanup.raw_os_error(),
                            "MD合并失败临时文件清理失败"
                        );
                    }
                    Err(error).with_context(|| {
                        format!(
                            "输出文件落盘失败（{} → {}）：目标可能被占用或权限不足，已清理临时文件",
                            temp_path.display(),
                            output.display()
                        )
                    })
                }
            }
        }
        Err(error) => {
            // 取消/失败：删除本次任务拥有的临时文件，不触碰任何已有输出
            if let Err(cleanup) = std::fs::remove_file(&temp_path) {
                tracing::warn!(
                    event = "md_cleanup_failed",
                    stage = "merge_rollback",
                    error_type = "io",
                    error_code = cleanup.raw_os_error(),
                    "MD合并未完成临时文件清理失败"
                );
            }
            Err(error)
        }
    }
}

/// U-12：无换行的超长行也按 256 KiB 读取块响应暂停/取消，仍只交付完整原始行。
fn read_merge_line(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
    control: &Control,
) -> Result<usize> {
    let mut read = 0;
    loop {
        let got = (&mut *reader).take(256 * 1024).read_until(b'\n', line)?;
        read += got;
        if got == 0 || line.last() == Some(&b'\n') {
            return Ok(read);
        }
        control.checkpoint()?;
    }
}

/// 逐文件逐行把 `entries` 写入已打开的 `target`（M-04~M-06 转换规则与文件边界逻辑）。
fn write_merge_entries(
    entries: &[MergeEntry],
    target: File,
    control: &Control,
    on_event: &dyn Fn(MdProgress) -> Result<()>,
) -> Result<()> {
    let mut out = BufWriter::with_capacity(512 * 1024, target);
    let mut prev_empty = true;
    for (index, entry) in entries.iter().enumerate() {
        let item_span = tracing::info_span!("md_merge_item", item_index = index + 1, file = %jchtools_foundation::logging::safe_error(&entry.rel));
        let _item_entered = item_span.enter();
        let item_started = std::time::Instant::now();
        let mut item_stage = "callback";
        tracing::debug!(event = "md_merge_item_started", "MD合并输入项开始");
        let item_result: Result<()> = (|| {
            control.checkpoint()?;
            on_event(MdProgress::FileStarted(index + 1, entries.len()))?;
            item_stage = "output_header";
            // M-04：文件之间保证合理空行，防止上一文件最后一行与下一文件标题粘连
            if index > 0 && !prev_empty {
                out.write_all(b"\n")
                    .with_context(|| "写合并输出失败（文件分隔）".to_string())?;
            }
            let header = format!("# {}\n\n", entry.file_name);
            out.write_all(header.as_bytes())
                .with_context(|| "写合并输出失败（文件标题）".to_string())?;
            item_stage = "input_open";
            let source = File::open(&entry.path)
                .with_context(|| format!("打开输入文件失败：{}", entry.path.display()))?;
            let mut reader = BufReader::with_capacity(256 * 1024, source);
            let mut shifter = HeadingShift::default();
            let mut first_line = true;
            let mut lines_since_checkpoint = 0usize;
            let mut line: Vec<u8> = Vec::with_capacity(1024);
            loop {
                line.clear();
                item_stage = "input_read";
                let read = read_merge_line(&mut reader, &mut line, control)
                    .with_context(|| format!("读取输入文件失败：{}", entry.path.display()))?;
                if read == 0 {
                    break;
                }
                // U-12：文件内按行粒度响应取消/暂停，超大单文件也能及时停下
                lines_since_checkpoint += 1;
                if lines_since_checkpoint >= MERGE_CHECKPOINT_LINES {
                    control.checkpoint()?;
                    lines_since_checkpoint = 0;
                }
                if first_line {
                    first_line = false;
                    // 某些编辑器（Windows 旧版记事本、PowerShell 重定向等）会写出带 UTF-8 BOM 的
                    // 文件；BOM 字节会让首行不再被识别为标题，破坏 M-04/M-05 的下移（CommonMark
                    // 口径：文档开头的 BOM 被忽略）。只在每个文件开头剥除恰好一次，文件中间出现
                    // 的相同字节是普通文本、原样保留；拆分路径（M-09/M-10）是字节级无损操作，不剥。
                    const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];
                    if line.starts_with(&UTF8_BOM) {
                        line.drain(..UTF8_BOM.len());
                        if line.is_empty() {
                            continue; // 整个文件只有一个 BOM：按空文件处理
                        }
                    }
                }
                item_stage = "output_write";
                shifter.feed(&line, &mut out)?;
            }
            item_stage = "output_write";
            shifter.finish(&mut out)?;
            // 内容不以换行结束时补一个，保证下一文件标题不粘连（M-04）
            if shifter.wrote_any && !shifter.ended_newline {
                out.write_all(b"\n")
                    .with_context(|| "写合并输出失败（末行换行）".to_string())?;
            }
            prev_empty = shifter.consumed == 0;
            item_stage = "callback";
            on_event(MdProgress::FileCompleted(index + 1, entries.len()))?;
            Ok(())
        })();
        log_md_item_result("merge", item_stage, item_started, &item_result, control);
        item_result?;
    }
    out.flush().context("写输出文件失败（磁盘可能已满）")?;
    // 最后一个完成事件之后仍可能收到停止请求；正式落盘前保持取消安全边界。
    control.checkpoint()?;
    Ok(())
}
