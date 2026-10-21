    use sha2::{Digest, Sha256};
    use std::fs;
    use std::path::Path;
    use std::sync::{Mutex, MutexGuard};

    /// 测试共享进程环境变量，组件根相关用例必须串行访问；锁实例与
    /// snap_ocr_assets 的测试共用（见 foundation::test_support），避免两模块的
    /// 覆盖变量在并行线程中相互覆盖。
    pub(super) fn env_lock() -> &'static Mutex<()> {
        jchtools_foundation::test_support::env_lock()
    }

    /// 把资产根目录与推理组件目录都重定向到临时目录；Drop 恢复环境。
    pub(super) struct ComponentGuard {
        pub(super) root: tempfile::TempDir,
        _lock: MutexGuard<'static, ()>,
        previous_env: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    pub(super) fn redirect_component_env() -> ComponentGuard {
        let lock = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temporary_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.tmp");
        fs::create_dir_all(&temporary_root).expect("创建仓库临时目录");
        let root = tempfile::tempdir_in(temporary_root).expect("创建资产根目录");
        let previous_env = [
            "JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT", "JCHTOOLS_TEST_STATE_DIR",
            "JCHTOOLS_TEST_BUNDLED_XBERG_DIR", "JCHTOOLS_TEST_AUDIO_DOWNLOAD_BARRIER",
        ].into_iter().map(|name| (name, std::env::var_os(name))).collect();
        std::env::set_var("JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT", root.path());
        std::env::set_var("JCHTOOLS_TEST_STATE_DIR", root.path().join("app-settings"));
        std::env::remove_var("JCHTOOLS_TEST_BUNDLED_XBERG_DIR");
        std::env::remove_var("JCHTOOLS_TEST_AUDIO_DOWNLOAD_BARRIER");
        ComponentGuard { root, _lock: lock, previous_env }
    }

    impl Drop for ComponentGuard {
        fn drop(&mut self) {
            for (name, value) in &self.previous_env {
                if let Some(value) = value {
                    std::env::set_var(name, value);
                } else {
                    std::env::remove_var(name);
                }
            }
        }
    }

    pub(super) fn sha256_bytes(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hex::encode(hasher.finalize())
    }
