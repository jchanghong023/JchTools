use crate::{config::DeleteMode, engine::Job, fsutil, model::bytes, process, rules};
use anyhow::{bail, ensure, Context, Result};
use rusqlite::{params, OptionalExtension};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::Ordering,
    time::{Duration, SystemTime},
};

/// 「解压失败」子目录名（X-06）：建在所选目录根下，收纳未能完全解开的原包。
/// 扫描（两工具）与重跑计数默认排除它（X-07 / C-09）。
pub const QUARANTINE_DIR_NAME: &str = "解压失败";

/// 条目清单结果：声明总大小、大小元数据是否完整、以及 H-06 的 Git 排除子树。
/// `git_subtrees` 存归档内相对路径的目录前缀：该目录「直接含有 .git」，其自身及
/// 全部后代（含 .git 的兄弟条目）在合入阶段整树跳过；空串表示归档根本身直接含
/// .git，整个暂存根都不解出。
struct Listing {
    total: u64,
    sizes_complete: bool,
    git_subtrees: HashSet<String>,
}

/// 成员路径里出现 `.git` 组件时，返回「直接含该 .git 的那个目录」的归档相对路径
/// （空串 = 归档根）。H-06 的边界按目录项识别，不深入 Git 树内部。
fn git_boundary_prefix(rel: &str) -> Option<String> {
    let parts: Vec<&str> = rel.split('/').collect();
    let index = parts
        .iter()
        .position(|part| part.eq_ignore_ascii_case(".git"))?;
    Some(parts[..index].join("/"))
}

/// 成员（或空目录）是否落在某个「直接含 .git 的目录」子树内：H-06 要求该目录及
/// 全部后代整树排除，含 .git 的兄弟条目——按路径组件比较，`projectx` 不会命中
/// `project` 的边界。
fn inside_git_subtree(subtrees: &HashSet<String>, rel: &str) -> bool {
    if subtrees.is_empty() {
        return false;
    }
    let mut prefix = String::new();
    for part in rel.split('/') {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(part);
        if subtrees.contains(&prefix) {
            return true;
        }
    }
    false
}

/// Git 边界判定缓存（H-06）：同一目录在一次解压里被反复询问（同一包的兄弟成员、
/// 嵌套归档的父链），缓存把目录项检查从「成员数 × 深度」收敛到「不同目录数」。
/// 本工具从不创建 `.git` 条目，一次解压期间目录的 Git 状态不变，缓存安全。
#[derive(Default)]
struct GitBoundaries {
    known: HashMap<PathBuf, bool>,
}

impl GitBoundaries {
    /// 目录是否位于 Git 树内：自该目录起、直到（不含）用户选定的根目录，任一级
    /// 直接含 `.git` 即为真。用户选定的根本身不参与判定——根即 Git 根属整次任务的
    /// 前置拒绝，不在解压层处理。
    fn blocked(&mut self, root: &Path, directory: &Path) -> Result<bool> {
        if let Some(&cached) = self.known.get(directory) {
            return Ok(cached);
        }
        let mut chain: Vec<PathBuf> = Vec::new();
        let mut verdict: Option<bool> = None;
        let mut current = Some(directory.to_path_buf());
        while let Some(candidate) = current {
            if candidate.as_path() == root {
                break;
            }
            if let Some(&cached) = self.known.get(&candidate) {
                verdict = Some(cached);
                break;
            }
            current = candidate.parent().map(Path::to_path_buf);
            chain.push(candidate);
        }
        // 祖先缓存只证明祖先自身；仍须检查后代是否开启了新的 Git 边界。
        let mut blocked = verdict.unwrap_or(false);
        for candidate in chain.into_iter().rev() {
            if !blocked {
                blocked = fsutil::is_git_root(&candidate)?;
            }
            self.known.insert(candidate, blocked);
        }
        Ok(blocked)
    }
}

/// 分卷组实际体积合计（X-08 的展开比例分母）：主体与每个随组分卷各按实际文件大小
/// 相加。单卷大小会随分卷数缩小分母，把健康的分卷组误判为「展开比例超限」。
/// 某个卷读取失败即上抛：分母不能凭空变小。
fn volume_bytes(volumes: &[PathBuf]) -> Result<u64> {
    let mut total = 0u64;
    for path in volumes {
        total = total.saturating_add(fs::metadata(path)?.len());
    }
    Ok(total.max(1))
}

/// 任务级中止信号（X-05/X-06/X-08）：空间不足、以及成功原包/分卷删除失败都不是包
/// 损坏——保留当前源包（含未删除的分卷）、报错并停止整个解压任务，不隔离本包，也不
/// 继续处理后续包。错误在解压回调里会被 `with_context` 包装，判定走错误链而不是顶层
/// downcast（见 [`stops_extraction`]）。
#[derive(Debug)]
struct StopExtraction(String);

impl std::fmt::Display for StopExtraction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StopExtraction {}

/// 错误链里是否带「停止任务」信号（空间不足无法继续）：命中即整次解压中止，
/// 不做隔离处置。
fn stops_extraction(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<StopExtraction>().is_some())
}

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
    /// 列出压缩包条目：声明总大小、大小元数据是否完整、以及 H-06 的 Git 边界子树。
    /// 7-Zip 对 bzip2/xz 等流式格式可能不输出成员 Path，甚至不输出 Size；此时不能把条目静默丢掉，
    /// 否则 total=0 会在合入阶段误报「实际解压量超过压缩包声明」。
    /// 展开比例与空间预检由调用方执行（分母是实际卷集合合计，见 extract_one）。
    fn list(&self, archive: &Path, job: &mut Job) -> Result<Listing> {
        let mut command = self.command();
        command
            .args(["l", "-slt", "-ba", "-sccUTF-8", "-p-", "--"])
            .arg(archive);
        let mut fields = BTreeMap::<String, String>::new();
        let mut total = 0u64;
        let mut count = 0u64;
        let mut sizes_complete = true;
        let mut git_subtrees = HashSet::new();
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
            // 影子成员落盘不会删除原包（X-06 隔离而非 X-05 删除），但解出的内容会落在
            // 两工具都看不到的区域、用户无法管理；故在清单阶段就按危险条目拒绝并整包隔离（可逆、有日志）。
            let top_component = raw.split('/').next().unwrap_or_default();
            if (archive_at_root && top_component == QUARANTINE_DIR_NAME)
                || raw.split('/').any(|s| s.starts_with(".jchtools-link-"))
            {
                bail!("拒绝压缩包中的「解压失败」暂存区或内部链接标记条目：{raw}");
            }
            // H-06：成员路径里出现 .git 组件时，登记「直接含该 .git 的目录」为整树
            // 排除边界。边界必须在合入前从完整条目清单确定：该目录及全部后代整树
            // 排除，包括 .git 的兄弟条目——只看落盘结果会把兄弟条目先写进用户目录，
            // 而一旦 .git 落盘，该目录在后续扫描里就是永久排除区。
            if let Some(prefix) = git_boundary_prefix(&raw) {
                git_subtrees.insert(prefix);
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
        Ok(Listing {
            total,
            sizes_complete,
            git_subtrees,
        })
    }
    /// 只读取最外层档案头中的格式与实际卷数。注释和后续内层档案头不具有删除授权。
    fn archive_volume_count(&self, archive: &Path, job: &mut Job) -> Result<ArchiveVolumes> {
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
                if line.starts_with("----") || line.starts_with("Comment =") || line == "{" {
                    in_header = false;
                }
                if !in_header {
                    return Ok(());
                }
                if let Some((key, value)) = line.split_once(" = ") {
                    match key {
                        "Type" => value.clone_into(&mut kind),
                        "Volumes" => {
                            let value = value.parse::<usize>().context("引擎分卷数量无效")?;
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
    }
    fn extract_one(&self, job: &mut Job, archive_rel: &str, depth: u32) -> Result<bool> {
        let archive = fsutil::safe_join(&job.root, archive_rel)?;
        job.context.status(format!("检查压缩包：{archive_rel}"));
        // 文件名仅用于发现候选；删除与展开比例只能使用引擎实际打开的连续分卷。
        // 隔离仍使用独立的可逆宽匹配，不能把该集合复用为永久删除授权。
        let named = volume_set(&archive)?;
        let volumes = if named.paths.len() > 1
            || matches!(named.scheme, VolumeScheme::RarParts | VolumeScheme::OldRar)
        {
            let info = self.archive_volume_count(&archive, job)?;
            named.scheme.actual_paths(&archive, &info)?
        } else {
            named.paths
        };
        // X-08：展开比例的分母是实际卷集合合计，不是主体单卷大小。
        let packed = volume_bytes(&volumes)?;
        let Listing {
            total,
            sizes_complete,
            git_subtrees,
        } = self.list(&archive, job)?;
        // H-06：归档根直接含 .git 时整个暂存根都不解出——落盘会让归档所在目录（或
        // 其上级）变成 Git 树，两工具随后会整体拒绝该目录，且半个 Git 树对用户无用。
        // 先于容量/比例判定：本包根本不会落盘，不适用展开防护。
        if git_subtrees.contains("") {
            job.summary.skipped += 1;
            job.log(
                "解压",
                archive_rel,
                "",
                "跳过",
                "压缩包根目录含 .git：按 H-06 整树排除，未解出任何成员；原包保留",
                0,
            )?;
            return Ok(false);
        }
        // X-08：展开比例＝解出体积 ÷ 包体积，分卷包按实际卷集合合计。用乘法比较
        // 避免整数除法截断导致边界上更宽松。
        if job.config.max_ratio > 0 && sizes_complete {
            let limit = packed.saturating_mul(job.config.max_ratio);
            if total > limit {
                bail!("压缩包展开比例超过用户设置的上限");
            }
        }
        let reserve = job.config.reserve_gib * (1 << 30);
        let free = fs2::available_space(&job.root)
            .map_err(|error| StopExtraction(format!("无法查询磁盘可用空间：{error}")))?;
        // 大小元数据不完整时只校验预留空间，避免对流式格式误报容量不足。
        if sizes_complete && total.checked_add(reserve).context("容量计算溢出")? > free {
            // X-06/X-08：空间不足不是包损坏——保留原包、报错并停止整个解压任务，
            // 不隔离本包，也不继续批量隔离后续正常包。
            return Err(StopExtraction(format!(
                "可用空间不足：本包需 {}，预留 {}，当前 {}；已保留原包并停止本次解压，未写入任何解压文件",
                bytes(total),
                bytes(reserve),
                bytes(free)
            ))
            .into());
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
                if fs2::available_space(&root)
                    .map_err(|error| StopExtraction(format!("无法查询磁盘可用空间：{error}")))?
                    < reserve
                {
                    // X-08：预留阈值被击穿即保留原包并停止整个任务（不隔离、不继续）。
                    return Err(StopExtraction(format!(
                        "磁盘剩余空间低于预留阈值 {}，已停止解压并保留原包",
                        bytes(reserve)
                    ))
                    .into());
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
        let mut complete = true;
        let exclusions = rules::build_exclusions(&job.config.exclusions)?;
        let mut git = GitBoundaries::default();
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
            if inside_git_subtree(&git_subtrees, &relative) {
                // H-06：该成员所属目录直接含 .git——整树排除（含 .git 的兄弟条目与
                // 全部后代），不解出；原包按未完全解开处理（X-06），内容仍在原包里。
                complete = false;
                job.summary.skipped += 1;
                job.log(
                    "解压",
                    archive_rel,
                    &destination_rel,
                    "跳过",
                    "成员位于含 .git 的目录树内：H-06 整树排除，不解压；原包保留",
                    meta.len(),
                )?;
                continue;
            }
            if member_excluded(&exclusions, &destination_rel)
                || excluded_destination(&job.root, &destination, &job.config, &mut git)?
            {
                complete = false;
                job.summary.skipped += 1;
                job.log(
                    "解压",
                    archive_rel,
                    &destination_rel,
                    "跳过",
                    "目标命中排除/隐藏/系统文件或 Git 目录树设置；原包保留",
                    meta.len(),
                )?;
                continue;
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
            let (final_path, renamed) =
                match merge_extracted(&job.root, entry.path(), &destination)? {
                    MergeOutcome::Placed(path) => (path, false),
                    MergeOutcome::Renamed(path) => (path, true),
                };
            job.summary.extracted += 1;
            let final_rel = fsutil::relative_string(&job.root, &final_path)?;
            if !normalize_new_member_attributes(&final_path, &job.config) {
                // 剥离失败 = 成员带隐藏/系统属性落盘 = 扫描不可见的影子内容。
                // 与其它影子内容同口径：原包强制保留并留日志（不把用户内容留成盲区）。
                complete = false;
                job.log(
                    "解压",
                    archive_rel,
                    &final_rel,
                    "保留",
                    "成员已解压，但隐藏/系统属性剥离失败（将成扫描不可见的影子文件）；原包强制保留",
                    meta.len(),
                )?;
            } else if renamed {
                job.log(
                    "解压",
                    archive_rel,
                    &final_rel,
                    "成功",
                    "目标已存在：既有文件保持不动，新成员按 H-07 改名落盘（保留扩展名）",
                    meta.len(),
                )?;
            } else {
                job.log(
                    "解压",
                    archive_rel,
                    &final_rel,
                    "成功",
                    "解压并同卷移动",
                    meta.len(),
                )?;
            }
            // X-08：继续处理本次解出的嵌套压缩包（层数上限沿用 max_depth）；
            // H-06：落点若位于 Git 目录树内则不处理该包（该树整树排除，不解压）。
            if rules::archive_name(&final_path.to_string_lossy())
                && !git.blocked(&job.root, final_path.parent().context("成员缺少父目录")?)?
            {
                enqueue(job, &final_path, depth + 1)?;
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
            // 取消在目录操作的安全边界生效：已落盘的成员保留，后续空目录不再创建。
            job.context.control.checkpoint()?;
            let entry = entry?;
            if entry.file_type().is_dir() {
                let rel = fsutil::relative_string(&stage.content, entry.path())?;
                if inside_git_subtree(&git_subtrees, &rel) {
                    // H-06：该目录所属子树直接含 .git——整树排除，空目录也不创建。
                    complete = false;
                    job.summary.skipped += 1;
                    job.log(
                        "解压",
                        archive_rel,
                        "",
                        "跳过",
                        &format!("空目录位于含 .git 的目录树内（{rel}）：H-06 整树排除，不创建；原包保留"),
                        0,
                    )?;
                    continue;
                }
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
                    || excluded_destination(&job.root, &dest, &job.config, &mut git)?
                {
                    complete = false;
                    job.summary.skipped += 1;
                    job.log(
                        "解压",
                        archive_rel,
                        &root_rel,
                        "跳过",
                        "目标命中排除/隐藏/系统文件或 Git 目录树设置；原包保留",
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
        // X-05：只有整包解码与校验成功、全部成员（含空目录）完整落盘后，才永久删除
        // 原包及其实际分卷；失败、部分解开或取消都不启动删除，也不删除仅同主干的文件。
        if complete {
            delete_successful_source(job, archive_rel, &volumes)?;
        }
        // 「解压成功」包数由调用方按 complete 口径累加：未完全解开的包要计入失败
        // 并移入「解压失败」（X-06），不得在解压层无条件先记一次成功。
        Ok(complete)
    }
}
/// X-05：完整解开的原包与其实际分卷按永久删除处置（`volumes` 已在解压前按命名口径
/// 与档案级佐证门校验：主体 + 精确命名兄弟卷 + 已证实的宽命名兄弟卷）。
/// 复用 [`Job::delete_path`] 的既有语义（审计日志、删除计数、files 行失活），删除方式
/// 固定为永久删除，不受目录整理的全局删除方式影响。
/// 每个卷删除前检查取消：取消后不再启动后续删除（已落盘结果与未删除的卷保留）。
/// 删除失败即如实上抛为任务级中止（不是包损坏）：解压结果已落盘，未删除的卷保留在
/// 原位置，不做隔离、不虚计成功、也不回滚已删除的卷（X-05 不承诺多卷删除事务性）。
fn delete_successful_source(job: &mut Job, archive_rel: &str, volumes: &[PathBuf]) -> Result<()> {
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
/// 流式压缩包去掉**一层**压缩后缀后的名字；不是流式格式时返回 None。
/// 后缀集合与 X-01 白名单里的压缩流一致（`.tar.<流后缀>` 只去一层，剩下的 `.tar`
/// 由 tar 处理逻辑继续展开）。
fn stream_stem(archive: &Path) -> Option<String> {
    let name = archive.file_name()?.to_str()?.to_string();
    let lower = name.to_ascii_lowercase();
    for suffix in [
        ".tgz", ".tbz2", ".tbz", ".txz", ".tzst", ".bz2", ".gz", ".xz", ".lzma", ".zst", ".lz4",
        ".lz", ".z", ".br",
    ] {
        if lower.ends_with(suffix) {
            let stem = &name[..name.len() - suffix.len()];
            if stem.is_empty() {
                return None;
            }
            // tgz/tbz/tbz2/txz/tzst 本质是 tar 容器，名字里补回 .tar
            if matches!(suffix, ".tgz" | ".tbz" | ".tbz2" | ".txz" | ".tzst")
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
/// 否则整棵树都会被上级目录的属性判定为隐藏，所有解压结果都会被跳过；
/// 根即 Git 根属整次任务的前置拒绝，同理不在此判定。
/// H-06：目标（或其任一级祖先目录）直接含 `.git` 时整树排除——不解压、不写入。
fn excluded_destination(
    root: &Path,
    path: &Path,
    config: &crate::config::Config,
    git: &mut GitBoundaries,
) -> Result<bool> {
    if git.blocked(root, path)? {
        return Ok(true);
    }
    // 隐藏/系统开关都开启时无需逐级判定（默认即如此，H-06 之外不再有排除）。
    if config.include_hidden && config.include_system {
        return Ok(false);
    }
    let mut current = Some(path);
    while let Some(candidate) = current {
        if candidate == root {
            break;
        }
        if !config.include_hidden && is_hidden(candidate) {
            return Ok(true);
        }
        if !config.include_system && is_system(candidate) {
            return Ok(true);
        }
        current = candidate.parent();
    }
    Ok(false)
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
struct ArchiveVolumes {
    kind: String,
    count: usize,
    new_rar_names: bool,
}

/// 分卷命名仅描述引擎逐卷打开时使用的路径；是否多卷以及实际数量必须由引擎确认。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VolumeScheme {
    Single,
    RarParts,
    Numbered,
    OldRar,
    SplitZip,
}
impl VolumeScheme {
    fn actual_paths(self, archive: &Path, info: &ArchiveVolumes) -> Result<Vec<PathBuf>> {
        let kind = &info.kind;
        let count = info.count;
        if count == 1 {
            return Ok(vec![archive.to_path_buf()]);
        }
        let compatible = match self {
            Self::RarParts | Self::OldRar => {
                kind.eq_ignore_ascii_case("rar") || kind.eq_ignore_ascii_case("rar5")
            }
            Self::Numbered => kind.eq_ignore_ascii_case("split"),
            Self::SplitZip => kind.eq_ignore_ascii_case("zip"),
            Self::Single => false,
        };
        ensure!(compatible, "引擎格式与分卷命名不一致，保留源包");
        let name = archive
            .file_name()
            .and_then(|name| name.to_str())
            .context("无效分卷名称")?;
        // RAR4 由 NewVolName 标志决定；RAR5 总用新式规则。新式取扩展名前最后一段数字，
        // 不要求叫 part1：report1.rar → report2.rar，数字后的文字与前导零也保持不变。
        let base = &name[..name.len() - 4];
        let mut rar_number = if info.new_rar_names || kind.eq_ignore_ascii_case("rar5") {
            base.as_bytes()
                .iter()
                .rposition(u8::is_ascii_digit)
                .map(|last| {
                    let end = last + 1;
                    let start = base.as_bytes()[..end]
                        .iter()
                        .rposition(|byte| !byte.is_ascii_digit())
                        .map_or(0, |position| position + 1);
                    (start, end, name.as_bytes()[start..end].to_vec())
                })
        } else {
            None
        };
        let mut paths = vec![archive.to_path_buf()];
        for index in 1..count {
            let next = match self {
                Self::RarParts | Self::OldRar => {
                    if let Some((start, end, digits)) = &mut rar_number {
                        let mut carry = true;
                        for digit in digits.iter_mut().rev() {
                            if *digit == b'9' {
                                *digit = b'0';
                            } else {
                                *digit += 1;
                                carry = false;
                                break;
                            }
                        }
                        if carry {
                            digits.insert(0, b'1');
                        }
                        format!(
                            "{}{}{}",
                            &name[..*start],
                            std::str::from_utf8(digits)?,
                            &name[*end..]
                        )
                    } else {
                        let extension = u32::try_from((index - 1) / 100)?
                            .checked_add(u32::from('r'))
                            .and_then(char::from_u32)
                            .context("RAR 分卷编号超出范围")?;
                        format!("{base}.{extension}{:02}", (index - 1) % 100)
                    }
                }
                Self::Numbered => format!("{base}.{:03}", index + 1),
                Self::SplitZip => format!("{}.z{index:02}", &name[..name.len() - 4]),
                Self::Single => unreachable!(),
            };
            let path = archive.with_file_name(next);
            let metadata = fs::symlink_metadata(&path)
                .with_context(|| format!("实际分卷不可访问：{}", path.display()))?;
            ensure!(
                metadata.file_type().is_file(),
                "实际分卷不是普通文件，保留源包：{}",
                path.display()
            );
            paths.push(path);
        }
        Ok(paths)
    }
}
/// 候选组供可逆隔离使用；成功删除前必须转换成经引擎确认的实际卷集合。
#[derive(Debug)]
struct VolumeSet {
    paths: Vec<PathBuf>,
    scheme: VolumeScheme,
}
/// 分卷组解析：返回主体自身 + 同目录下的兄弟卷（X-05 删除与 X-06 隔离的处置单位）。
/// 非分卷包（命名不能匹配任何分卷方案）返回只含主体自身的单项集合，且**不枚举目录**：
/// 单卷 7z/tar/gz 等格式没有可匹配的兄弟卷命名，逐包扫描目录是纯粹的重复工作。
/// 识别口径与旧 protect_volumes 一致：分卷识别只走 rar_part_stem，不用 starts_with
/// 宽匹配（report.partial.rar 不是分卷）。
fn volume_set(archive: &Path) -> Result<VolumeSet> {
    let name = archive
        .file_name()
        .and_then(|s| s.to_str())
        .context("无效压缩包名称")?
        .to_lowercase();
    let (stem, scheme) = if let Some(stem) = rar_part_stem(&name) {
        (stem.to_string(), VolumeScheme::RarParts)
    } else if name.ends_with(".7z.001") || name.ends_with(".zip.001") {
        (name[..name.len() - 4].to_string(), VolumeScheme::Numbered)
    } else if let Some(stem) = name.strip_suffix(".rar") {
        (stem.to_string(), VolumeScheme::OldRar)
    } else if let Some(stem) = name.strip_suffix(".zip") {
        (stem.to_string(), VolumeScheme::SplitZip)
    } else {
        return Ok(VolumeSet {
            paths: vec![archive.to_path_buf()],
            scheme: VolumeScheme::Single,
        });
    };
    // 逐条目复用的匹配前缀与最少位数在扫描前算好（此前每个目录条目要 format! 两次）：
    // rar 的 `.partN` 判定走 rar_part_stem，没有固定前缀；编号命名最少 3 位
    // （7-Zip 多卷可到 .1000+，001/1000 都算兄弟卷，不写死恰好 3 位），宽命名至少 2 位。
    let (prefix, min_digits) = match scheme {
        VolumeScheme::Numbered => (Some(format!("{stem}.")), 3),
        VolumeScheme::OldRar => (Some(format!("{stem}.r")), 2),
        VolumeScheme::SplitZip => (Some(format!("{stem}.z")), 2),
        VolumeScheme::RarParts | VolumeScheme::Single => (None, 0),
    };
    let matches_candidate = |candidate: &str| -> bool {
        match scheme {
            // 与主体识别同口径：只认同主干的 .partN.rar，不用 starts_with 宽匹配。
            VolumeScheme::RarParts => rar_part_stem(candidate).is_some_and(|s| s == stem),
            VolumeScheme::Numbered | VolumeScheme::OldRar | VolumeScheme::SplitZip => {
                prefix.as_deref().is_some_and(|prefix| {
                    candidate.strip_prefix(prefix).is_some_and(|digits| {
                        digits.len() >= min_digits && digits.chars().all(|c| c.is_ascii_digit())
                    })
                })
            }
            VolumeScheme::Single => false,
        }
    };
    let mut paths = vec![archive.to_path_buf()];
    for entry in fs::read_dir(archive.parent().context("压缩包缺少目录")?)? {
        let entry = entry?;
        let candidate = entry.file_name().to_string_lossy().to_lowercase();
        // 主体自身已在集合里（如 part1.rar 对主干同判）；大小写不敏感路径上可能重复命名，去重交给文件系统唯一性。
        if matches_candidate(&candidate) && entry.path() != archive && entry.file_type()?.is_file()
        {
            paths.push(entry.path());
        }
    }
    Ok(VolumeSet { paths, scheme })
}
/// 失败处置（X-06）：把未能完全解开的原包（连同兄弟卷）移入所选目录根下的
/// 「解压失败」子目录。移动用不覆盖改名；同名冲突改用唯一名。失败原因是界面可查的
/// 日志字段。移动不走删除接口——隔离不是删除，原包保持可用等待人工处理。
fn quarantine(job: &mut Job, archive_rel: &str, reason: &str) -> Result<()> {
    let archive = fsutil::safe_join(&job.root, archive_rel)?;
    // 隔离可逆（改名进「解压失败」，用户可移回）：宽命名兄弟卷保持整组隔离。
    // 真 PKZIP/旧 RAR 分卷集失败时常无法从主体取得 Volume Index 佐证，若在此也
    // 设门会把真兄弟卷残留在原目录（.zNN/.rNN 不在扫描口径内，永远不会再被处理）；
    // 误隔离可还原、有日志，误删除不可逆——佐证门只设在删除路径（extract_one）。
    let sources = volume_set(&archive)?.paths;
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
        // 移走后该路径的扫描记录不再代表磁盘现状，标为 inactive（任务库记录与磁盘保持一致）。
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
/// 单个条目合入结果：冲突（目标已存在）不淘汰、不询问，只给新成员改名。
enum MergeOutcome {
    /// 目标位置空闲：成员按原相对路径落位。
    Placed(PathBuf),
    /// 目标已被既有文件/目录/链接占用：既有内容原样不动，新成员改用未占用名
    /// （X-04/H-07）。TOCTOU 竞争后改用唯一名的情况也走这一支：无论哪种冲突，
    /// 结果都是「新成员以另一个名字落盘」。
    Renamed(PathBuf),
}
/// 把暂存成员合入正式位置（X-08：整包解码校验完成后才合入）。
/// 既有目标一律不动：目标存在就改用唯一名（保留扩展名，含复合扩展名），
/// 内容相同也照常落盘；不比较内容、不淘汰、不询问、不删除。
fn merge_extracted(root: &Path, source: &Path, target: &Path) -> Result<MergeOutcome> {
    // 只校验/创建父目录链（ensure_dir 创建目录链本身，ensure_parent 只会创建到祖父目录，
    // 带子目录的成员会因此以「系统找不到指定的路径」整包失败）；
    // 最终名被符号链接 / junction / OneDrive 在线占位占用是成员级场景
    //（下方改用唯一名落盘），对最终名也做整链校验会让含这类成员的整包失败。
    let parent = target.parent().context("目标缺少父目录")?;
    if parent != root {
        fsutil::ensure_dir(root, parent)?;
    }
    if !target.try_exists()? {
        match fsutil::rename_noreplace(source, target) {
            Ok(()) => return Ok(MergeOutcome::Placed(target.to_path_buf())),
            Err(error) => {
                // TOCTOU：目标在 try_exists 与 rename 之间出现。与既有冲突同一处置：
                // 改用唯一名落盘，而不是整包失败（P-08 不要求防御外部改动，但这里
                // 本来就有不覆盖的安全落点）。
                let emergency = fsutil::unique_target(root, target)?;
                fsutil::rename_noreplace(source, &emergency).map_err(|_| error)?;
                return Ok(MergeOutcome::Renamed(emergency));
            }
        }
    }
    // 目标已存在（普通文件、链接、坏链或目录）：绝不覆盖、不淘汰，直接改名落盘。
    let renamed = fsutil::unique_target(root, target)?;
    fsutil::rename_noreplace(source, &renamed).with_context(|| {
        format!(
            "目标已存在且改名落盘失败：{} → {}",
            target.display(),
            renamed.display()
        )
    })?;
    Ok(MergeOutcome::Renamed(renamed))
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
        // 指纹复核：排队期间该路径可能已被删除或替换（X-04 之后成员一律改名落盘，
        // 不会再顶替既有路径，所以这里只剩外部改动这一种可能）。与 enqueue 同公式
        // 重算指纹，不一致即「原包已不在原位」，清掉残留 pending 行并如实记一条跳过：
        // 不得把已改动的路径当原包解压，也不得虚报失败计数或隔离它。
        let Ok(path) = fsutil::safe_join(&job.root, &relative) else {
            job.db
                .conn
                .execute("DELETE FROM archives WHERE id=?1", [id])?;
            continue;
        };
        // 路径已不存在或此刻无法读取：残留行一并清除。
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
                "扫描入队后无法确认原包仍在原位（已被删除、改写或此刻不可读），本次不再处理",
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
            engine.extract_one(job, &relative, depth)
        };
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
                quarantine(
                    job,
                    &relative,
                    "未能完全解开：有成员被跳过、被排除规则命中或位于 Git 目录树内",
                )?;
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

    // 覆盖 H-06：已知祖先不在 Git 树内，不代表后代也没有独立 Git 边界。
    #[test]
    fn git_cache_checks_descendants_of_an_allowed_directory() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("parent");
        let repo = parent.join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        let mut boundaries = GitBoundaries::default();
        assert!(!boundaries.blocked(root.path(), &parent).unwrap());
        assert!(boundaries.blocked(root.path(), &repo).unwrap());
    }

    // 覆盖 H-06：Git 保护只向后代传播，不能错误排除 Git 树外的兄弟目录。
    #[test]
    fn git_cache_does_not_protect_siblings_of_a_repository() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("parent/repo");
        let sibling = root.path().join("parent/ordinary");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        let mut boundaries = GitBoundaries::default();
        assert!(boundaries.blocked(root.path(), &repo).unwrap());
        assert!(!boundaries.blocked(root.path(), &sibling).unwrap());
    }
    use crate::config::Config;
    use crate::control::{Context as TaskContext, Control};
    use crate::db::Database;
    use crate::engine::Job;
    use anyhow::Context;

    // 覆盖 X-03（成员就地解到包所在位置，含缺失父目录）
    #[test]
    fn merge_creates_missing_parent_directories() {
        // 回归：合入成员时把「父目录」传给只创建父级的 ensure_parent，实际只创建到祖父目录，
        // 带子目录的成员改名必然报「系统找不到指定的路径」，于是含子目录的整包（RAR 的子目录成员、
        // 分卷包里的 vols/ 目录）解压失败并留下半截空目录。合入必须创建到成员的父目录。
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        let stage = tempfile::tempdir().unwrap();
        let source = stage.path().join("file1.txt");
        fs::write(&source, b"member payload").unwrap();
        let target = root.join("archives/sub/dir1/file1.txt");

        let outcome = merge_extracted(&root, &source, &target).unwrap();
        assert!(
            matches!(outcome, MergeOutcome::Placed(_)),
            "目标空闲时成员应落到目标路径"
        );
        assert_eq!(fs::read(&target).unwrap(), b"member payload");
        assert!(!source.exists(), "合入后暂存文件应已改名离开");
    }

    // 覆盖 X-04, H-07（回归：目标已有同内容文件时仍必须落盘改名副本，不得以内容相同跳过）
    #[test]
    fn merge_conflict_with_identical_bytes_lands_a_renamed_copy() {
        // 合同 X-04/H-07：冲突即使内容完全相同，也不得跳过新文件落盘、不按内容淘汰；
        // 既有文件不动，新文件改用未占用的名字。整改前这里走「等字节即视为已落位」的
        // 快捷路径：成员不落盘、extracted 虚计一次，用户拿不到本次解出的那份副本。
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        let target = root.join("a.txt");
        fs::write(&target, b"same bytes").unwrap();
        let stage = tempfile::tempdir().unwrap();
        let source = stage.path().join("a.txt");
        fs::write(&source, b"same bytes").unwrap();
        let outcome = merge_extracted(&root, &source, &target).unwrap();
        assert!(
            matches!(outcome, MergeOutcome::Renamed(_)),
            "目标已存在时必须改名落盘而不是按内容跳过"
        );
        assert_eq!(
            fs::read(&target).unwrap(),
            b"same bytes",
            "既有文件必须原样保留（H-07）"
        );
        assert_eq!(
            fs::read(root.join("a (1).txt")).unwrap(),
            b"same bytes",
            "等内容的成员仍必须落盘为改名副本（X-04/H-07）"
        );
        assert!(!source.exists(), "成员已改名离开暂存区");
    }

    // 覆盖 H-07, X-04（回归：冲突改名只改文件名主体，复合扩展名整体保留）
    #[test]
    fn merge_conflict_preserves_compound_extension() {
        // 合同 H-07 的例子：资料.tar.gz → 资料 (1).tar.gz；不得变成 资料.tar (1).gz，
        // 也不得通过更换/追加扩展名规避冲突。整改前该路径会按冲突策略删除既有文件
        // （或按策略丢弃新成员），两个断言都不成立。
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        let target = root.join("资料.tar.gz");
        fs::write(&target, b"old package").unwrap();
        let stage = tempfile::tempdir().unwrap();
        let source = stage.path().join("资料.tar.gz");
        fs::write(&source, b"brand new package content").unwrap();
        let outcome = merge_extracted(&root, &source, &target).unwrap();
        assert!(
            matches!(outcome, MergeOutcome::Renamed(_)),
            "目标已存在时必须改名落盘（H-07）"
        );
        assert_eq!(
            fs::read(&target).unwrap(),
            b"old package",
            "既有文件不得被覆盖或删除（H-07/S-01）"
        );
        assert_eq!(
            fs::read(root.join("资料 (1).tar.gz")).unwrap(),
            b"brand new package content",
            "冲突改名必须保留复合扩展名（H-07）"
        );
        assert!(
            !root.join("资料.tar (1).gz").exists(),
            "不得把复合扩展名拆开改名（H-07）"
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

    // 覆盖 X-06, X-08（回归：空间不足是任务级中止，不得再走隔离；取消与包级失败口径不变）
    #[test]
    fn failure_classification_separates_stop_from_quarantine() {
        // 空间不足必须被识别为「停止整个解压任务」：即使被 with_context 包装（解压
        // 回调的实际形态）也要命中——判定走错误链，而不是顶层 downcast；否则空间不足
        // 又会退回「逐包隔离并继续处理后续包」。
        let space: anyhow::Error = StopExtraction("可用空间不足：本包需 1 GiB".into()).into();
        assert!(stops_extraction(&space), "空间不足必须停止整个任务（X-08）");
        let wrapped = Err::<(), _>(StopExtraction("磁盘剩余空间低于预留阈值".into()))
            .context("解压失败（可能已损坏、加密或格式不受支持）")
            .unwrap_err();
        assert!(
            stops_extraction(&wrapped),
            "被 with_context 包装后仍必须识别（解压回调的实际形态）"
        );
        // 防护上限属于包级失败：按 X-06 隔离，不得停止整个任务。
        let capped = anyhow::anyhow!("压缩包条目数量超过用户设置的上限");
        assert!(!stops_extraction(&capped), "防护上限走包级隔离口径（X-06）");
        // 用户取消：既不是空间不足，也不隔离——沿用 control 的取消口径。
        let control = Control::default();
        control.cancel();
        let cancelled = control.check_cancelled().unwrap_err();
        assert!(!stops_extraction(&cancelled), "取消不得被当成空间不足");
    }

    // 覆盖 X-05, X-08（取消在删除的安全边界生效）：已取消时不启动任何删除，
    // 错误沿用取消口径（不是「停止任务」信号，不隔离、不虚计成功）。
    #[test]
    fn delete_successful_source_stops_on_cancellation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        let volume = root.join("pack.zip");
        fs::write(&volume, b"archive bytes").unwrap();
        let mut job = Job {
            root,
            config: Config::default(),
            context: TaskContext::default(),
            db: Database::create(&temp.path().join("state")).unwrap(),
            summary: crate::model::Summary::default(),
        };
        job.context.control.cancel();

        let error = delete_successful_source(&mut job, "pack.zip", std::slice::from_ref(&volume))
            .unwrap_err();
        assert!(
            !stops_extraction(&error),
            "取消沿用取消口径，不得被当成任务级中止信号处理：{error:#}"
        );
        assert!(volume.is_file(), "取消后不得启动任何删除");
        assert_eq!(job.summary.deleted, 0, "取消后不得虚计删除");
    }
}
