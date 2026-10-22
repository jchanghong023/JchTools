//! //! 开机启动注册表回归：隔离写入、损坏值分类与显式保存修复。
//! 从 `service.rs` 的内联测试模块原样搬移（仅调整 use 导入与自引用测试名）。

use super::*;

// 覆盖 O-15/O-29：测试服务的自启读写只能作用于本轮测试值，
// 与代表原有用户设置的隔离 sentinel 无关；绝不读取或更改真实产品 Run 值。
#[test]
fn isolated_service_autostart_preserves_existing_sentinel() -> Result<(), Box<dyn std::error::Error>>
{
    const CHILD: &str = "JCHTOOLS_AUTOSTART_ISOLATION_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "service::tests::autostart::isolated_service_autostart_preserves_existing_sentinel",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .env_remove("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT")
            .status()?;
        assert!(status.success(), "隔离自启服务回归必须通过");
        return Ok(());
    }
    struct Values(Vec<String>);
    impl Drop for Values {
        fn drop(&mut self) {
            for path in &self.0 {
                assert!(path.starts_with(r"Software\JchTools\Tests\SnapOcr\"));
                super::set_autostart_value(path, false).expect("必须清理本轮创建的测试自启值");
                let wide = super::protocol::wide(path);
                // SAFETY: 只删除本测试独占子键，生产 Run 路径不参与。
                let status = unsafe {
                    windows_sys::Win32::System::Registry::RegDeleteKeyW(
                        windows_sys::Win32::System::Registry::HKEY_CURRENT_USER,
                        wide.as_ptr(),
                    )
                };
                assert_eq!(status, 0, "必须清理本轮注册表子键");
            }
        }
    }
    let temp = tempfile::tempdir()?;
    let sentinel = format!(
        r"Software\JchTools\Tests\SnapOcr\unit-{}",
        std::process::id()
    );
    let mut cleanup = Values(vec![sentinel.clone()]);
    super::set_autostart_value(&sentinel, true).map_err(std::io::Error::other)?;
    let saved = super::read_autostart_value(&sentinel).map_err(std::io::Error::other)?;
    assert!(matches!(saved, super::StoredAutostartValue::Command(_)));
    std::env::set_var("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT", temp.path());
    let isolated = super::autostart_registry_path().map_err(std::io::Error::other)?;
    assert_ne!(isolated, sentinel);
    cleanup.0.push(isolated);
    super::set_autostart(true).map_err(std::io::Error::other)?;
    assert!(super::autostart_enabled());
    let (mut service, _work_rx) = ready_busy_service(temp.path());
    service.phase = ActivityPhase::Idle;
    service
        .save_settings("Ctrl+Alt+O", false)
        .map_err(std::io::Error::other)?;
    // F41：未注册的同值热键须在成功回执后才提交设置；本例仅验证
    // 注册表隔离，不创建托盘窗口或注册用户的全局热键。
    service.handle(Command::HotkeyReplaced(Ok(())));
    assert!(!super::autostart_enabled(), "服务保存应关闭本轮测试自启");
    assert_eq!(
        super::read_autostart_value(&sentinel).map_err(std::io::Error::other)?,
        saved,
        "原有 sentinel 设置必须逐字保留"
    );
    Ok(())
}

// 覆盖 O-15：本功能启动项的错误类型或超长内容属于损坏设置，
// 不得阻止服务启动，也不能在读取时改写原值；显式关闭仍可修复。
#[test]
fn invalid_autostart_registry_type_is_corrupt() -> Result<(), Box<dyn std::error::Error>> {
    let path = format!(
        r"Software\JchTools\Tests\SnapOcr\invalid-value-{}",
        std::process::id()
    );
    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            super::set_autostart_value(&self.0, false).expect("清理本测试启动项");
            let path = super::protocol::wide(&self.0);
            // SAFETY: 仅删除本测试独占的非 Run 子键，不接触产品自启。
            assert_eq!(
                unsafe {
                    windows_sys::Win32::System::Registry::RegDeleteKeyW(
                        windows_sys::Win32::System::Registry::HKEY_CURRENT_USER,
                        path.as_ptr(),
                    )
                },
                0
            );
        }
    }
    super::set_autostart_value(&path, false).map_err(std::io::Error::other)?;
    let _cleanup = Cleanup(path.clone());
    let wide_path = super::protocol::wide(&path);
    let name = super::protocol::wide(super::VALUE);
    for (kind, bytes) in [(4, vec![1u8, 0, 0, 0]), (1, vec![0u8; 32770])] {
        let mut key = 0usize;
        // SAFETY: 子键由本测试创建；输出句柄由本分支关闭。
        assert_eq!(
            unsafe {
                super::RegOpenKeyExW(
                    super::HKEY_CURRENT_USER,
                    wide_path.as_ptr(),
                    0,
                    0x0003,
                    &raw mut key,
                )
            },
            0
        );
        // SAFETY: key 有效，bytes 在同步写入期间存活，大小按字节计。
        let written = unsafe {
            super::RegSetValueExW(
                key,
                name.as_ptr(),
                0,
                kind,
                bytes.as_ptr(),
                u32::try_from(bytes.len())?,
            )
        };
        // SAFETY: 本分支独占的句柄已写完，不再使用。
        unsafe { super::RegCloseKey(key) };
        assert_eq!(written, 0);
        let raw = super::read_autostart_value(&path).map_err(std::io::Error::other)?;
        assert_eq!(
            raw,
            super::StoredAutostartValue::Corrupt,
            "损坏自启应作为可报告状态，不能让服务启动失败"
        );
        // 读取只能分类：原注册表值的类型与字节必须保留。
        let mut actual_kind = 0;
        let mut actual = vec![0u8; bytes.len()];
        let mut size = u32::try_from(actual.len())?;
        // SAFETY: 同一独占测试子键，输出句柄在查询后关闭。
        assert_eq!(
            unsafe {
                super::RegOpenKeyExW(
                    super::HKEY_CURRENT_USER,
                    wide_path.as_ptr(),
                    0,
                    0x0001,
                    &raw mut key,
                )
            },
            0
        );
        // SAFETY: 缓冲按已知测试值长度分配，类型和大小出参均在本栈上。
        let queried = unsafe {
            super::RegQueryValueExW(
                key,
                name.as_ptr(),
                std::ptr::null_mut(),
                &raw mut actual_kind,
                actual.as_mut_ptr(),
                &raw mut size,
            )
        };
        // SAFETY: 查询已结束，关闭本分支独占句柄。
        unsafe { super::RegCloseKey(key) };
        assert_eq!(queried, 0);
        assert_eq!(actual_kind, kind);
        assert_eq!(actual, bytes);
        super::set_autostart_value(&path, false).map_err(std::io::Error::other)?;
        assert_eq!(
            super::read_autostart_value(&path).map_err(std::io::Error::other)?,
            super::StoredAutostartValue::Absent
        );
    }
    super::set_autostart_value(&path, true).map_err(std::io::Error::other)?;
    let normal = super::read_autostart_value(&path).map_err(std::io::Error::other)?;
    let super::StoredAutostartValue::Command(command) = normal else {
        panic!("正常 REG_SZ 必须保留命令");
    };
    assert_eq!(
        super::classify_autostart(Some(&command), &std::env::current_exe()?),
        super::AutostartValue::Current
    );
    assert_eq!(
        super::read_autostart_value(&format!(r"{path}\missing")).map_err(std::io::Error::other)?,
        super::StoredAutostartValue::Absent
    );

    // 真实拒绝读取必须仍是 I/O 错误，不能吞成损坏或不存在。
    // 仅修改本测试子键的 DACL；已有句柄保留 WRITE_DAC 用于恢复。
    #[link(name = "advapi32")]
    extern "system" {
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            text: *const u16,
            revision: u32,
            descriptor: *mut *mut std::ffi::c_void,
            size: *mut u32,
        ) -> i32;
        fn RegSetKeySecurity(key: usize, info: u32, descriptor: *const std::ffi::c_void) -> i32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(memory: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    }
    let mut key = 0usize;
    // SAFETY: 测试独占子键，打开查询及修改 DACL 权限，句柄稍后关闭。
    assert_eq!(
        unsafe {
            super::RegOpenKeyExW(
                super::HKEY_CURRENT_USER,
                wide_path.as_ptr(),
                0,
                0x40003,
                &raw mut key,
            )
        },
        0
    );
    for (sddl, deny) in [
        ("D:P(D;;0x1;;;OW)(A;;GA;;;OW)", true),
        ("D:P(A;;GA;;;OW)", false),
    ] {
        let text = super::protocol::wide(sddl);
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: 字符串带 NUL，输出描述符由 LocalFree 释放。
        assert_ne!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    text.as_ptr(),
                    1,
                    &raw mut descriptor,
                    std::ptr::null_mut(),
                )
            },
            0
        );
        // SAFETY: key 保留 WRITE_DAC，描述符有效，仅替换测试键 DACL。
        let changed = unsafe { RegSetKeySecurity(key, 4, descriptor) };
        // SAFETY: 描述符由上述转换分配，设置完成后释放一次。
        unsafe { LocalFree(descriptor) };
        assert_eq!(changed, 0);
        if deny {
            assert!(
                super::read_autostart_value(&path).is_err(),
                "真实拒绝读取不得误报损坏"
            );
        }
    }
    // SAFETY: 已恢复测试键权限，关闭独占句柄。
    unsafe { super::RegCloseKey(key) };
    Ok(())
}

// 覆盖 O-15：显式保存关闭必须删除本功能的损坏启动项，而非仅显示关闭。
#[test]
fn explicit_disabled_save_repairs_corrupt_autostart() {
    use super::{autostart_update_required, AutostartValue};
    assert!(autostart_update_required(AutostartValue::Corrupt, false));
    assert!(autostart_update_required(AutostartValue::Corrupt, true));
    assert!(!autostart_update_required(AutostartValue::Absent, false));
    assert!(!autostart_update_required(AutostartValue::Current, true));
    assert!(autostart_update_required(AutostartValue::Current, false));
    assert!(autostart_update_required(AutostartValue::Absent, true));
}

// 覆盖 S8-05：Run 值归类——空串/不可解析/他程序/缺参数都是损坏，不得
// 当作已启用（否则启动时会静默重写、擅自重开自启动）；形状有效但路径
// 迁移/过期（Stale）仍按已启用处理（用户此前主动开启，启动时刷新路径）。
#[test]
fn autostart_value_classification_rejects_corrupt_entries() {
    use super::{classify_autostart, AutostartValue};
    let current = std::path::Path::new(r"C:\app\worker\v1\snap-ocr-worker.exe");
    assert_eq!(classify_autostart(None, current), AutostartValue::Absent);
    assert_eq!(
        classify_autostart(Some(""), current),
        AutostartValue::Corrupt,
        "空串是损坏值，不是已启用（S8-05）"
    );
    assert_eq!(
        classify_autostart(Some("垃圾值"), current),
        AutostartValue::Corrupt
    );
    assert_eq!(
        classify_autostart(
            Some(r#""C:\Windows\System32\notepad.exe" --service--autostart"#),
            current
        ),
        AutostartValue::Corrupt,
        "指向他程序（basename 不符）是损坏值"
    );
    assert_eq!(
        classify_autostart(Some(r#""C:\app\worker\v1\snap-ocr-worker.exe""#), current),
        AutostartValue::Corrupt,
        "缺 --service--autostart 参数不可判为本服务的值"
    );
    assert_eq!(
        classify_autostart(
            Some(r#""C:\app\worker\v1\snap-ocr-worker.exe" --service--autostart"#),
            current
        ),
        AutostartValue::Current
    );
    assert_eq!(
        classify_autostart(
            Some(r#""C:\app\worker\v9\snap-ocr-worker.exe" --service--autostart"#),
            current
        ),
        AutostartValue::Stale,
        "形状有效的旧安装路径按「已启用待刷新」处理"
    );
    // 损坏值永远不是 Stale：启动时的刷新分支（仅 Stale 触发）不会改写它。
    assert_ne!(
        classify_autostart(Some(""), current),
        AutostartValue::Stale,
        "损坏值不得触发启动重写（S8-05）"
    );
}
