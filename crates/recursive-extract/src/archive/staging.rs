use super::placement::{classify_occupancy, Occupancy};
use anyhow::{bail, Context, Result};
use jchtools_file_core::fsutil;
use std::fs;
use std::path::{Path, PathBuf};

pub(super) struct Staging {
    pub(super) directory: PathBuf,
    pub(super) content: PathBuf,
    pub(super) owner: String,
    context: jchtools_file_core::control::Context,
    pub(super) remove_container: bool,
    initialized: bool,
}
impl Staging {
    pub(super) fn new(root: &Path, context: &jchtools_file_core::control::Context) -> Result<Self> {
        let owner = uuid::Uuid::new_v4().to_string();
        let (directory, remove_container) = Self::create_directory(root, &owner, context)?;
        // 独占创建成功即建立守卫；初始化失败尚无 OWNER，也只清理本次拥有的目录。
        let mut stage = Self {
            content: directory.join("content"),
            directory,
            owner,
            context: context.clone(),
            remove_container,
            initialized: false,
        };
        fs::write(stage.directory.join("OWNER"), &stage.owner)
            .with_context(|| format!("无法初始化解压所有权标记：{}", stage.directory.display()))?;
        fs::create_dir(&stage.content)
            .with_context(|| format!("无法初始化解压内容目录：{}", stage.directory.display()))?;
        stage.initialized = true;
        Ok(stage)
    }

    /// 只复用安全的固定普通容器；受占用时独占创建同卷备用容器，不进入用户 Git 树。
    fn create_directory(
        root: &Path,
        owner: &str,
        context: &jchtools_file_core::control::Context,
    ) -> Result<(PathBuf, bool)> {
        for index in 0u64..=u64::MAX {
            context.control.checkpoint()?;
            if index == 1 {
                tracing::warn!(
                    event = "archive_staging_container_fallback",
                    stage = "staging_create",
                    "固定临时区容器不可用，改用同卷独立容器"
                );
            }
            let container = if index == 0 {
                root.join(".jchtools-work")
            } else {
                root.join(fsutil::suffixed_candidate(".jchtools-work", "", index))
            };
            let created = match classify_occupancy(&container) {
                Ok(Occupancy::PlainDir) if index == 0 => {
                    if !matches!(fsutil::is_git_root(&container), Ok(false)) {
                        continue;
                    }
                    false
                }
                Ok(Occupancy::Free) => match fs::create_dir(&container) {
                    Ok(()) => true,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(_) if index == 0 => continue,
                    Err(error) => return Err(error).context("无法创建同卷备用解压容器"),
                },
                Ok(_) => continue,
                Err(_) if index == 0 => continue,
                Err(error) => return Err(error),
            };
            let directory = container.join(owner);
            match fs::create_dir(&directory) {
                Ok(()) => return Ok((directory, created)),
                Err(_) if !created => {}
                Err(error) => {
                    // 只清理本次独占创建、且仍为空的容器，不删除任何已有目录。
                    fs::remove_dir(&container).with_context(|| {
                        format!(
                            "创建随机工作区失败（{error}），且无法清理本次空容器：{}",
                            container.display()
                        )
                    })?;
                    return Err(error).context("无法创建随机解压工作区");
                }
            }
        }
        bail!("可表示的同卷解压临时区容器序号已用尽")
    }
}
impl Drop for Staging {
    fn drop(&mut self) {
        if !self.initialized
            || fs::read_to_string(self.directory.join("OWNER"))
                .ok()
                .as_deref()
                == Some(self.owner.as_str())
        {
            // 内容清空后才移除标记；清理失败的残留仍须被后续扫描识别并保护。
            let cleanup = if self.initialized {
                fs::remove_dir_all(&self.content)
                    .and_then(|()| fs::remove_file(self.directory.join("OWNER")))
                    .and_then(|()| fs::remove_dir(&self.directory))
            } else {
                // OWNER 尚未成功时依靠本次独占创建事实；绝不清理预存容器或用户内容。
                fs::remove_dir_all(&self.directory)
            };
            if let Err(error) = cleanup {
                tracing::warn!(
                    event = "archive_staging_cleanup_failed",
                    stage = "owned_directory",
                    error_type = "io",
                    error_code = error.raw_os_error(),
                    "本次归档暂存目录清理失败"
                );
                self.context
                    .emit(jchtools_file_core::control::Event::Error(format!(
                        "本次解压临时区清理失败，残留位置：{}；原因：{error}",
                        self.directory.display()
                    )));
                return;
            }
            if !self.remove_container {
                return;
            }
            if let Some(parent) = self.directory.parent() {
                if let Err(error) = fs::remove_dir(parent) {
                    if error.kind() != std::io::ErrorKind::DirectoryNotEmpty
                        && error.kind() != std::io::ErrorKind::NotFound
                    {
                        tracing::warn!(
                            event = "archive_staging_cleanup_failed",
                            stage = "work_directory",
                            error_type = "io",
                            error_code = error.raw_os_error(),
                            "归档工作空目录清理失败"
                        );
                        self.context
                            .emit(jchtools_file_core::control::Event::Error(format!(
                                "本次解压临时区清理失败，残留位置：{}；原因：{error}",
                                parent.display()
                            )));
                    }
                }
            }
        }
    }
}
