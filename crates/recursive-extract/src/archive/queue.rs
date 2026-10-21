use super::observe::{observe_archive, stops_extraction, StopExtraction};
use super::placement::quarantine;
use super::seven_zip::SevenZip;
use super::space::volume_scope_allowed;
use super::volumes::{volume_set, x10_volume_precheck};
use anyhow::{ensure, Result};
use jchtools_file_core::{config::DeleteMode, fsutil, task::Job};
use rusqlite::OptionalExtension;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
};

/// X-05：完整解开的原包与其实际分卷按永久删除处置（`volumes` 已在解压前按命名口径
/// 与档案级佐证门校验：主体 + 精确命名兄弟卷 + 已证实的宽命名兄弟卷）。
/// 复用 [`Job::delete_path`] 的既有语义（审计日志、删除计数、files 行失活），删除方式
/// 固定为永久删除，不受目录整理的全局删除方式影响。
/// 每个卷删除前检查取消：取消后不再启动后续删除（已落盘结果与未删除的卷保留）。
/// 删除失败即如实上抛为任务级中止（不是包损坏）：解压结果已落盘，未删除的卷保留在
/// 原位置，不做隔离、不虚计成功、也不回滚已删除的卷（X-05 不承诺多卷删除事务性）。
pub(super) fn delete_successful_source(
    job: &mut Job,
    archive_rel: &str,
    volumes: &[PathBuf],
) -> Result<()> {
    ensure!(
        volume_scope_allowed(job, archive_rel, volumes)?,
        "实际卷组含范围外项，禁止删源"
    );
    for (index, volume) in volumes.iter().enumerate() {
        // 取消在文件操作的安全边界生效：取消后不再启动任何删除。
        job.context.control.checkpoint()?;
        let expected = fsutil::snapshot(volume).ok();
        let reason = if index == 0 {
            "已完全解压落盘：按 X-05 永久删除原包"
        } else {
            "已完全解压落盘：按 X-05 随主体一并永久删除分卷"
        };
        if let Err(error) = job.delete_path(
            volume,
            expected.as_ref(),
            DeleteMode::Permanent,
            reason,
            true,
        ) {
            let unattempted = volumes.len() - index - 1;
            let volume_rel = fsutil::relative_string(&job.root, volume)
                .unwrap_or_else(|_| volume.display().to_string());
            let stop = StopExtraction(format!(
                "解压已完全落盘，但源包清理失败：永久删除原包/分卷时出错（{archive_rel} → {volume_rel}）；\
                 当前卷可能已删除，后续 {unattempted} 个卷尚未尝试删除；已删除的卷不回滚；不隔离本包，也不继续处理后续包：{error:#}"
            ));
            // 文件删除后的结果日志或数据库更新同样可能失败；不能据 Err 声称当前卷仍在。
            // 尽力补写日志，但始终保留清理失败的任务级中止，不再隔离已部分删除的包组。
            let _ = job.log(
                "删除",
                &volume_rel,
                "",
                "失败",
                &format!(
                    "源包清理未完成：解压结果已完全落盘，当前卷可能已删除；后续 {unattempted} 个卷未尝试删除（不隔离、不回滚）"
                ),
                expected.as_ref().map_or(0, |snapshot| snapshot.size),
            );
            return Err(stop.into());
        }
    }
    Ok(())
}

/// 新解压成员归本工具管理：7-Zip 会还原压缩包内的隐藏/系统属性，若不剥离，
/// 成员会变成扫描不可见的"影子文件"（去重/归类/清理都看不到它）。按用户当前的
/// 隐藏/系统过滤设置剥离相应属性，内容与名称不变。
/// 返回 false 表示剥离未生效，调用方必须按影子内容口径保留原包——剥离失败并不
/// 「不影响解压结果」：complete 若仍为真，原包会被删除，用户视角即内容丢失。
/// 路径必须显式补 \\?\ verbatim 前缀：裸 Win32 调用不走 std 的自动转换，
/// Windows 10 / 旧版 Windows 11（LongPathsEnabled 默认 0，清单需注册表配合）上
/// >260 字符路径会直接失败；UNC 目标须用 \\?\UNC\ 形式。
#[cfg(windows)]
pub(super) fn normalize_new_member_attributes(
    path: &Path,
    config: &jchtools_file_core::config::Config,
) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::SetFileAttributesW;
    let Ok(meta) = fs::symlink_metadata(path) else {
        return false;
    };
    let original = meta.file_attributes();
    let mut attrs = original;
    if !config.include_hidden {
        attrs &= !2;
    }
    if !config.include_system {
        attrs &= !4;
    }
    if attrs == original {
        return true;
    }
    let text: Vec<u16> = path.as_os_str().encode_wide().collect();
    // \\?\ 与 \\?\UNC\ 前缀内的路径不做规范化，按原始组件逐字拼接。
    let (prefix, skip): (&[u16], usize) =
        if text.len() >= 4 && text[..4] == [0x5C, 0x5C, 0x3F, 0x5C] {
            (&[], 0) // 已是 verbatim：std 内部构造的 PathBuf 可能带 \\?\ 前缀，按原样使用
        } else if text.len() >= 2 && text[..2] == [0x5C, 0x5C] {
            (&[0x5C, 0x5C, 0x3F, 0x5C, 0x55, 0x4E, 0x43, 0x5C], 2) // \\server\... → \\?\UNC\server\...
        } else {
            (&[0x5C, 0x5C, 0x3F, 0x5C], 0)
        };
    let mut wide: Vec<u16> = Vec::with_capacity(prefix.len() + text.len() + 1);
    wide.extend_from_slice(prefix);
    wide.extend_from_slice(&text[skip..]);
    wide.push(0);
    // SAFETY: wide 是以 NUL 结尾的 UTF-16 路径（含 verbatim 前缀）；调用只读取该缓冲区。
    let result = unsafe { SetFileAttributesW(wide.as_ptr(), attrs) };
    result != 0
}
#[cfg(not(windows))]
pub(super) fn normalize_new_member_attributes(
    _path: &Path,
    _config: &jchtools_file_core::config::Config,
) -> bool {
    true
}

#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "extract_batch", skip_all)
)]
pub fn extract_queued(job: &mut Job, resolve_engine: impl Fn() -> Result<SevenZip>) -> Result<()> {
    let control = job.context.control.clone();
    observe_archive("extract_queue", &control, |boundary_stage| {
        *boundary_stage = "queue_database"; // 引擎懒解析（E-05/X-06 边界）：缺首卷/缺入口/歧义组/缺主包的整组隔离是
                                            // 纯文件系统判定，不需要 7-Zip 引擎，先于引擎解析执行——无引擎宿主上这些
                                            // 组仍按 X-06 隔离（回归 CI run 37131023474：引擎解析前置曾把缺首卷组整体
                                            // 报错）。首个通过预检的真实解压包出现时才解析引擎；解析失败按 E-05 明确
                                            // 报错并终止任务，不得把这些包静默跳过或全部隔离。
        let mut engine: Option<SevenZip> = None;
        loop {
            *boundary_stage = "queue_database";
            job.context.control.checkpoint()?;
            let next: Option<(i64, String, u32)> = job
        .db
        .conn
        .query_row(
            "SELECT id,rel,depth FROM archives WHERE state='pending' ORDER BY depth,id LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
            let Some((id, relative, depth)) = next else {
                break;
            };
            let item_span = tracing::info_span!("archive_item", archive_id = id, depth,
        source = %jchtools_foundation::logging::safe_error(&relative));
            let _item_entered = item_span.enter();
            let item_started = std::time::Instant::now();
            tracing::debug!(event = "archive_item_started", "压缩包处理开始");
            // P-08：不复核扫描后的外部变化；仍保留操作固有的路径/链接安全边界。
            let path = fsutil::safe_join(&job.root, &relative)?;
            job.db
                .conn
                .execute("UPDATE archives SET state='running' WHERE id=?1", [id])?;
            *boundary_stage = "volume_precheck";
            let named = volume_set(&path)?;
            if !volume_scope_allowed(job, &relative, &named.paths)? {
                job.db
                    .conn
                    .execute("UPDATE archives SET state='done' WHERE id=?1", [id])?;
                job.context
                    .control
                    .completed
                    .fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let result = if depth >= job.config.max_depth {
                Err(anyhow::anyhow!("达到最大嵌套层数（X-08 防护上限）"))
            } else if let Err(reason) = x10_volume_precheck(&relative, &path, &named) {
                Err(reason)
            } else {
                if engine.is_none() {
                    *boundary_stage = "engine_resolution";
                    engine = Some(observe_archive("engine_resolution", &control, |stage| {
                        *stage = "engine_assets";
                        resolve_engine()
                    })?);
                }
                // 上一分支保证引擎已解析；此处拿不到引用只能说明内部状态被破坏，
                // 按 E-05 口径报错而不是 panic。
                let Some(active) = engine.as_ref() else {
                    anyhow::bail!("共享 7-Zip 引擎未就绪（E-05：无引擎时解压必须明确报错并停止）");
                };
                *boundary_stage = "extract_one";
                active.extract_one(job, &relative, depth)
            };
            match &result {
                Ok(complete) => tracing::debug!(
                    event = "archive_item_decoded",
                    complete,
                    elapsed_ms = jchtools_foundation::logging::elapsed_ms(item_started),
                    "压缩包解压阶段结束"
                ),
                Err(_) if job.context.control.is_cancelled() => tracing::info!(
                    event = "archive_item_cancelled",
                    elapsed_ms = jchtools_foundation::logging::elapsed_ms(item_started),
                    "压缩包处理已取消"
                ),
                Err(_) => tracing::error!(
                    event = "archive_item_failed",
                    stage = *boundary_stage,
                    error_type = "archive_boundary",
                    elapsed_ms = jchtools_foundation::logging::elapsed_ms(item_started),
                    "压缩包处理失败"
                ),
            }
            *boundary_stage = "settle_or_quarantine";
            match result {
                Ok(true) => {
                    job.db
                        .conn
                        .execute("UPDATE archives SET state='done' WHERE id=?1", [id])?;
                    // X-05/X-07：完全解开的包（含全部分卷）已在 extract_one 尾部永久删除，
                    // 删除失败会以任务级中止上抛、走不到这里；同次任务里该包已标 done，
                    // 不会再入队（X-07/H-04）。
                    job.summary.archives_ok += 1;
                    job.log(
                        "解压",
                        &relative,
                        "",
                        "成功",
                        "已完全解开；原包与分卷按 X-05 永久删除",
                        0,
                    )?;
                }
                Ok(false) => {
                    job.db
                        .conn
                        .execute("UPDATE archives SET state='done' WHERE id=?1", [id])?;
                    job.summary.archives_failed += 1;
                    job.log(
                        "解压",
                        &relative,
                        "",
                        "未完全解开",
                        "有成员被跳过或未落盘（排除规则/Git 目录树/目标冲突无法落位）；原包保留",
                        0,
                    )?;
                    // X-06：隔离自身失败（容器被占用、无法分配目标名或某卷移动失败）时，
                    // 该失败包原地保留并记录隔离失败原因与未移动源项位置，其余包可继续。
                    if let Err(error) = quarantine(
                        job,
                        &relative,
                        "未能完全解开：有成员被跳过、被排除规则命中或位于 Git 目录树内",
                    ) {
                        job.summary.errors += 1;
                        job.log(
                            "解压",
                            &relative,
                            "",
                            "隔离失败",
                            &format!("隔离未完成；源项实际位置见卷集明细：{error:#}"),
                            0,
                        )?;
                        job.context.control.check_cancelled()?;
                    }
                }
                Err(error) => {
                    // 归档行先标 failed，避免永久停在 running。
                    job.db
                        .conn
                        .execute("UPDATE archives SET state='failed' WHERE id=?1", [id])?;
                    // 用户主动取消：上抛取消错误，不隔离、不累加失败计数、不写失败日志
                    // （与 apply_with 的取消口径一致）。单出口，避免双写 failed/误计失败。
                    if job.context.control.is_cancelled() {
                        job.context.control.check_cancelled()?;
                    }
                    // X-06/X-08：空间不足不是包损坏——保留源包并停止整个解压任务，
                    // 不隔离本包，也不继续批量隔离后续正常包。
                    if stops_extraction(&error) {
                        job.summary.errors += 1;
                        return Err(error);
                    }
                    job.summary.archives_failed += 1;
                    job.summary.errors += 1;
                    job.log("解压", &relative, "", "失败", &format!("{error:#}"), 0)?;
                    // X-06：解压出错（损坏/加密/不支持/触上限）的原包移入「解压失败」。
                    // 隔离自身失败时记录隔离失败原因与原包位置，不中止其余包的处理。
                    if let Err(quarantine_error) =
                        quarantine(job, &relative, &format!("解压失败：{error:#}"))
                    {
                        job.log(
                            "解压",
                            &relative,
                            "",
                            "隔离失败",
                            &format!("隔离未完成；源项实际位置见卷集明细：{quarantine_error:#}"),
                            0,
                        )?;
                    }
                    job.context.control.check_cancelled()?;
                }
            }
            tracing::debug!(
                event = "archive_item_completed",
                elapsed_ms = jchtools_foundation::logging::elapsed_ms(item_started),
                total_ok = job.summary.archives_ok,
                total_failed = job.summary.archives_failed,
                errors = job.summary.errors,
                "压缩包处理及处置结束"
            );
            // U-03：解压页的实时计数按「已处理的包」统计（成败都算处理过）；与目录整理的
            // 计划项计数互不影响（两工具各自持有独立 Control）。
            job.context
                .control
                .completed
                .fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    })
}
