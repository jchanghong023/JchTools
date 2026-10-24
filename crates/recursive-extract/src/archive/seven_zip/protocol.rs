//! 7-Zip 进程协议子模块：运行引擎子进程并解析其 stdout。
//!
//! 承接 `SevenZip` 的三个进程交互方法，由上层 `seven_zip.rs` 的 `extract_one` 管线调用：
//! - `list`：`l -slt` 成员清单解析、条目限额与危险条目校验，返回 `Listing`；
//! - `archive_volume_count`：最外层卷头格式与实际分卷数，返回 `ArchiveVolumes`；
//! - `decode_into`：`x` 解码执行、引擎空间报错解析与进度回传。

use super::super::boundaries::git_boundary_prefix;
use super::super::names::{DecodedFileName, DecoderNames, Listing};
use super::super::observe::{member_rejected, observe_archive, StopExtraction};
#[cfg(windows)]
use super::super::space::{engine_space_error_line, engine_space_messages};
use super::super::volumes::{stream_member_name, ArchiveVolumes};
use super::SevenZip;
use anyhow::{bail, ensure, Context, Result};
use jchtools_file_core::{fsutil, model::bytes, process, task::Job};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::Path,
};

impl SevenZip {
    /// 列出压缩包条目：声明总大小、大小元数据是否完整、以及 H-06 的 Git 边界子树。
    /// 7-Zip 对 bzip2/xz 等流式格式可能不输出成员 Path，甚至不输出 Size；此时不能把条目静默丢掉，
    /// 否则 total=0 会在合入阶段误报「实际解压量超过压缩包声明」。
    /// 展开比例与空间预检由调用方执行（分母是实际卷集合合计，见 extract_one）。
    pub(in crate::archive) fn list(&self, archive: &Path, job: &mut Job) -> Result<Listing> {
        let control = job.context.control.clone();
        observe_archive("list_members", &control, |boundary_stage| {
            *boundary_stage = "member_listing_protocol";
            let result: Result<Listing> = (|| {
                let mut command = self.command();
                command
                    .args(["l", "-slt", "-ba", "-sccUTF-8", "-p-", "--"])
                    .arg(archive);
                let mut fields = BTreeMap::<String, String>::new();
                let mut total = 0u64;
                let mut count = 0u64;
                let mut sizes_complete = true;
                let mut git_subtrees = HashSet::new();
                let cfg = job.config.clone();
                let mut flush = |fields: &mut BTreeMap<String, String>| -> Result<()> {
                    let raw = if let Some(raw) = fields.remove("Path") {
                        raw
                    } else {
                        // 流式格式（bzip2/xz）可能没有 Path；若块内仍有成员元数据则用包名合成。
                        if fields.is_empty() {
                            return Ok(());
                        }
                        let has_meta = fields.contains_key("Size")
                            || fields.contains_key("Packed Size")
                            || fields.contains_key("Folder")
                            || fields.contains_key("Encrypted")
                            || fields.contains_key("Attributes");
                        if !has_meta {
                            fields.clear();
                            return Ok(());
                        }
                        stream_member_name(archive)
                    };
                    // X-08：根目录本身无需落盘，但仍是归档列出的条目，重复根条目同样计数。
                    count = count.checked_add(1).context("条目计数溢出")?;
                    if count > cfg.max_entries {
                        member_rejected("entry_limit", &raw);
                        bail!("压缩包条目数量超过用户设置的上限");
                    }
                    if raw == "." || raw == "./" {
                        fields.clear();
                        return Ok(());
                    }
                    let relative = fsutil::safe_relative(&raw).inspect_err(|_| {
                        member_rejected("invalid_member_path", &raw);
                    })?;
                    let raw = fsutil::path_string(&relative)?.replace('\\', "/");
                    if raw.split('/').any(|s| {
                        s.eq_ignore_ascii_case("$RECYCLE.BIN")
                            || s.eq_ignore_ascii_case("System Volume Information")
                    }) {
                        member_rejected("protected_member_path", &raw);
                        bail!("拒绝压缩包中的系统目录条目：{raw}");
                    }
                    // H-06：成员路径里出现 .git 组件时，登记「直接含该 .git 的目录」为整树
                    // 排除边界。边界必须在合入前从完整条目清单确定：该目录及全部后代整树
                    // 排除，包括 .git 的兄弟条目——只看落盘结果会把兄弟条目先写进用户目录，
                    // 而一旦 .git 落盘，该目录在后续扫描里就是永久排除区。
                    if let Some(prefix) = git_boundary_prefix(&raw) {
                        git_subtrees.insert(prefix);
                    }
                    if fields.get("Encrypted").is_some_and(|s| s == "+") {
                        member_rejected("encrypted_archive", &raw);
                        bail!("加密压缩包需要人工处理；没有把密码写入进程命令行");
                    }
                    for field in ["Symbolic Link", "Hard Link", "Reparse", "Alternate Stream"] {
                        if fields.get(field).is_some_and(|s| !s.is_empty() && s != "-") {
                            member_rejected("link_or_alternate_stream", &raw);
                            bail!("拒绝带链接、reparse 或备用数据流的压缩包：{raw}");
                        }
                    }
                    let attr = fields.get("Attributes").cloned().unwrap_or_default();
                    if attr.split_whitespace().any(|s| s.starts_with('l')) {
                        member_rejected("unix_symlink", &raw);
                        bail!("拒绝 Unix 符号链接条目：{raw}");
                    }
                    let directory = fields.get("Folder").is_some_and(|s| s == "+")
                        || attr.starts_with('D')
                        || attr.starts_with('d');
                    let size = match fields.get("Size") {
                        // 目录条目一律按 0 计入总量：部分引擎会给目录填非 0 Size，导致合入后 expanded!=total 必败。
                        _ if directory => 0,
                        Some(s) if !s.is_empty() => {
                            s.parse::<u64>().context("压缩包条目大小无效")?
                        }
                        _ => {
                            // 流式单文件包可能不声明展开大小：记为不完整，合入阶段跳过精确大小校验。
                            sizes_complete = false;
                            0
                        }
                    };
                    total = total.checked_add(size).context("解压总大小溢出")?;
                    // X-08：单包与单文件展开体积不另设固定上限（允许 TB 级资料），
                    // 仍受条目数、展开比例、空间和文件系统能力约束。
                    // 条目信息只在此处做限额/危险校验，不再整包入库：archive_members 表此前
                    // 只写不读（上限百万条目的纯写放大），已随表一起删除。
                    fields.clear();
                    Ok(())
                };
                let ctl = job.context.control.clone();
                // 条目校验只读不落库（archive_members 已删除），无需事务壳。
                let listed = process::run(
                    &mut command,
                    &ctl,
                    |err, line| {
                        if err {
                            return Ok(());
                        }
                        if line.trim().is_empty() {
                            return flush(&mut fields);
                        }
                        if let Some((key, value)) = line.split_once(" = ") {
                            if fields.len() >= 100 {
                                tracing::error!(
                                    event = "archive_protocol_failed",
                                    stage = "member_metadata",
                                    error_type = "protocol",
                                    error_code = "metadata_field_limit",
                                    "归档成员元数据字段过多"
                                );
                                bail!("压缩包元数据异常");
                            }
                            if fields.insert(key.to_string(), value.to_string()).is_some() {
                                tracing::error!(
                                    event = "archive_protocol_failed",
                                    stage = "member_metadata",
                                    error_type = "protocol",
                                    error_code = "duplicate_metadata_field",
                                    "归档成员元数据存在重复字段"
                                );
                                bail!("压缩包元数据有重复字段，无法安全解析");
                            }
                        }
                        Ok(())
                    },
                    || Ok(()),
                )
                .and_then(|()| flush(&mut fields));
                listed?;
                Ok(Listing {
                    total,
                    sizes_complete,
                    git_subtrees,
                })
            })();
            if let Ok(listing) = &result {
                tracing::debug!(
                    event = "archive_listing_result",
                    declared_bytes = listing.total,
                    sizes_complete = listing.sizes_complete,
                    git_subtrees = listing.git_subtrees.len(),
                    "归档成员清单结果"
                );
            }
            result
        })
    }
    /// 只读取最外层档案头中的格式与实际卷数。注释和后续内层档案头不具有删除授权。
    pub(super) fn archive_volume_count(
        &self,
        archive: &Path,
        job: &mut Job,
    ) -> Result<ArchiveVolumes> {
        let control = job.context.control.clone();
        observe_archive("volume_count", &control, |boundary_stage| {
            *boundary_stage = "volume_header_protocol";
            let result: Result<ArchiveVolumes> = (|| {
                let mut command = self.command();
                command
                    .args(["l", "-slt", "-sccUTF-8", "-p-", "--"])
                    .arg(archive);
                let mut in_header = false;
                let mut header_seen = false;
                let mut kind = String::new();
                let mut count = None;
                let mut multipart = false;
                let mut new_rar_names = false;
                let ctl = job.context.control.clone();
                process::run(
                    &mut command,
                    &ctl,
                    |err, line| {
                        if err {
                            return Ok(());
                        }
                        let line = line.trim_end();
                        if line == "--" && !header_seen {
                            header_seen = true;
                            in_header = true;
                            return Ok(());
                        }
                        // Comment 是不可信的原样文本；此后绝不重新进入头块，哪怕注释伪造 } / --。
                        if line.starts_with("----") || line.starts_with("Comment =") || line == "{"
                        {
                            in_header = false;
                        }
                        if !in_header {
                            return Ok(());
                        }
                        if let Some((key, value)) = line.split_once(" = ") {
                            match key {
                                "Type" => value.clone_into(&mut kind),
                                "Volumes" => {
                                    let value =
                                        value.parse::<usize>().context("引擎分卷数量无效")?;
                                    ensure!(value > 0, "引擎分卷数量不能为零");
                                    count = Some(value);
                                }
                                "Characteristics" => {
                                    new_rar_names =
                                        value.split_whitespace().any(|flag| flag == "NewVolName");
                                }
                                "Volume Index" => multipart = true,
                                "Multivolume" => multipart |= value == "+",
                                _ => {}
                            }
                        }
                        Ok(())
                    },
                    || Ok(()),
                )
                .with_context(|| "无法确认压缩包实际分卷")?;
                ensure!(
                    !multipart || count.is_some(),
                    "引擎未提供可信的实际分卷数量，保留源包"
                );
                Ok(ArchiveVolumes {
                    kind,
                    count: count.unwrap_or(1),
                    new_rar_names,
                })
            })();
            if let Ok(info) = &result {
                tracing::debug!(
                    event = "archive_volume_result",
                    count = info.count,
                    "归档实际分卷数量"
                );
            }
            result
        })
    }
    pub(super) fn decode_into(
        &self,
        job: &Job,
        archive: &Path,
        output: &Path,
        label: &str,
    ) -> Result<HashMap<String, DecodedFileName>> {
        let control = job.context.control.clone();
        observe_archive("decode", &control, |boundary_stage| {
            *boundary_stage = "decoder_process_and_space";
            let mut names = DecoderNames::default();
            #[cfg(windows)]
            let space_messages = engine_space_messages();
            #[cfg(windows)]
            let mut system_error = false;
            let mut command = self.command();
            command
                .args([
                    "x",
                    "-aou",
                    "-y",
                    "-bb1",
                    "-bsp2",
                    "-bso1",
                    "-bse2",
                    "-sccUTF-8",
                    "-p-",
                    "-mmt=2",
                ])
                .arg(format!("-o{}", fsutil::path_string(output)?))
                .arg("--")
                .arg(archive);
            process::run(
                &mut command,
                &job.context.control,
                |err, line| {
                    #[cfg(windows)]
                    if err {
                        if let Some(error) =
                            engine_space_error_line(&mut system_error, &space_messages, line)
                        {
                            return Err(error)
                                .context("7-Zip 报告目标写入空间不足；保留源包并停止本次任务");
                        }
                    }
                    if !err {
                        if let Some(member) = line.strip_prefix("- ") {
                            return names.record(member);
                        }
                    }
                    if line.contains('%') {
                        job.context
                            .status(format!("正在解压 {label} · {}", line.trim()));
                    }
                    Ok(())
                },
                || {
                    if fs2::available_space(&job.root)
                        .map_err(|error| StopExtraction(format!("无法查询磁盘可用空间：{error}")))?
                        < job.config.reserve_bytes
                    {
                        return Err(StopExtraction(format!(
                            "磁盘剩余空间低于预留阈值 {}，已停止解压并保留原包",
                            bytes(job.config.reserve_bytes)
                        ))
                        .into());
                    }
                    Ok(())
                },
            )
            .with_context(|| "解压失败（可能已损坏、加密或格式不受支持）")?;
            for name in names.renamed.values() {
                job.context.control.checkpoint()?;
                let path = output.join(fsutil::safe_relative(&name.staged)?);
                ensure!(
                    fs::symlink_metadata(path)
                        .is_ok_and(|metadata| metadata.is_file() && !fsutil::is_link(&metadata)),
                    "解码成员改名与引擎输出不一致，未合入任何成员"
                );
            }
            Ok(names.renamed)
        })
    }
}
