use super::boundaries::GitBoundaries;
use super::observe::StopExtraction;
use anyhow::{ensure, Context, Result};
use jchtools_file_core::{
    fsutil,
    model::bytes,
    rules,
    task::{Job, QUARANTINE_DIR_NAME},
};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// 分卷组实际体积合计（X-08 的展开比例分母）：主体与每个随组分卷各按实际文件大小
/// 相加。单卷大小会随分卷数缩小分母，把健康的分卷组误判为「展开比例超限」。
/// 某个卷读取失败即上抛：分母不能凭空变小。
pub(super) fn volume_bytes(volumes: &[PathBuf]) -> Result<u64> {
    let mut total = 0u64;
    for path in volumes {
        total = total.saturating_add(fs::metadata(path)?.len());
    }
    Ok(total)
}

/// X-08：每步解码前按已知新增逻辑大小检查比例与目标卷预留。
pub(super) fn check_expansion_space(
    job: &Job,
    declared: Option<u64>,
    packed: u64,
    prior_decoded: u64,
) -> Result<()> {
    if let Some(total) = declared {
        let decoded = prior_decoded
            .checked_add(total)
            .context("累计解压字节计数溢出")?;
        if job.config.max_ratio > 0 {
            ensure!(
                decoded <= packed.saturating_mul(job.config.max_ratio),
                "压缩包展开比例超过用户设置的上限"
            );
        }
    }
    let reserve = job.config.reserve_bytes;
    let free = fs2::available_space(&job.root)
        .map_err(|error| StopExtraction(format!("无法查询磁盘可用空间：{error}")))?;
    if free < reserve {
        return Err(StopExtraction(format!(
            "可用空间不足：预留 {}，当前 {}；已保留原包并停止本次解压，未合入任何解压文件",
            bytes(reserve),
            bytes(free)
        ))
        .into());
    }
    if let Some(total) = declared {
        if total.checked_add(reserve).context("容量计算溢出")? > free {
            return Err(StopExtraction(format!(
                "可用空间不足：本包需 {}，预留 {}，当前 {}；已保留原包并停止本次解压，未合入任何解压文件",
                bytes(total), bytes(reserve), bytes(free)
            )).into());
        }
    }
    Ok(())
}

pub(super) fn skip_git_root(
    job: &mut Job,
    archive_rel: &str,
    subtrees: &HashSet<String>,
) -> Result<bool> {
    if !subtrees.contains("") {
        return Ok(false);
    }
    job.summary.skipped += 1;
    job.log(
        "解压",
        archive_rel,
        "",
        "跳过",
        "压缩包根目录含 .git：按 H-06 整树排除，未解出任何成员；原包保留",
        0,
    )?;
    Ok(true)
}

/// X-08：完整暂存结果先检查，再允许任何成员合入。未知声明大小同样检查实际比例。
pub(super) fn validate_staging(
    job: &Job,
    content: &Path,
    declared: Option<u64>,
    packed: u64,
    prior_decoded: u64,
) -> Result<u64> {
    let mut expanded = 0u64;
    for entry in walkdir::WalkDir::new(content)
        .follow_links(false)
        .min_depth(1)
    {
        job.context.control.checkpoint()?;
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        ensure!(
            !fsutil::is_link(&metadata),
            "解压结果出现链接，拒绝整包合入"
        );
        let relative = fsutil::relative_string(content, entry.path())?;
        fsutil::safe_relative(&relative)?;
        if metadata.is_file() {
            expanded = expanded
                .checked_add(metadata.len())
                .context("解压字节计数溢出")?;
        } else {
            ensure!(metadata.is_dir(), "解压结果含非普通文件");
        }
    }
    if let Some(total) = declared {
        ensure!(expanded == total, "解压总量与条目清单不一致，原包保留");
    }
    let decoded = prior_decoded
        .checked_add(expanded)
        .context("累计解压字节计数溢出")?;
    if job.config.max_ratio > 0 {
        ensure!(
            decoded <= packed.saturating_mul(job.config.max_ratio),
            "压缩包实际展开比例超过用户设置的上限"
        );
    }
    Ok(decoded)
}

/// 随包 7z2603 的 CSystemException 以独立系统诊断块输出 HRESULT；
/// 只识别原生空间错误的精确消息，不把退出码 2 或任意错误文本当作磁盘满。
#[cfg(windows)]
pub(super) fn engine_space_messages() -> [(i32, String); 2] {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn FormatMessageW(
            flags: u32,
            source: *const std::ffi::c_void,
            message: u32,
            language: u32,
            buffer: *mut u16,
            size: u32,
            arguments: *const std::ffi::c_void,
        ) -> u32;
    }
    [112_i32, 39].map(|code| {
        let hresult = 0x8007_0000 | code.cast_unsigned();
        let mut buffer = [0u16; 2048];
        // SAFETY: 独占定长缓冲区有效，FROM_SYSTEM 不读取 source，IGNORE_INSERTS 不读取 arguments。
        let length = unsafe {
            FormatMessageW(
                0x1000 | 0x0200,
                std::ptr::null(),
                hresult,
                0,
                buffer.as_mut_ptr(),
                2048,
                std::ptr::null(),
            )
        };
        let message = if length == 0 {
            format!("Error #{hresult:08X}")
        } else {
            String::from_utf16_lossy(&buffer[..length as usize])
                .trim_end_matches("\r\n")
                .to_string()
        };
        (code, message)
    })
}

#[cfg(windows)]
pub(super) fn engine_space_error_line(
    system_error: &mut bool,
    messages: &[(i32, String); 2],
    line: &str,
) -> Option<std::io::Error> {
    let line = line.trim();
    let was_system_error = std::mem::replace(system_error, line == "System ERROR:");
    if was_system_error {
        return messages
            .iter()
            .find(|(_, message)| line == message)
            .map(|(code, _)| std::io::Error::from_raw_os_error(*code));
    }
    // 7z2603 ExtractResult 仅对112直接上抛；39打印此完整系统错误行后吞掉HRESULT。
    // 因而39必须在该确定协议边界恢复数字错误，不能依赖最终通用退出码2。
    if let Some(message) = line.strip_prefix("ERROR: ") {
        return messages
            .iter()
            .find(|(code, expected)| *code == 39 && message == expected)
            .map(|(code, _)| std::io::Error::from_raw_os_error(*code));
    }
    None
}

/// S-04：先约束整个在场卷组，再允许引擎、隔离或删源访问任一卷。
/// 不过滤掉范围外卷后继续解码，否则会把显式排除误报成物理缺卷。
pub(super) fn volume_scope_allowed(
    job: &mut Job,
    archive_rel: &str,
    paths: &[PathBuf],
) -> Result<bool> {
    use jchtools_file_core::scan::{special_prefixes, ScopeFilter};
    let exclusions = rules::build_exclusions(&job.config.exclusions)?;
    let executable = std::env::current_exe()?;
    let (state_prefix, exe_prefix, root_under_special) =
        special_prefixes(&job.root, Some(&job.db.directory), executable.parent());
    let protected_prefix = fsutil::relative_string(&job.root, &fsutil::protected_root()?).ok();
    let scope = ScopeFilter {
        excluded: &exclusions,
        include_hidden: job.config.include_hidden,
        include_system: job.config.include_system,
        quarantine: Some(QUARANTINE_DIR_NAME),
        state_prefix,
        exe_prefix,
        protected_prefix,
        root_under_special,
    };
    let mut git = GitBoundaries::default();
    let mut blocked = None;
    for path in paths {
        let relative = fsutil::relative_string(&job.root, path)?;
        if !job.config.recursive && path.parent() != Some(job.root.as_path()) {
            blocked = Some(relative);
            break;
        }
        let mut current = job.root.clone();
        let mut prefix = String::new();
        for component in relative.split('/') {
            current.push(component);
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            // glob/祖先边界先判定，禁止为了检查排除后代而进入该子树。
            if exclusions.is_match(&prefix) {
                blocked = Some(prefix.clone());
                break;
            }
            let metadata = fs::symlink_metadata(&current)?;
            if scope.prunes(&prefix, component, &metadata)
                || (metadata.is_dir() && git.blocked(&job.root, &current)?)
            {
                blocked = Some(prefix.clone());
                break;
            }
        }
        if blocked.is_some() {
            break;
        }
    }
    if let Some(blocked) = blocked {
        job.summary.skipped += 1;
        job.log(
            "解压",
            archive_rel,
            &blocked,
            "跳过",
            "范围限制：实际卷组含排除项；整组原位保留，未访问引擎、隔离或删源",
            0,
        )?;
        return Ok(false);
    }
    Ok(true)
}
