use crate::{
    config::{ConflictPolicy, DeleteMode},
    control::ConflictInfo,
    engine::Job,
    fsutil, hashing,
    model::bytes,
    platform::DeleteResult,
    process, rules,
};
use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::Ordering,
    time::{Duration, SystemTime},
};

/// 「解压失败」子目录名（X-06）：建在所选目录根下，收纳未能完全解开的原包。
/// 扫描（两工具）与重跑计数默认排除它（X-07 / C-09）。
pub const QUARANTINE_DIR_NAME: &str = "解压失败";

pub struct SevenZip {
    executable: PathBuf,
}
impl SevenZip {
    pub fn from_bundle() -> Result<Self> {
        // 随包目录与内嵌引擎统一走 resolve_executable：随包路径与 AGENTS §3.1 一致
        // 仅警告放行（LGPL 允许替换外部 7-Zip 引擎），内嵌释放路径保留硬校验。
        // 不再对随包路径做硬哈希 bail——那会拒绝用户自备/替换的引擎，与 LGPL 冲突。
        Ok(Self {
            executable: crate::engine_bundle::resolve_executable()?,
        })
    }
    /// Explicit test/development injection, never inferred from the system PATH.
    pub fn with_executable(executable: &Path) -> Result<Self> {
        Ok(Self {
            executable: fs::canonicalize(executable).context("指定的测试解压引擎不存在")?,
        })
    }
    fn command(&self) -> Command {
        let mut command = Command::new(&self.executable);
        if let Some(directory) = self.executable.parent() {
            command.current_dir(directory);
        }
        command
    }
    /// 列出压缩包条目。返回 (声明总大小, 大小元数据是否完整)。
    /// 7-Zip 对 bzip2/xz 等流式格式可能不输出成员 Path，甚至不输出 Size；此时不能把条目静默丢掉，
    /// 否则 total=0 会在合入阶段误报「实际解压量超过压缩包声明」。
    fn list(&self, archive: &Path, job: &mut Job) -> Result<(u64, bool)> {
        let mut command = self.command();
        command
            .args(["l", "-slt", "-ba", "-sccUTF-8", "-p-", "--"])
            .arg(archive);
        let mut fields = BTreeMap::<String, String>::new();
        let mut total = 0u64;
        let mut count = 0u64;
        let mut sizes_complete = true;
        // 归档是否位于选定根：只有此时成员的顶层组件才会落进 scan 的顶层隔离剪枝，
        // 「解压失败」保留名判定才适用（子目录归档的同名成员对 scan 可见，见下方 guard）。
        let archive_at_root =
            fsutil::relative_string(&job.root, archive).map_or(true, |rel| !rel.contains('/'));
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
            if raw == "." || raw == "./" {
                fields.clear();
                return Ok(());
            }
            let relative = fsutil::safe_relative(&raw)?;
            let raw = fsutil::path_string(&relative)?.replace('\\', "/");
            if raw.split('/').any(|s| {
                s.eq_ignore_ascii_case(".jchtools-work")
                    || s.eq_ignore_ascii_case("$RECYCLE.BIN")
                    || s.eq_ignore_ascii_case("System Volume Information")
            }) {
                bail!("拒绝压缩包中的程序工作区/系统目录条目：{raw}");
            }
            // 保留命名空间（按成员的落盘位置对齐 scan 剪枝口径）：任意层
            // .jchtools-link-* 组件与 scan 按名剪枝同构；顶层「解压失败」组件仅在
            // 归档位于选定根时落进 scan 永久盲区——子目录归档的同名成员落在归档
            // 自己的目录下（如 sub/解压失败/x），scan 可见、两工具可管理，不拒绝。
            // 影子内容成员落盘后原包会按「完全解开」处置（默认永久删除），用户唯一
            // 副本随之流入不可管理区域，故整包隔离（X-06 危险条目，可逆、有日志）。
            let top_component = raw.split('/').next().unwrap_or_default();
            if (archive_at_root && top_component == QUARANTINE_DIR_NAME)
                || raw.split('/').any(|s| s.starts_with(".jchtools-link-"))
            {
                bail!("拒绝压缩包中的「解压失败」暂存区或内部链接标记条目：{raw}");
            }
            if fields.get("Encrypted").is_some_and(|s| s == "+") {
                bail!("加密压缩包需要人工处理；没有把密码写入进程命令行");
            }
            for field in ["Symbolic Link", "Hard Link", "Reparse", "Alternate Stream"] {
                if fields.get(field).is_some_and(|s| !s.is_empty() && s != "-") {
                    bail!("拒绝带链接、reparse 或备用数据流的压缩包：{raw}");
                }
            }
            let attr = fields.get("Attributes").cloned().unwrap_or_default();
            if attr.split_whitespace().any(|s| s.starts_with('l')) {
                bail!("拒绝 Unix 符号链接条目：{raw}");
            }
            let directory = fields.get("Folder").is_some_and(|s| s == "+")
                || attr.starts_with('D')
                || attr.starts_with('d');
            let size = match fields.get("Size") {
                // 目录条目一律按 0 计入总量：部分引擎会给目录填非 0 Size，导致合入后 expanded!=total 必败。
                _ if directory => 0,
                Some(s) if !s.is_empty() => s.parse::<u64>().context("压缩包条目大小无效")?,
                _ => {
                    // 流式单文件包可能不声明展开大小：记为不完整，合入阶段跳过精确大小校验。
                    sizes_complete = false;
                    0
                }
            };
            count = count.checked_add(1).context("条目计数溢出")?;
            total = total.checked_add(size).context("解压总大小溢出")?;
            if count > cfg.max_entries {
                bail!("压缩包条目数量超过用户设置的上限");
            }
            if cfg.max_file_gib > 0 && size > cfg.max_file_gib * (1 << 30) {
                bail!("文件展开大小超过用户上限：{raw}");
            }
            if cfg.max_unpacked_gib > 0 && total > cfg.max_unpacked_gib * (1 << 30) {
                bail!("压缩包展开总量超过用户上限");
            }
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
                        bail!("压缩包元数据异常");
                    }
                    if fields.insert(key.to_string(), value.to_string()).is_some() {
                        bail!("压缩包元数据有重复字段，无法安全解析");
                    }
                }
                Ok(())
            },
            || Ok(()),
        )
        .and_then(|()| flush(&mut fields));
        listed?;
        let packed = fs::metadata(archive)?.len().max(1);
        // 用乘法比较避免整数除法截断导致边界上更宽松。
        if cfg.max_ratio > 0 && sizes_complete {
            let limit = packed.saturating_mul(cfg.max_ratio);
            if total > limit {
                bail!("压缩包展开比例超过用户设置的上限");
            }
        }
        Ok((total, sizes_complete))
    }
    /// 档案级多卷佐证（处置路径专用）。`l -slt`（不带 -ba）输出的档案头块才被
    /// 多卷标志门控：zip 的 `Volume Index` 仅在 IsMultiVol 时出现、rar 的
    /// `Multivolume`/`Volume Index` 仅在卷标志置位时出现、字节切割集（Type = Split）
    /// 带 `Volumes` 计数。条目级 `Volume Index` 不能作佐证——zip 单卷包也逐条
    /// 无条件输出（值恒为条目所在盘号 0）。档案注释以 `{`…`}` 原样逐行输出，
    /// 注释内伪造的多卷键必须忽略（crafted 档案对抗面）。探测失败即上抛：佐证
    /// 拿不到时整包走失败路径（隔离可逆），不得默认放行宽匹配。
    fn archive_is_multi_volume(&self, archive: &Path, job: &mut Job) -> Result<bool> {
        let mut command = self.command();
        command
            .args(["l", "-slt", "-sccUTF-8", "-p-", "--"])
            .arg(archive);
        let mut in_header = false;
        let mut in_comment = false;
        let mut multipart = false;
        let ctl = job.context.control.clone();
        process::run(
            &mut command,
            &ctl,
            |err, line| {
                if err {
                    return Ok(());
                }
                let line = line.trim_end();
                // 注释块整体跳过：里面的键值（含伪造的 -- / ----）都不参与判定。
                // 注释内容本身可含 } 行（crafted 对抗）：} 结束注释的同时结束档案
                // 头块——真实输出中 Comment 是头块最后一个字段，其后的键只可能是
                // 注释伪造；漏检（如带多行注释的真分卷集）只导致宽命名兄弟卷留在
                // 原地（extract_one 有用户可见日志），不可被伪造键放行处置。
                if in_comment {
                    if line == "}" {
                        in_comment = false;
                        in_header = false;
                    }
                    return Ok(());
                }
                if line == "{" {
                    in_comment = true;
                    return Ok(());
                }
                if line == "--" {
                    in_header = true;
                    return Ok(());
                }
                // `----------`（及内层档案块的 `----`）结束档案头块；条目块不参与判定。
                if line.starts_with("----") {
                    in_header = false;
                    return Ok(());
                }
                if !in_header {
                    return Ok(());
                }
                if let Some((key, value)) = line.split_once(" = ") {
                    if key == "Volume Index" {
                        multipart = true;
                    } else if key == "Volumes" {
                        multipart |= value.trim().parse::<u64>().is_ok_and(|v| v > 1);
                    } else if key == "Multivolume" {
                        multipart |= value.trim() == "+";
                    }
                }
                Ok(())
            },
            || Ok(()),
        )
        .with_context(|| "无法完成压缩包的多卷佐证探测")?;
        Ok(multipart)
    }
    fn extract_one(
        &self,
        job: &mut Job,
        archive_rel: &str,
        depth: u32,
        disposed: &std::collections::HashSet<String>,
    ) -> Result<ExtractOutcome> {
        let archive = fsutil::safe_join(&job.root, archive_rel)?;
        // Keep an open, write-denying source handle on Windows during listing/extraction.
        let source_guard = fsutil::open_stable_read(&archive)?;
        job.context.status(format!("检查压缩包：{archive_rel}"));
        let (total, sizes_complete) = self.list(&archive, job)?;
        // 分卷组（主体 + 兄弟卷）：成功时整组处置、失败时整组隔离（X-05/X-06）。
        // 不可逆处置只认引擎佐证：oldrar/splitzip 的宽命名兄弟卷（同主干 .rNN/.zNN）
        // 仅在档案级属性证实多卷时计入——单卷包旁的同主干无辜文件（如换成完整包后
        // 残留的旧 .z01）不得被一并回收或永久删除。partN/.NNN 命名本身精确，不受门
        // 约束；宽命名候选不存在时也无需付出探测开销。
        let named = volume_set(&archive, true)?;
        let multipart = named.len() > 1 && self.archive_is_multi_volume(&archive, job)?;
        if named.len() > 1 && !multipart {
            // 漏检显性化（如带多行档案注释的 PKZIP 式分卷集：7-Zip 把 zip 头块的
            // Comment 排在多卷键之前，真键会落在注释闭合之后而无法核实）：
            // 无佐证的宽命名兄弟卷留在原地（不误删优先），但必须让用户看得见、
            // 能手动处理，而不是无声残留。
            job.log(
                "解压",
                archive_rel,
                "",
                "保留",
                "发现同主干的 .rNN/.zNN 文件但未能证实分卷关系（可能因档案注释无法核实），未随包处置；若确为旧分卷残留请手动处理",
                0,
            )?;
        }
        let volumes = if multipart {
            named
        } else {
            volume_set(&archive, false)?
        };
        let reserve = job.config.reserve_gib * (1 << 30);
        let free = fs2::available_space(&job.root)?;
        // 大小元数据不完整时只校验预留空间，避免对流式格式误报容量不足。
        if sizes_complete && total.checked_add(reserve).context("容量计算溢出")? > free {
            bail!(
                "可用空间不足：本包需 {}，预留 {}，当前 {}。未写入任何解压文件",
                bytes(total),
                bytes(reserve),
                bytes(free)
            );
        }
        // 流式包没有 Size 元数据时无法按声明总量预检：仍受用户「单包展开上限」约束
        // （R-02：0 = 不限，此时只有磁盘预留与剩余空间检查兜底；用户 2026-09-18 裁决
        // 取消此前的 50 GiB 内置硬顶）。
        let stream_cap_bytes: Option<u64> = if sizes_complete || job.config.max_unpacked_gib == 0 {
            None
        } else {
            Some(
                job.config
                    .max_unpacked_gib
                    .checked_mul(1 << 30)
                    .context("容量计算溢出")?,
            )
        };
        let stage = Staging::new(&job.root)?;
        let mut command = self.command();
        command
            .args([
                "x",
                "-aou",
                "-y",
                "-bb0",
                "-bsp1",
                "-bso1",
                "-bse2",
                "-sccUTF-8",
                "-p-",
                "-mmt=2",
            ])
            .arg(format!("-o{}", fsutil::path_string(&stage.content)?))
            .arg("--")
            .arg(&archive);
        let ctl = job.context.control.clone();
        let context = job.context.clone();
        let root = job.root.clone();
        let stage_probe = stage.content.clone();
        process::run(
            &mut command,
            &ctl,
            |err, line| {
                if !err && line.contains('%') {
                    context.status(format!("正在解压 {archive_rel} · {}", line.trim()));
                }
                Ok(())
            },
            || {
                if fs2::available_space(&root)? < reserve {
                    bail!("磁盘剩余空间低于预留阈值，停止解压并保留原包");
                }
                // 无 Size 元数据的包在解压过程中累计暂存量，超过硬顶立即停止（与预留空间联动）。
                if let Some(cap) = stream_cap_bytes {
                    let mut staged = 0u64;
                    for entry in walkdir::WalkDir::new(&stage_probe)
                        .follow_links(false)
                        .min_depth(1)
                    {
                        let entry = entry?;
                        if entry.file_type().is_file() {
                            staged = staged.saturating_add(entry.metadata()?.len());
                            if staged > cap {
                                bail!("流式压缩包解压量超过上限 {}，已停止并保留原包", bytes(cap));
                            }
                        }
                    }
                }
                Ok(())
            },
        )
        .with_context(|| "解压失败（可能已损坏、加密或格式不受支持）")?;
        // P-08：不再复核源包在解包期间是否被其他程序改动；原包处置只依据解包结果。
        drop(source_guard);
        let mut complete = true;
        let exclusions = rules::build_exclusions(&job.config.exclusions)?;
        // 分卷组（主体+兄弟卷）随后将整组处置（X-05/X-06），不得充当成员的
        // 「树内已有相同内容」来源：否则成员跳过落盘、整组又被永久删除，两处皆失
        // （与 quine 自指包同型，见 find_identical_elsewhere 注释）。无法无损表示为
        // 相对路径的兄弟卷（非 UTF-8 名）跳过即可：只少一个排除项、退回旧口径，
        // 不得让整个包因此转隔离。
        let mut exclude_rels: Vec<String> = volumes
            .iter()
            .filter_map(|path| fsutil::relative_string(&job.root, path).ok())
            .collect();
        if !exclude_rels.iter().any(|rel| rel == archive_rel) {
            exclude_rels.push(archive_rel.to_string());
        }
        let mut expanded = 0u64;
        // One archive is decoded once, including solid archives. Final placement is rename, never copy.
        for entry in walkdir::WalkDir::new(&stage.content)
            .follow_links(false)
            .min_depth(1)
        {
            job.context.control.checkpoint()?;
            let entry = entry?;
            let meta = fs::symlink_metadata(entry.path())?;
            if fsutil::is_link(&meta) {
                bail!("解压结果出现链接，已停止合入");
            }
            if meta.is_dir() {
                continue;
            }
            if !meta.is_file() {
                bail!("解压结果含非普通文件");
            }
            expanded = expanded
                .checked_add(meta.len())
                .context("解压字节计数溢出")?;
            if sizes_complete && expanded > total {
                bail!("实际解压量超过压缩包声明，已停止合入");
            }
            // 合入阶段兜底：流式包解压期间的抽样检查可能漏掉峰值，这里按最终字节量强制卡住硬顶。
            if let Some(cap) = stream_cap_bytes {
                if expanded > cap {
                    bail!(
                        "流式压缩包实际解压量超过上限 {}，已停止合入并保留原包",
                        bytes(cap)
                    );
                }
            }
            // sizes_complete=false 时 list 阶段拿不到成员大小，单文件上限改在合入阶段检查。
            if !sizes_complete
                && job.config.max_file_gib > 0
                && meta.len() > job.config.max_file_gib * (1 << 30)
            {
                bail!(
                    "文件展开大小超过用户上限：{}",
                    fsutil::relative_string(&job.root, entry.path())?
                );
            }
            let relative = fsutil::relative_string(&stage.content, entry.path())?;
            let base = archive.parent().context("压缩包缺少父目录")?;
            let mut destination = base.join(fsutil::safe_relative(&relative)?);
            // 压缩包里含有与压缩包同名的成员（gzip 头会记录原始文件名，base.tgz 里就可能是 base.tgz）：
            // 绝不能覆盖仍在使用的源包。流式包的解压结果其实就是去掉一层压缩后的内容，
            // 用真实名字（base.tar）落盘并按正常冲突策略处理；其他格式改名放置。
            // 路径相等判断仅在 Windows 上忽略大小写（NTFS 不区分）；其他平台区分大小写。
            // Windows 必须用 Unicode 大小写折叠（to_lowercase），不能退回 ASCII 比较：
            // NTFS 大小写折叠是 Unicode 表驱动的，Ä/ä 这类非 ASCII 对在文件系统层视为同一路径。
            let collides_with_source = if cfg!(windows) {
                fsutil::path_string(&destination)?.to_lowercase()
                    == fsutil::path_string(&archive)?.to_lowercase()
            } else {
                destination == archive
            };
            if collides_with_source {
                match stream_stem(&archive).map(|stem| base.join(stem)) {
                    Some(candidate) => destination = candidate,
                    None => destination = fsutil::unique_target(&job.root, &destination)?,
                }
            }
            let destination_rel = fsutil::relative_string(&job.root, &destination)?;
            // 父链仍做整链校验；最终名可能是既有链接 / junction / OneDrive 在线占位，
            // 那是成员级场景（merge_extracted 改用唯一名落盘），整链校验会让含这类
            // 成员的整包解压失败。
            if let Some(split) = destination_rel.rfind('/') {
                fsutil::safe_join(&job.root, &destination_rel[..split])?;
            }
            if member_excluded(&exclusions, &destination_rel)
                || excluded_destination(&job.root, &destination, &job.config)
            {
                complete = false;
                job.summary.skipped += 1;
                job.log(
                    "解压",
                    archive_rel,
                    &destination_rel,
                    "跳过",
                    "目标命中排除/隐藏/系统文件设置；原包保留",
                    meta.len(),
                )?;
                continue;
            }
            // 分卷/保留源包会在下次分析时再次解压。若归类已把同名同内容文件搬走，
            // 在源目录旁再写一份只会制造“删除+移动”循环；树内已有相同字节则不再落盘。
            if !destination.try_exists()? {
                let incoming = fsutil::snapshot(entry.path())?;
                if let Some(equivalent) = find_identical_elsewhere(
                    job,
                    entry.path(),
                    &incoming,
                    &relative,
                    &exclude_rels,
                    disposed,
                )? {
                    job.summary.extracted += 1;
                    let shown = fsutil::relative_string(&job.root, &equivalent)?;
                    job.log(
                        "解压",
                        archive_rel,
                        &shown,
                        "成功",
                        "树内已有相同内容，未在源目录重复写入；原包按规则处理",
                        meta.len(),
                    )?;
                    continue;
                }
            }
            // 非 Windows 上点开头路径组件等价隐藏目录/文件：未开启 include_hidden 时不落盘，
            // 否则会成为扫描不可见的影子内容（engine 扫描按组件剪枝，去重/归类/清理都看不到）。
            #[cfg(not(windows))]
            if !job.config.include_hidden && has_hidden_component(&destination_rel) {
                complete = false;
                job.summary.skipped += 1;
                job.log(
                    "解压",
                    archive_rel,
                    &destination_rel,
                    "跳过",
                    "路径含点开头（隐藏）组件，未开启包含隐藏文件；原包保留",
                    meta.len(),
                )?;
                continue;
            }
            match merge_extracted(job, entry.path(), &destination, archive_rel)? {
                MergeOutcome::Merged(final_path) => {
                    job.summary.extracted += 1;
                    if normalize_new_member_attributes(&final_path, &job.config) {
                        job.log(
                            "解压",
                            archive_rel,
                            &fsutil::relative_string(&job.root, &final_path)?,
                            "成功",
                            "解压并同卷移动",
                            meta.len(),
                        )?;
                    } else {
                        // 剥离失败 = 成员带隐藏/系统属性落盘 = 扫描不可见的影子内容。
                        // 与其它影子内容同口径（207/278 行）：原包强制保留并留日志。
                        // 后续轮次：叶子成员命中 207 行隐藏门走「跳过」，稳定不动点；
                        // 若是嵌套归档则每轮重走「合入→剥离失败→保留」，稳定重复、无数据风险。
                        complete = false;
                        job.log("解压",archive_rel,&fsutil::relative_string(&job.root,&final_path)?,"保留","成员已解压，但隐藏/系统属性剥离失败（将成扫描不可见的影子文件）；原包强制保留",meta.len())?;
                    }
                    if job.config.nested_archives
                        && rules::archive_name(&final_path.to_string_lossy())
                    {
                        enqueue(job, &final_path, depth + 1)?;
                    }
                }
                MergeOutcome::BlockedByKeep => {
                    job.summary.skipped += 1;
                    // 新文件按冲突策略应替换已有文件，但冲突删除方式为「保留」，目标腾不出来：
                    // 必须与 Skip 同口径保留原包，否则原包按规则删除后，新内容随暂存目录
                    // 一起消失，新版本在磁盘上不复存在。
                    complete = false;
                    job.log("解压",archive_rel,&destination_rel,"跳过","新文件按冲突策略应替换已有文件，但冲突删除方式为「保留」，无法腾出目标；原包强制保留",meta.len())?;
                }
                MergeOutcome::KeptExisting(keep_package) => {
                    job.summary.skipped += 1;
                    // 保留判定来自实际采用的策略（含对话框一次性选择），不得用
                    // archive_override/config 重推导：一次性「跳过」不回写 override，
                    // 重推导会把它当成 Newest/Largest 的已有胜出而误删原包，
                    // 新内容随暂存丢弃后这次解压将一无所获。
                    if keep_package {
                        complete = false;
                        job.log(
                            "解压",
                            archive_rel,
                            &destination_rel,
                            "跳过",
                            "目标冲突未采用新文件；原包强制保留",
                            meta.len(),
                        )?;
                    } else {
                        job.log(
                            "解压",
                            archive_rel,
                            &destination_rel,
                            "跳过",
                            "目标冲突：已有文件按策略保留；原包按规则处理",
                            meta.len(),
                        )?;
                    }
                }
            }
        }
        if sizes_complete && expanded != total {
            bail!("解压总量与条目清单不一致，原包保留");
        }
        // Preserve empty archive directories too. Do not merge them before checking for file/dir collisions.
        for entry in walkdir::WalkDir::new(&stage.content)
            .follow_links(false)
            .min_depth(1)
        {
            let entry = entry?;
            if entry.file_type().is_dir() {
                let rel = fsutil::relative_string(&stage.content, entry.path())?;
                let dest = archive
                    .parent()
                    .context("压缩包路径缺少父目录")?
                    .join(fsutil::safe_relative(&rel)?);
                let root_rel = fsutil::relative_string(&job.root, &dest)?;
                // 父链仍整链校验；最终名可能是链接/junction（如 OneDrive 占位目录）：
                // 已存在的目录直接合入；链接到目录不会创建也不会被写入（空目录无成员；
                // 含成员时成员路径的父链校验仍会拒绝），不再让整包解压失败。
                let dir_parent_rel = match root_rel.rfind('/') {
                    Some(i) => &root_rel[..i],
                    None => "",
                };
                if !dir_parent_rel.is_empty() {
                    fsutil::safe_join(&job.root, dir_parent_rel)?;
                }
                // 与文件合入/扫描同一过滤口径：X/** 不匹配 bare X，需补 X/ 变体；
                // 祖先目录命中排除同样跳过（扫描对 X 整树剪枝，子目录不得落盘）。
                if member_excluded(&exclusions, &root_rel)
                    || excluded_destination(&job.root, &dest, &job.config)
                {
                    complete = false;
                    job.summary.skipped += 1;
                    job.log(
                        "解压",
                        archive_rel,
                        &root_rel,
                        "跳过",
                        "目标命中排除/隐藏/系统文件设置；原包保留",
                        0,
                    )?;
                    continue;
                }
                // 非 Windows 上点开头目录组件等价隐藏目录：不创建，否则目录连同其中的
                // 成员都会成为扫描不可见的影子内容（excluded_destination 对尚不存在的
                // 目标判不出"隐藏"，必须按名称判定）。
                #[cfg(not(windows))]
                if !job.config.include_hidden && has_hidden_component(&root_rel) {
                    complete = false;
                    job.summary.skipped += 1;
                    job.log(
                        "解压",
                        archive_rel,
                        &root_rel,
                        "跳过",
                        "路径含点开头（隐藏）组件，未开启包含隐藏文件；原包保留",
                        0,
                    )?;
                    continue;
                }
                if !dest.try_exists()? {
                    if let Err(error) = fs::create_dir_all(&dest) {
                        // 最终名被悬空链接等占用（try_exists 跟随链接判为不存在，但名字已被占）：
                        // 与文件成员的应急改名同口径降级为跳过，不让整包解压失败。
                        if fs::symlink_metadata(&dest).is_ok() {
                            complete = false;
                            job.summary.skipped += 1;
                            job.log(
                                "解压",
                                archive_rel,
                                &root_rel,
                                "跳过",
                                &format!(
                                    "空目录名被既有文件/链接占用且无法创建：{error}；原包保留"
                                ),
                                0,
                            )?;
                            continue;
                        }
                        return Err(error)
                            .with_context(|| format!("无法创建目录 {}", dest.display()));
                    }
                } else if !dest.is_dir() {
                    complete = false;
                    job.summary.skipped += 1;
                    job.log(
                        "解压",
                        archive_rel,
                        &root_rel,
                        "跳过",
                        "空目录名与目标处已有文件冲突；原包保留",
                        0,
                    )?;
                }
            }
        }
        // 「解压成功」包数由调用方按 complete 口径累加：未完全解开的包要计入失败
        // 并移入「解压失败」（X-06），不得在解压层无条件先记一次成功。
        Ok(ExtractOutcome { complete, volumes })
    }
}
/// 单个压缩包的解压结果：complete=全部成员都已按策略落盘（原包可按处置策略处理）；
/// volumes=本分卷组的全部文件（主体 + 兄弟卷），非分卷时只含主体自身。
pub(crate) struct ExtractOutcome {
    pub complete: bool,
    pub volumes: Vec<PathBuf>,
}
/// 流式压缩包去掉**一层**压缩后缀后的名字；不是流式格式时返回 None。
fn stream_stem(archive: &Path) -> Option<String> {
    let name = archive.file_name()?.to_str()?.to_string();
    let lower = name.to_ascii_lowercase();
    for suffix in [
        ".tgz", ".tbz2", ".tbz", ".txz", ".bz2", ".gz", ".xz", ".lzma", ".zst",
    ] {
        if lower.ends_with(suffix) {
            let stem = &name[..name.len() - suffix.len()];
            if stem.is_empty() {
                return None;
            }
            // tgz/tbz/txz 本质是 tar 容器，名字里补回 .tar
            if matches!(suffix, ".tgz" | ".tbz" | ".tbz2" | ".txz")
                && !stem.to_ascii_lowercase().ends_with(".tar")
            {
                return Some(format!("{stem}.tar"));
            }
            return Some(stem.to_string());
        }
    }
    None
}
/// 流式压缩包（bzip2/xz/gzip）成员名：7-Zip 有时不输出 Path，用包名去掉**一层**压缩后缀合成。
fn stream_member_name(archive: &Path) -> String {
    if let Some(stem) = stream_stem(archive) {
        return stem;
    }
    let name = archive
        .file_name()
        .map_or_else(|| "content".into(), |s| s.to_string_lossy().into_owned());
    match archive.file_stem() {
        Some(stem) => stem.to_string_lossy().into_owned(),
        None => name,
    }
}

/// 成员排除判定与扫描剪枝口径对齐：扫描对目录 X 整树剪枝，因此裸目录名 `X`
/// 也必须排除其下成员 `X/y.txt`，否则成员落盘成为扫描不可见的影子，且原包每轮
/// 因目录 X 命中排除而强制保留、重复解压永不收敛。
fn member_excluded(exclusions: &globset::GlobSet, rel: &str) -> bool {
    let bytes = rel.as_bytes();
    for i in 0..=bytes.len() {
        if i == bytes.len() || bytes[i] == b'/' {
            let prefix = &rel[..i];
            if exclusions.is_match(prefix) || exclusions.is_match(format!("{prefix}/")) {
                return true;
            }
        }
    }
    false
}

/// 非 Windows 上点开头路径组件等价隐藏目录/文件（engine 扫描按组件剪枝）：
/// 未开启 include_hidden 时这样的解压目标不得落盘，否则会成为扫描不可见的影子内容。
#[cfg(not(windows))]
fn has_hidden_component(rel: &str) -> bool {
    rel.split('/').any(|part| part.starts_with('.'))
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
fn normalize_new_member_attributes(path: &Path, config: &crate::config::Config) -> bool {
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
fn normalize_new_member_attributes(_path: &Path, _config: &crate::config::Config) -> bool {
    true
}

/// 只判断根目录以下的层级。用户选定的根目录本身（例如位于隐藏的 AppData 之下）不参与隐藏/系统判定，
/// 否则整棵树都会被上级目录的属性判定为隐藏，所有解压结果都会被跳过。
fn excluded_destination(root: &Path, path: &Path, config: &crate::config::Config) -> bool {
    let mut current = Some(path);
    while let Some(candidate) = current {
        if candidate == root {
            break;
        }
        if !config.include_hidden && is_hidden(candidate) {
            return true;
        }
        if !config.include_system && is_system(candidate) {
            return true;
        }
        current = candidate.parent();
    }
    false
}
#[cfg(windows)]
fn is_hidden(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    fs::symlink_metadata(path).is_ok_and(|meta| meta.file_attributes() & 2 != 0)
}
#[cfg(not(windows))]
fn is_hidden(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
        && path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with('.'))
}
#[cfg(windows)]
fn is_system(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    fs::symlink_metadata(path).is_ok_and(|meta| meta.file_attributes() & 4 != 0)
}
#[cfg(not(windows))]
fn is_system(_path: &Path) -> bool {
    false
}
/// 将小写文件名解析为 RAR 新式分卷主干（去掉末尾 `.partN.rar` 后的部分）。
/// 与 rules::multipart_name 使用的 `\.part(\d+)\.rar$` 对齐：
/// report.partial.rar 这类仅含 “.part” 子串的普通包不得当作分卷。
fn rar_part_stem(name: &str) -> Option<&str> {
    let base = name.strip_suffix(".rar")?;
    let (stem, part) = base.rsplit_once(".part")?;
    (!part.is_empty() && part.chars().all(|c| c.is_ascii_digit())).then_some(stem)
}
/// 分卷组解析：返回主体自身 + 同目录下的兄弟卷（X-05/X-06 的处置单位）。
/// 非分卷包返回只含主体自身的单项集合。识别口径与旧 protect_volumes 一致：
/// 分卷识别只走 rar_part_stem，不用 starts_with 宽匹配（report.partial.rar 不是分卷）。
/// sweep_fuzzy 控制 oldrar/splitzip 的宽命名兄弟卷（同主干 .rNN/.zNN）：处置不可逆
/// （可配永久删除），只有引擎档案级属性证实多卷（archive_is_multi_volume）才计入；
/// .partN.rar 与 .NNN 编号命名本身精确，不受此门约束。
fn volume_set(archive: &Path, sweep_fuzzy: bool) -> Result<Vec<PathBuf>> {
    let name = archive
        .file_name()
        .and_then(|s| s.to_str())
        .context("无效压缩包名称")?
        .to_lowercase();
    let stem = if let Some(stem) = rar_part_stem(&name) {
        Some((stem.to_string(), "rar"))
    } else if name.ends_with(".7z.001") || name.ends_with(".zip.001") {
        Some((name[..name.len() - 4].to_string(), "numbered"))
    } else if let Some(stem) = name.strip_suffix(".rar") {
        Some((stem.to_string(), "oldrar"))
    } else {
        name.strip_suffix(".zip")
            .map(|stem| (stem.to_string(), "splitzip"))
    };
    let mut volumes = vec![archive.to_path_buf()];
    let Some((stem, kind)) = stem else {
        return Ok(volumes);
    };
    for entry in fs::read_dir(archive.parent().context("压缩包缺少目录")?)? {
        let entry = entry?;
        let candidate = entry.file_name().to_string_lossy().to_lowercase();
        let matches = match kind {
            // 与主体识别同口径：只认同主干的 .partN.rar，不用 starts_with 宽匹配。
            "rar" => rar_part_stem(&candidate).is_some_and(|s| s == stem),
            // 7-Zip 多卷可到 .1000+：按最少位数匹配（001/1000 都算兄弟卷），不写死恰好 3 位。
            "numbered" => candidate
                .strip_prefix(&format!("{stem}."))
                .is_some_and(|v| v.len() >= 3 && v.chars().all(|c| c.is_ascii_digit())),
            // 宽命名兄弟卷受 sweep_fuzzy 门禁（见函数注释）；门禁关闭时落到 _ => false。
            "oldrar" if sweep_fuzzy => candidate
                .strip_prefix(&format!("{stem}.r"))
                .is_some_and(|v| v.len() >= 2 && v.chars().all(|c| c.is_ascii_digit())),
            "splitzip" if sweep_fuzzy => candidate
                .strip_prefix(&format!("{stem}.z"))
                .is_some_and(|v| v.len() >= 2 && v.chars().all(|c| c.is_ascii_digit())),
            _ => false,
        };
        // 主体自身已在集合里（如 part1.rar 对主干同判）；大小写不敏感路径上可能重复命名，去重交给文件系统唯一性。
        if matches && entry.path() != archive {
            volumes.push(entry.path());
        }
    }
    Ok(volumes)
}
/// 成功整组处置（X-05）：complete 的分卷组每个文件按原包处置策略处理，默认永久删除。
/// 兄弟卷从未单独入队，必须在这里一并处理，否则跑完后目录里仍残留压缩包。
fn dispose_archive(job: &mut Job, volumes: &[PathBuf]) -> Result<()> {
    let mode = job.config.archive_delete.resolve();
    for path in volumes {
        if !path.try_exists()? {
            continue;
        }
        let snapshot = fsutil::snapshot(path)?;
        job.delete_path(path, Some(&snapshot), mode, "解压成功后的原压缩包", true)?;
    }
    Ok(())
}
/// 失败处置（X-06）：把未能完全解开的原包（连同兄弟卷）移入所选目录根下的
/// 「解压失败」子目录。移动用不覆盖改名；同名冲突改用唯一名。失败原因是界面可查的
/// 日志字段。移动不走删除接口——隔离不是删除，原包保持可用等待人工处理。
fn quarantine(job: &mut Job, archive_rel: &str, reason: &str) -> Result<()> {
    let archive = fsutil::safe_join(&job.root, archive_rel)?;
    // 隔离可逆（改名进「解压失败」，用户可移回）：宽命名兄弟卷保持整组隔离。
    // 真 PKZIP/旧 RAR 分卷集失败时常无法从主体取得 Volume Index 佐证，若在此也
    // 设门会把真兄弟卷残留在原目录（.zNN/.rNN 不在扫描口径内，永远不会再被处理）；
    // 误隔离可还原、有日志，误处置不可逆——佐证门只设在处置路径。
    let sources = volume_set(&archive, true)?;
    let dir = job.root.join(QUARANTINE_DIR_NAME);
    if !dir.try_exists()? {
        fs::create_dir_all(&dir)?;
    }
    anyhow::ensure!(
        dir.is_dir(),
        "无法建立「{QUARANTINE_DIR_NAME}」子目录：{}（目标位置被同名文件占用）",
        dir.display()
    );
    for source in sources {
        if !source.try_exists()? {
            continue;
        }
        let source_rel = fsutil::relative_string(&job.root, &source)?;
        let size = fsutil::snapshot(&source).map_or(0, |s| s.size);
        let mut target = dir.join(source.file_name().unwrap_or_default());
        if target.try_exists()? || fs::symlink_metadata(&target).is_ok() {
            target = fsutil::unique_target(&job.root, &target)?;
        }
        fsutil::rename_noreplace(&source, &target)?;
        let target_rel = fsutil::relative_string(&job.root, &target)?;
        job.summary.archives_quarantined += 1;
        // 移走后源路径不再参与后续按名/按大小查找（与 delete_path 的簿记口径一致）。
        job.db
            .conn
            .execute("UPDATE files SET active=0 WHERE rel=?1", [&source_rel])?;
        job.log(
            "解压",
            &source_rel,
            &target_rel,
            "移入解压失败",
            reason,
            size,
        )?;
    }
    Ok(())
}
/// 在已扫描文件中查找与暂存条目字节相同的副本（按文件名+大小预筛，再逐字节确认）。
/// `exclude_rels` 是本次不得充当内容来源的相对路径：正在解压的原包连同将被整组处置的
/// 分卷兄弟卷；`disposed` 是整个任务将被处置的相对路径全集（全部排队压缩包及其分卷
/// 组，见 extract_queued）。quine 型自指包（成员字节=整包字节，rsc 式 gzip/zip quine）
/// 的成员名+大小与原包行完全一致，分卷成员也可能与兄弟卷同名同字节，跨包成员还可能
/// 撞上稍后才处置的另一包——若不排除，成员会被判「树内已有相同内容」而跳过落盘，
/// 随后来源按规则删除——内容两处皆失。
///
/// 已登记残余边界（审查第 1/2 轮，两轮独立确认为预存在窄面、登记备查）：成员命中
/// 普通文件来源后，本任务内另一包的成员若按 X-04 冲突策略胜出并置换删除该来源路径，
/// 已跳过落盘的成员仍会两处皆失。封堵需要「冲突置换时动态登记 disposed」或调度级
/// 设计，超出本函数职责；在合同层面裁决前接受现状。
fn find_identical_elsewhere(
    job: &Job,
    source: &Path,
    incoming: &crate::model::Snapshot,
    member_rel: &str,
    exclude_rels: &[String],
    disposed: &std::collections::HashSet<String>,
) -> Result<Option<PathBuf>> {
    let name = Path::new(member_rel)
        .file_name()
        .and_then(|s| s.to_str())
        .context("无效压缩包成员名")?
        .to_lowercase();
    let size = i64::try_from(incoming.size).context("成员大小超出范围")?;
    let candidates: Vec<String> = {
        // 候选排除放在循环内而非 SQL：最坏情形（排除项占满 LIMIT 32 个候选槽）的
        // 后果只是成员正常落盘（放弃一次去重捷径），没有丢失路径。
        let mut statement = job.db.conn.prepare(
            "SELECT rel FROM files WHERE active=1 AND name=?1 AND size=?2 ORDER BY id LIMIT 32",
        )?;
        let rows = statement.query_map(params![name, size], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let source_rel = fsutil::relative_string(&job.root, source)?;
    for rel in candidates {
        if exclude_rels.iter().any(|excluded| excluded == &rel) || disposed.contains(&rel) {
            continue;
        }
        if rel == source_rel {
            continue;
        }
        let path = fsutil::safe_join(&job.root, &rel)?;
        // 候选可能已经在本次任务里被删除或移动（例如同名的原压缩包刚被回收），
        // 这种情况直接跳过：磁盘状态才是准的，不能让整个压缩包因此解压失败。
        let Ok(existing) = fsutil::snapshot(&path) else {
            continue;
        };
        if existing.size == incoming.size {
            // 候选此刻无法稳定读取（如正被其他进程以写方式占用，stable read 的
            // 共享模式冲突）与 snapshot 失败同口径：当作「不等价」跳过该候选，
            // 让成员正常落盘；不得把健康的整包因此打入「解压失败」。
            let Ok(equivalent) =
                hashing::equal_bytes(source, incoming, &path, &existing, &job.context.control)
            else {
                continue;
            };
            if equivalent {
                return Ok(Some(path));
            }
        }
    }
    Ok(None)
}
/// 单个冲突条目的合入结果。
enum MergeOutcome {
    /// 新文件已落位（含 TOCTOU 竞争后改用唯一名的情况）。
    Merged(PathBuf),
    /// 按策略保留已有文件（Skip，或 Newest/Largest 判定已有内容更优）：
    /// 目标内容已就位，新内容可随暂存丢弃。布尔值=是否强制保留原包：
    /// 「跳过」（含对话框一次性选择）放弃新内容后原包是唯一数据来源，必须保留；
    /// Newest/Largest 下已有内容胜出时目标已是更优内容，原包可按规则处理。
    /// 判定必须由 merge_extracted 按实际采用的策略（含 Ask 的回答）给出，
    /// 调用方无法从 archive_override/config 重推导出一次性选择。
    KeptExisting(bool),
    /// 新文件按冲突策略应当胜出，但冲突删除方式解析为「保留」，目标腾不出来：
    /// 新内容尚未落位，调用方必须保留原包，否则新版本内容会随暂存目录一起消失。
    BlockedByKeep,
}
fn merge_extracted(
    job: &mut Job,
    source: &Path,
    target: &Path,
    archive_rel: &str,
) -> Result<MergeOutcome> {
    let incoming = fsutil::snapshot(source)
        .with_context(|| format!("读取暂存解压结果失败：{}", source.display()))?;
    // 只校验/创建父目录链（ensure_dir 创建目录链本身，ensure_parent 只会创建到祖父目录，
    // 带子目录的成员会因此以「系统找不到指定的路径」整包失败）；
    // 最终名被符号链接 / junction / OneDrive 在线占位占用是成员级场景
    //（下方改用唯一名落盘），对最终名也做整链校验会让含这类成员的整包失败。
    let parent = target.parent().context("目标缺少父目录")?;
    if parent != job.root {
        fsutil::ensure_dir(&job.root, parent)?;
    }
    if !target.try_exists()? {
        match fsutil::rename_noreplace(source, target) {
            Ok(()) => return Ok(MergeOutcome::Merged(target.to_path_buf())),
            Err(error) => {
                // TOCTOU：目标在 try_exists 与 rename 之间出现。与冲突删除路径一致，
                // 改用唯一名落盘，而不是整包失败。
                let emergency = fsutil::unique_target(&job.root, target)?;
                fsutil::rename_noreplace(source, &emergency).map_err(|_| error)?;
                return Ok(MergeOutcome::Merged(emergency));
            }
        }
    }
    let meta = fs::symlink_metadata(target)
        .with_context(|| format!("读取目标状态失败：{}", target.display()))?;
    if !meta.is_file() || fsutil::is_link(&meta) {
        let renamed = fsutil::unique_target(&job.root, target)?;
        fsutil::rename_noreplace(source, &renamed)?;
        return Ok(MergeOutcome::Merged(renamed));
    }
    let existing = fsutil::snapshot(target)
        .with_context(|| format!("读取已有目标失败：{}", target.display()))?;
    // Identical bytes are already at the destination. Treat as success so the source archive
    // can be deleted; otherwise Largest/Newest/Skip on equal size keep the archive forever,
    // and the next run re-extracts after classification moved the file away.
    if incoming.size == existing.size
        && hashing::equal_bytes(source, &incoming, target, &existing, &job.context.control)?
    {
        return Ok(MergeOutcome::Merged(target.to_path_buf()));
    }
    let policy = job.archive_override.unwrap_or(job.config.extract_conflict);
    let policy = if policy == ConflictPolicy::Ask {
        let answer = (job.context.decisions)(ConflictInfo {
            existing: crate::platform::display_path_text(&target.display().to_string()),
            incoming_size: incoming.size,
            existing_size: existing.size,
            incoming_time: incoming.modified_ns,
            existing_time: existing.modified_ns,
        })?;
        if answer.apply_all {
            job.archive_override = Some(answer.policy);
        }
        answer.policy
    } else {
        policy
    };
    job.context.control.checkpoint()?;
    let use_new = match policy {
        ConflictPolicy::Overwrite => true,
        // X-04：默认保留 mtime 最新；无法判定哪个最新（mtime 相同）时保留体积最大者。
        // mtime 与体积全平局时维持已有文件，避免无谓替换。
        ConflictPolicy::Newest => {
            incoming.modified_ns > existing.modified_ns
                || (incoming.modified_ns == existing.modified_ns && incoming.size > existing.size)
        }
        ConflictPolicy::Largest => incoming.size > existing.size,
        ConflictPolicy::Skip => false,
        ConflictPolicy::KeepBoth | ConflictPolicy::Ask => {
            let renamed = fsutil::unique_target(&job.root, target)?;
            fsutil::rename_noreplace(source, &renamed)?;
            return Ok(MergeOutcome::Merged(renamed));
        }
    };
    // policy 此处已是实际采用的策略（Ask 时为对话框回答）：一次性「跳过」不回写
    // archive_override，只有这里能判定并把它传给调用方。
    if !use_new {
        return Ok(MergeOutcome::KeptExisting(policy == ConflictPolicy::Skip));
    }
    let mode = job.config.conflict_delete.resolve(job.config.global_delete);
    if mode == DeleteMode::Keep {
        // 策略要求新文件胜出，但删除方式为「保留」，已有文件腾不出来：新内容不能
        // 无声丢弃。返回 BlockedByKeep 让调用方保留原包；若当作「已有文件胜出」处理，
        // 原包会按规则删除、新内容随暂存目录消失，且与「按策略保留」的日志矛盾。
        return Ok(MergeOutcome::BlockedByKeep);
    }
    // 冲突策略决定删除已有文件时写带策略名的明确原因，便于在日志/审计中看出
    // 是策略自动处理（默认 Newest 也会在用户未显式选择时替换旧文件）。
    let reason = match policy {
        ConflictPolicy::Overwrite => "解压冲突策略（覆盖）：已有文件将被解压结果替换".to_string(),
        ConflictPolicy::Newest => {
            if incoming.modified_ns == existing.modified_ns {
                format!(
                    "解压冲突策略（较新）：修改时间相同，保留体积较大者（旧 {} 字节 / 新 {} 字节）",
                    existing.size, incoming.size
                )
            } else {
                format!(
                    "解压冲突策略（较新）：已有文件较旧，将被替换（旧 {} 字节 / 新 {} 字节）",
                    existing.size, incoming.size
                )
            }
        }
        ConflictPolicy::Largest => format!(
            "解压冲突策略（较大）：已有文件较小，将被替换（旧 {} 字节 / 新 {} 字节）",
            existing.size, incoming.size
        ),
        _ => "解压覆盖旧文件".to_string(),
    };
    let removed = job.delete_path(target, Some(&existing), mode, &reason, true)?;
    if removed == DeleteResult::Kept {
        return Ok(MergeOutcome::BlockedByKeep);
    }
    if let Err(error) = fsutil::rename_noreplace(source, target) {
        // The extracted source remains in an owned staging directory until this function returns.
        // Preserve it under a separate visible name rather than losing it when cleanup runs.
        let emergency = fsutil::unique_target(&job.root, target)?;
        fsutil::rename_noreplace(source, &emergency)
            .context("目标删除后合入失败；原压缩包仍保留")?;
        // 日志用压缩包 rel 而不是暂存路径：暂存目录随即删除，审计时必须能归属到压缩包。
        job.log(
            "解压",
            archive_rel,
            &fsutil::relative_string(&job.root, &emergency)
                .unwrap_or_else(|_| emergency.display().to_string()),
            "警告",
            &format!("目标发生竞争，改用不冲突名称：{error}"),
            incoming.size,
        )?;
        return Ok(MergeOutcome::Merged(emergency));
    }
    Ok(MergeOutcome::Merged(target.to_path_buf()))
}
pub fn enqueue(job: &Job, archive: &Path, depth: u32) -> Result<()> {
    let snapshot = fsutil::snapshot(archive)?;
    let relative = fsutil::relative_string(&job.root, archive)?;
    let fingerprint = format!(
        "{}:{}:{}:{}",
        relative, snapshot.size, snapshot.modified_ns, snapshot.identity
    );
    // 同一路径的包可能已被扫描按旧内容入队、随后被其他包的成员覆盖：清掉未处理的旧行再入队。
    // 唯一键在 fingerprint 上，INSERT OR IGNORE 挡不住同 rel 的新内容；旧行残留会在其
    // 删除该包后让新行 snapshot 失败，把「解压失败 N 包」与错误计数虚高。
    job.db.conn.execute(
        "DELETE FROM archives WHERE rel=?1 AND state='pending'",
        params![relative],
    )?;
    job.db.conn.execute(
        "INSERT OR IGNORE INTO archives(rel,fingerprint,depth) VALUES(?1,?2,?3)",
        params![relative, fingerprint, depth],
    )?;
    Ok(())
}
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "extract_batch", skip_all)
)]
pub fn extract_queued(job: &mut Job, engine: &SevenZip) -> Result<()> {
    // 崩溃/强杀后 Drop 不会执行，.jchtools-work 下可能残留孤儿暂存目录；
    // 解压开始前清理超过 24 小时的残留（阈值远大于正常解压时长，避免误伤并发任务）。
    if let Ok(removed) = clean_orphan_staging(&job.root, Duration::from_hours(24)) {
        if removed > 0 {
            job.context
                .status(format!("已清理 {removed} 个残留解压暂存目录"));
        }
    }
    // 本次任务将被处置的相对路径全集（排队压缩包 + 各分卷组可能随处置的兄弟卷）：
    // 成员解压的「树内已有相同内容」快捷路径不得信任它们——否则成员跳过落盘后
    // 来源又被整组永久删除，两处皆失（A-3，含跨包形态）。宽命名兄弟卷宽松计入：
    // 多排除只会让成员正常落盘（不走去重捷径），不会丢数据。
    let mut disposed: std::collections::HashSet<String> = job
        .db
        .conn
        .prepare("SELECT rel FROM archives")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    let queued: Vec<String> = disposed.iter().cloned().collect();
    for rel in &queued {
        let Ok(path) = fsutil::safe_join(&job.root, rel) else {
            continue;
        };
        let Ok(volumes) = volume_set(&path, true) else {
            continue;
        };
        for volume in volumes {
            if let Ok(volume_rel) = fsutil::relative_string(&job.root, &volume) {
                disposed.insert(volume_rel);
            }
        }
    }
    loop {
        job.context.control.checkpoint()?;
        let next: Option<(i64, String, u32, String)> = job
            .db
            .conn
            .query_row(
                "SELECT id,rel,depth,fingerprint FROM archives WHERE state='pending' ORDER BY depth,id LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((id, relative, depth, stored_fingerprint)) = next else {
            break;
        };
        // 指纹复核（A8-1）：排队期间该路径可能已被其他包的成员按冲突策略置换——
        // 旧包被删除、新成员顶替同名路径，而 enqueue 清理旧行的兜底只在嵌套开启
        // 且成员名匹配压缩包后缀时才生效。与 enqueue 同公式重算指纹，不一致即
        // 「原包已被取代」，清掉残留 pending 行并跳过：不得把刚解出的合法文件误当
        // 失败包移入「解压失败」，也不得虚报失败计数。比对对象是本任务自己的扫描
        // 记录与自身成员的处置结果，不属 P-08 禁止的对外部并发改动的防御。
        let Ok(path) = fsutil::safe_join(&job.root, &relative) else {
            job.db
                .conn
                .execute("DELETE FROM archives WHERE id=?1", [id])?;
            continue;
        };
        // 路径已不存在或此刻无法读取：原包已被删除/取代，残留行一并清除。
        let superseded = if let Ok(snapshot) = fsutil::snapshot(&path) {
            let fingerprint = format!(
                "{}:{}:{}:{}",
                relative, snapshot.size, snapshot.modified_ns, snapshot.identity
            );
            fingerprint != stored_fingerprint
        } else {
            true
        };
        if superseded {
            job.db
                .conn
                .execute("DELETE FROM archives WHERE id=?1", [id])?;
            job.summary.skipped += 1;
            job.log(
                "解压",
                &relative,
                "",
                "跳过",
                "扫描入队后无法确认原包仍在原位（可能已被其他成员的冲突处置取代，或被其他程序占用），本次不再处理",
                0,
            )?;
            continue;
        }
        job.db
            .conn
            .execute("UPDATE archives SET state='running' WHERE id=?1", [id])?;
        let result = if depth >= job.config.max_depth {
            Err(anyhow::anyhow!("达到最大嵌套层数（X-08 防护上限）"))
        } else {
            engine.extract_one(job, &relative, depth, &disposed)
        };
        match result {
            Ok(outcome) => {
                job.db
                    .conn
                    .execute("UPDATE archives SET state='done' WHERE id=?1", [id])?;
                // X-05/X-06：complete 整组按处置策略处理（默认永久删除）；未完全解开的
                // 整组移入「解压失败」，目录里不残留压缩包（X 分区总体约束）。
                if outcome.complete {
                    // X-05：只有完全解开的包才计「解压成功」；未完全解开的包走下面的
                    // 失败分支，两个包计数按 X-05/X-06 的划分互斥。
                    job.summary.archives_ok += 1;
                    // X-02/X-06：单包故障隔离——处置失败（处置接口异常、共享冲突等）只记
                    // 警告并保留原包原地，不得中止整个任务；重跑按等字节合入幂等收敛。
                    if let Err(error) = dispose_archive(job, &outcome.volumes) {
                        job.summary.errors += 1;
                        job.log(
                            "解压",
                            &relative,
                            "",
                            "警告",
                            &format!("解压成功但原包处置失败，原包保留在原地：{error:#}"),
                            0,
                        )?;
                    }
                } else {
                    job.summary.archives_failed += 1;
                    job.log(
                        "解压",
                        &relative,
                        "",
                        "未完全解开",
                        "有成员被跳过或未采用；原包移入「解压失败」等待人工处理",
                        0,
                    )?;
                    quarantine(
                        job,
                        &relative,
                        "未能完全解开：有成员被跳过、被排除规则命中或属于分卷来源不确定",
                    )?;
                }
            }
            Err(error) => {
                // 归档行先标 failed，避免永久停在 running。
                // 用户主动取消：上抛取消错误，不隔离、不累加失败计数、不写失败日志
                // （与 apply_with 的取消口径一致）。单出口，避免双写 failed/误计失败。
                job.db
                    .conn
                    .execute("UPDATE archives SET state='failed' WHERE id=?1", [id])?;
                if job.context.control.is_cancelled() {
                    job.context.control.check_cancelled()?;
                }
                job.summary.archives_failed += 1;
                job.summary.errors += 1;
                job.log("解压", &relative, "", "失败", &format!("{error:#}"), 0)?;
                // X-06：解压出错（损坏/加密/不支持/触上限）的原包移入「解压失败」。
                // 隔离自身失败时如实上抛，不静默丢弃原包位置信息。
                quarantine(job, &relative, &format!("解压失败：{error:#}"))?;
                job.context.control.check_cancelled()?;
            }
        }
        // U-03：解压页的实时计数按「已处理的包」统计（成败都算处理过）；与目录整理的
        // 计划项计数互不影响（两工具各自持有独立 Control）。
        job.context
            .control
            .completed
            .fetch_add(1, Ordering::Relaxed);
    }
    Ok(())
}
struct Staging {
    directory: PathBuf,
    content: PathBuf,
    owner: String,
}
impl Staging {
    fn new(root: &Path) -> Result<Self> {
        let owner = uuid::Uuid::new_v4().to_string();
        let relative = format!(".jchtools-work/{owner}");
        let directory = fsutil::safe_join(root, &relative)?;
        fs::create_dir_all(&directory)?;
        let marker = directory.join("OWNER");
        fs::write(&marker, &owner)?;
        let content = directory.join("content");
        fs::create_dir(&content)?;
        Ok(Self {
            directory,
            content,
            owner,
        })
    }
}
impl Drop for Staging {
    fn drop(&mut self) {
        if fs::read_to_string(self.directory.join("OWNER"))
            .ok()
            .as_deref()
            == Some(self.owner.as_str())
        {
            // Only this freshly generated staging tree, never a user's original directory.
            let _ = fs::remove_dir_all(&self.directory);
            if let Some(parent) = self.directory.parent() {
                let _ = fs::remove_dir(parent);
            }
        }
    }
}
/// 清理崩溃/断电后残留的孤儿暂存目录（`<root>/.jchtools-work/<uuid>`）。
/// 只删除带 OWNER 标记且内容与目录名一致的条目（Staging::new 写入的归属标记），
/// 并且目录年龄超过 max_age 才处理——阈值须远大于正常解压时长，避免误删并发任务
/// 正在使用的暂存区。返回清理数量；错误一律跳过单个条目，不影响主流程。
pub fn clean_orphan_staging(root: &Path, max_age: Duration) -> Result<usize> {
    let Ok(work) = fsutil::safe_join(root, ".jchtools-work") else {
        return Ok(0);
    };
    let Ok(entries) = fs::read_dir(&work) else {
        return Ok(0);
    };
    let now = SystemTime::now();
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // OWNER 内容必须与目录名一致：这是 Staging::new 写入的归属标记。
        let owner = fs::read_to_string(path.join("OWNER")).unwrap_or_default();
        if owner != entry.file_name().to_string_lossy() {
            continue;
        }
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let Ok(age) = now.duration_since(modified) else {
            continue;
        };
        if age < max_age {
            continue;
        }
        if fs::remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        let _ = fs::remove_dir(&work);
    } // 仅当父目录已空时才会成功
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, DeleteChoice, DeleteMode};
    use crate::control::Context as TaskContext;
    use crate::db::Database;
    use crate::engine::Job;

    // 覆盖 X-03（成员就地解到包所在位置，含缺失父目录）
    #[test]
    fn merge_creates_missing_parent_directories() {
        // 回归：合入成员时把「父目录」传给只创建父级的 ensure_parent，实际只创建到祖父目录，
        // 带子目录的成员改名必然报「系统找不到指定的路径」，于是含子目录的整包（RAR 的子目录成员、
        // 分卷包里的 vols/ 目录）解压失败并留下半截空目录。合入必须创建到成员的父目录。
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let db_dir = temp.path().join("state");
        fs::create_dir(&root).unwrap();
        let stage = tempfile::tempdir().unwrap();
        let source = stage.path().join("file1.txt");
        fs::write(&source, b"member payload").unwrap();
        let target = root.join("archives/sub/dir1/file1.txt");

        let mut job = Job {
            root: root.clone(),
            config: Config::default(),
            context: TaskContext::default(),
            db: Database::create(&db_dir).unwrap(),
            summary: crate::model::Summary::default(),
            archive_override: None,
        };
        let outcome = merge_extracted(&mut job, &source, &target, "archives/pack.rar").unwrap();
        assert!(
            matches!(outcome, MergeOutcome::Merged(_)),
            "成员应落到目标路径"
        );
        assert_eq!(fs::read(&target).unwrap(), b"member payload");
        assert!(!source.exists(), "合入后暂存文件应已改名离开");
    }

    // 覆盖 X-04, S-01（冲突删除方式为「保留」时不覆盖，原包保留）
    #[test]
    fn merge_with_keep_delete_mode_reports_blocked_instead_of_lost_content() {
        // 回归：冲突策略要求新文件胜出（Overwrite），但冲突删除方式解析为「保留」时，
        // 必须返回 BlockedByKeep 让调用方保留原包。此前静默返回 KeptExisting（旧 None），
        // 调用方会按「已有文件胜出」放行删除原包，新内容随暂存目录丢弃——新版本内容
        // 在磁盘上不复存在，日志却写「已有文件按策略保留」。
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let db_dir = temp.path().join("state");
        fs::create_dir(&root).unwrap();
        let target = root.join("target.txt");
        fs::write(&target, b"old content bytes").unwrap();
        let stage = tempfile::tempdir().unwrap();
        let source = stage.path().join("target.txt");
        fs::write(&source, b"brand new and longer").unwrap();

        let config = Config {
            extract_conflict: ConflictPolicy::Overwrite,
            conflict_delete: DeleteChoice::Keep,
            // 即使全局删除方式为永久删除，冲突删除方式「保留」仍必须优先。
            global_delete: DeleteMode::Permanent,
            ..Config::default()
        };
        let mut job = Job {
            root,
            config,
            context: TaskContext::default(),
            db: Database::create(&db_dir).unwrap(),
            summary: crate::model::Summary::default(),
            archive_override: Some(ConflictPolicy::Overwrite),
        };
        let outcome = merge_extracted(&mut job, &source, &target, "target.txt").unwrap();
        assert!(
            matches!(outcome, MergeOutcome::BlockedByKeep),
            "腾不出目标的合入必须报告为 BlockedByKeep"
        );
        // 新旧内容都原样保留：目标未被覆盖，新文件仍留在暂存里等待下一次机会。
        assert_eq!(fs::read(&target).unwrap(), b"old content bytes");
        assert_eq!(fs::read(&source).unwrap(), b"brand new and longer");
    }

    // 覆盖 X-04（mtime 平局回退比较体积）
    #[test]
    fn newest_policy_breaks_mtime_tie_by_larger_size() {
        // 回归（C-03）：解压冲突默认策略 Newest 此前只比较 mtime，mtime 相同时直接保留
        // 已有文件，从不比较体积；合同要求「无法判定哪个最新（mtime 相同）时保留体积
        // 最大者」。用 conflict_delete=Keep 把「新文件胜出」表达为 BlockedByKeep，
        // 断言不触发真实删除。
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let db_dir = temp.path().join("state");
        fs::create_dir(&root).unwrap();
        let target = root.join("target.txt");
        fs::write(&target, b"short old").unwrap();
        let stage = tempfile::tempdir().unwrap();
        let source = stage.path().join("target.txt");
        fs::write(&source, b"much longer new content").unwrap();
        // 平局：两个文件 mtime 完全一致（C-03 的「无法判定哪个最新」）。
        let tie = filetime::FileTime::from_unix_time(1_700_000_000, 0);
        filetime::set_file_mtime(&target, tie).unwrap();
        filetime::set_file_mtime(&source, tie).unwrap();

        let config = Config {
            extract_conflict: ConflictPolicy::Newest,
            conflict_delete: DeleteChoice::Keep,
            global_delete: DeleteMode::Permanent,
            ..Config::default()
        };
        let mut job = Job {
            root: root.clone(),
            config,
            context: TaskContext::default(),
            db: Database::create(&db_dir).unwrap(),
            summary: crate::model::Summary::default(),
            archive_override: Some(ConflictPolicy::Newest),
        };
        let outcome = merge_extracted(&mut job, &source, &target, "target.txt").unwrap();
        assert!(
            matches!(outcome, MergeOutcome::BlockedByKeep),
            "mtime 平局且新文件更大：Newest 必须选择新文件（C-03 平局回退比较体积）"
        );
        assert_eq!(
            fs::read(&target).unwrap(),
            b"short old",
            "Keep 模式下旧文件保持原样"
        );
        assert_eq!(
            fs::read(&source).unwrap(),
            b"much longer new content",
            "新内容仍在暂存目录"
        );

        // 对照一：平局且新文件更小 → 保留体积更大的已有文件（KeptExisting，不是 BlockedByKeep）。
        fs::write(&source, b"tiny").unwrap();
        filetime::set_file_mtime(&source, tie).unwrap();
        let outcome = merge_extracted(&mut job, &source, &target, "target.txt").unwrap();
        assert!(
            matches!(outcome, MergeOutcome::KeptExisting(false)),
            "mtime 平局且新文件更小：应保留体积更大的已有文件"
        );

        // 对照二：平局且等大但内容不同 → 维持已有文件（全平局时稳定不动，避免无谓替换）。
        fs::write(&source, b"samelen!!").unwrap();
        filetime::set_file_mtime(&source, tie).unwrap();
        let outcome = merge_extracted(&mut job, &source, &target, "target.txt").unwrap();
        assert!(
            matches!(outcome, MergeOutcome::KeptExisting(false)),
            "mtime 与体积全平局：应稳定保留已有文件"
        );
    }

    // 覆盖 X-03, X-05（等价内容跳过不得吞掉原包自身的唯一副本）
    #[test]
    fn find_identical_elsewhere_never_matches_current_archive() {
        // 回归：quine 型自指包（成员字节=整个包字节，rsc 式 gzip/zip quine，gzip 头还会
        // 记录与包同名的原始文件名）此前会把「正在解压的原压缩包自身」当作树内等价副本——
        // 成员被判「树内已有相同内容」跳过落盘，原包又被按规则删除，内容两处皆失。
        // 等价查找必须排除当前原包；对其它同名同字节文件的等价去重能力保持不变。
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let db_dir = temp.path().join("state");
        fs::create_dir(&root).unwrap();
        let bytes: &[u8] = b"archive-bytes-identical-to-member";
        fs::write(root.join("pack.zip"), bytes).unwrap();
        // 暂存成员在真实管线里位于 job.root 下的 .jchtools-work：夹具保持同构。
        let member = root.join(".jchtools-work/stage/pack.zip");
        fs::create_dir_all(member.parent().unwrap()).unwrap();
        fs::write(&member, bytes).unwrap();
        let job = Job {
            root: root.clone(),
            config: Config::default(),
            context: TaskContext::default(),
            db: Database::create(&db_dir).unwrap(),
            summary: crate::model::Summary::default(),
            archive_override: None,
        };
        let archive_snapshot = fsutil::snapshot(&root.join("pack.zip")).unwrap();
        job.db
            .insert_file("pack.zip", "pack.zip", "pack.zip", &archive_snapshot)
            .unwrap();
        let incoming = fsutil::snapshot(&member).unwrap();
        let exclude = ["pack.zip".to_string()];
        let no_disposed = std::collections::HashSet::<String>::new();
        assert!(
            find_identical_elsewhere(&job, &member, &incoming, "pack.zip", &exclude, &no_disposed)
                .unwrap()
                .is_none(),
            "等价查找不得把正在解压的原包自身当作树内副本"
        );
        // 对照：其它路径上的同名同字节文件仍按等价副本命中（去重快捷路径不回归）。
        fs::create_dir(root.join("other")).unwrap();
        fs::write(root.join("other/pack.zip"), bytes).unwrap();
        let other_snapshot = fsutil::snapshot(&root.join("other/pack.zip")).unwrap();
        job.db
            .insert_file("other/pack.zip", "pack.zip", "pack.zip", &other_snapshot)
            .unwrap();
        let hit =
            find_identical_elsewhere(&job, &member, &incoming, "pack.zip", &exclude, &no_disposed)
                .unwrap();
        let hit_rel = fsutil::relative_string(&root, &hit.unwrap()).unwrap();
        assert_eq!(
            hit_rel.as_str(),
            "other/pack.zip",
            "非原包的等价副本仍应命中"
        );
    }

    #[test]
    fn enqueue_replaces_pending_row_for_same_rel() {
        // 回归：archives 表唯一键在 fingerprint 上；同一路径的压缩包被另一个包的成员
        // 处理并按规则删除该包后，新行 snapshot 必然失败，把「解压失败 N 包」与错误
        // 计数虚高。入队时应先清同 rel 的未处理旧行。
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let db_dir = temp.path().join("state");
        fs::create_dir(&root).unwrap();
        let pack = root.join("pack.zip");
        fs::write(&pack, b"first version bytes").unwrap();

        let job = Job {
            root: root.clone(),
            config: Config::default(),
            context: TaskContext::default(),
            db: Database::create(&db_dir).unwrap(),
            summary: crate::model::Summary::default(),
            archive_override: None,
        };
        enqueue(&job, &pack, 0).unwrap();
        // 同路径包内容被覆盖（大小与内容都变了）后再次入队。
        fs::write(&pack, b"second version with different length").unwrap();
        enqueue(&job, &pack, 0).unwrap();
        let rows: i64 = job
            .db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM archives WHERE rel='pack.zip'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1, "同一路径只应保留最新 fingerprint 的待处理行");
    }

    // 平台门禁原因：验证对象是 Windows 路径长度语义（LongPathsEnabled=0 时 >260 普通路径的
    // 裸 Win32 调用直接失败）与 \\?\ verbatim 前缀拼接，只能在 Windows 上构造。
    // 覆盖 X-08, S-04（隐藏属性剥离，避免成员成为扫描不可见的影子文件）
    #[cfg(windows)]
    #[test]
    fn normalize_strips_hidden_on_plain_long_path() {
        // 直测锚点：集成测试经 prepare_at 的 root 已 canonicalize（verbatim），裸调用恰好总是
        // 成功，锁不住「普通路径 + 超长」这一原始缺陷形态；这里显式传非 verbatim 的 >260 路径，
        // LongPathsEnabled=0 的机器上修复前（裸调用 + 吞错记成功）必失败。
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::{SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN};
        let temp = tempfile::tempdir().unwrap();
        let mut dir = temp.path().to_path_buf();
        let seg = "n".repeat(40);
        for i in 0..7 {
            dir.push(format!("{seg}{i}"));
        } // 相对段 ≈300 字符，基路径短但总长 >260
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("deep-hidden.txt");
        fs::write(&file, b"payload").unwrap();
        // 植入隐藏属性：这条路径本身 >260，植入同样要走 \\?\（裸调用会以同样方式失败）。
        let mut wide: Vec<u16> = r"\\".encode_utf16().chain(r"?\".encode_utf16()).collect();
        wide.extend(file.as_os_str().encode_wide());
        wide.push(0);
        assert_ne!(
            // SAFETY: wide 是以 NUL 结尾的 UTF-16 verbatim 路径；调用只读取该缓冲区。
            unsafe { SetFileAttributesW(wide.as_ptr(), FILE_ATTRIBUTE_HIDDEN) },
            0,
            "测试前置：植入隐藏属性失败"
        );
        let config = Config {
            include_hidden: false,
            ..Config::default()
        };
        assert!(
            normalize_new_member_attributes(&file, &config),
            "普通超长路径的属性剥离必须成功"
        );
        let attrs = fs::symlink_metadata(&file).unwrap().file_attributes();
        assert_eq!(attrs & FILE_ATTRIBUTE_HIDDEN, 0, "隐藏属性必须被剥离");
    }
}

#[cfg(test)]
mod one_shot_skip_tests {
    use super::*;
    use crate::config::{Config, ConflictPolicy};
    use crate::control::{ConflictAnswer, Context as TaskContext};
    use crate::db::Database;
    use crate::engine::Job;
    use std::sync::Arc;

    // 覆盖 X-04, R-02（逐次询问的一次性选择不应用到全部）
    #[test]
    fn merge_one_shot_skip_keeps_original_archive() {
        // 回归：Ask 对话框一次性选择「跳过」（不应用到全部）不回写 archive_override，
        // 调用方若按 extract_conflict 重推导（Ask≠Skip）会误删原包：新内容随暂存
        // 丢弃后，这次解压一无所获。KeptExisting 必须携带保留判定。
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let db_dir = temp.path().join("state");
        fs::create_dir(&root).unwrap();
        let target = root.join("target.txt");
        fs::write(&target, b"old content bytes").unwrap();
        let stage = tempfile::tempdir().unwrap();
        let source = stage.path().join("target.txt");
        fs::write(&source, b"brand new and longer").unwrap();

        let config = Config {
            extract_conflict: ConflictPolicy::Ask,
            ..Config::default()
        };
        let context = TaskContext {
            decisions: Arc::new(|_| {
                Ok(ConflictAnswer {
                    policy: ConflictPolicy::Skip,
                    apply_all: false,
                })
            }),
            ..TaskContext::default()
        };
        let mut job = Job {
            root,
            config,
            context,
            db: Database::create(&db_dir).unwrap(),
            summary: crate::model::Summary::default(),
            archive_override: None,
        };
        let outcome = merge_extracted(&mut job, &source, &target, "target.txt").unwrap();
        assert!(
            matches!(outcome, MergeOutcome::KeptExisting(true)),
            "一次性 Skip 必须判定为保留原包"
        );
        assert_eq!(fs::read(&target).unwrap(), b"old content bytes");
        assert!(
            job.archive_override.is_none(),
            "一次性选择不得回写 override"
        );
    }

    // 覆盖 X-04, S-01, S-02（默认 Newest 判新文件胜出：被淘汰旧文件先永久删除，再放置新文件）
    #[test]
    fn newest_winner_displaces_loser_permanently() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let db_dir = temp.path().join("state");
        fs::create_dir(&root).unwrap();
        let target = root.join("target.txt");
        fs::write(&target, b"old short").unwrap();
        filetime::set_file_mtime(&target, filetime::FileTime::from_unix_time(1000, 0)).unwrap();
        let staging = tempfile::tempdir().unwrap();
        let source = staging.path().join("target.txt");
        fs::write(&source, b"brand new winner").unwrap();
        filetime::set_file_mtime(&source, filetime::FileTime::from_unix_time(2000, 0)).unwrap();

        let config = Config {
            extract_conflict: ConflictPolicy::Newest,
            ..Config::default()
        };
        let mut job = Job {
            root: root.clone(),
            config,
            context: TaskContext::default(),
            db: Database::create(&db_dir).unwrap(),
            summary: crate::model::Summary::default(),
            archive_override: Some(ConflictPolicy::Newest),
        };
        let outcome = merge_extracted(&mut job, &source, &target, "target.txt").unwrap();
        assert!(
            matches!(outcome, MergeOutcome::Merged(_)),
            "新文件更旧文件新（mtime 更大）：Newest 必须判新文件胜出"
        );
        assert_eq!(
            fs::read(&target).unwrap(),
            b"brand new winner",
            "旧文件永久删除后，目标位置应放置新文件"
        );
        assert!(!source.exists(), "暂存新文件已改名离开");
        // S-02：不再有回收站路径，被淘汰的旧文件是永久删除，记账也是永久删除口径。
        assert_eq!(job.summary.deleted, 1);
        assert_eq!(job.summary.permanent_bytes, 9, "「old short」=9 字节");
    }
}
