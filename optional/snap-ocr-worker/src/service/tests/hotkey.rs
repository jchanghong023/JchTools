//! //! 热键设置保存与迟到回执（S8-01）回归；含损坏设置的读取保护。
//! 从 `service.rs` 的内联测试模块原样搬移（仅调整 use 导入与自引用测试名）。

use super::*;
use std::fs;
use std::sync::mpsc;

// F41 / O-14 / O-30：组合字符串相同不代表它已注册。隔离托盘的
// active=0 且消息泵被冻结；不执行 RegisterHotKey、不触碰通知区或桌面。
// 保存必须等待真实注册回执，不能先写设置并宣称成功。
#[test]
fn review_f41_same_hotkey_save_waits_for_inactive_registration(
) -> Result<(), Box<dyn std::error::Error>> {
    const CHILD: &str = "JCHTOOLS_REVIEW_F41_HOTKEY_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir()?;
        let status = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "service::tests::hotkey::review_f41_same_hotkey_save_waits_for_inactive_registration",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT", root.path())
            .env("JCHTOOLS_TEST_STATE_DIR", root.path().join("state"))
            .status()?;
        assert!(status.success(), "未注册的同值热键保存必须等待托盘确认");
        return Ok(());
    }
    let root = std::env::var_os("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT")
        .map(std::path::PathBuf::from)
        .ok_or("缺少隔离测试目录")?;
    let (mut service, _work_rx) = ready_busy_service(&root);
    service.phase = ActivityPhase::Idle;
    service
        .settings
        .save(&root)
        .map_err(std::io::Error::other)?;
    let saved = fs::read(root.join("settings.json"))?;
    let (events, _event_rx) = mpsc::channel();
    let mut native = tray::ReviewTray::start(events).map_err(std::io::Error::other)?;
    service.tray = native.handle.clone();
    let response = service.pipe(&serde_json::json!({
        "command": "save-settings",
        "hotkey": "Ctrl+Alt+O",
        "autostart": false
    }));
    // 先停止冻结消息线程，始终经 cleanup_tray_state 收尾；绝不让排队
    // 的 Replace 进入原生注册分支，即使以下先红断言触发也不注册全局热键。
    native.terminate().map_err(std::io::Error::other)?;
    assert_eq!(response["ok"], true, "待确认保存应被接受而非伪造注册失败");
    assert!(
        matches!(service.pending_hotkey, Some(PendingHotkey::Saving { .. })),
        "active=0 的同值热键必须进入待确认状态，不能跳过注册收据"
    );
    assert_eq!(
        fs::read(root.join("settings.json"))?,
        saved,
        "注册尚未确认时不能提交设置文件"
    );
    // 以下只向状态机交付回执，不泵原生托盘，不执行 RegisterHotKey。
    service.handle(Command::HotkeyReceiptTimedOut);
    assert!(matches!(
        service.pending_hotkey,
        Some(PendingHotkey::Saving { .. })
    ));
    assert!(service.settings.warning.is_some(), "超时未确认不得清除警告");
    assert_eq!(fs::read(root.join("settings.json"))?, saved);
    assert!(service.save_settings("Ctrl+Alt+O", false).is_err());
    service.handle(Command::HotkeyReplaced(Err("快捷键仍被占用".into())));
    assert!(service.pending_hotkey.is_none());
    assert_eq!(service.settings.warning.as_deref(), Some("快捷键仍被占用"));
    assert_eq!(fs::read(root.join("settings.json"))?, saved);
    service.pending_hotkey = Some(PendingHotkey::Saving {
        hotkey: "Ctrl+Alt+O".into(),
        old_hotkey: "Ctrl+Alt+O".into(),
        old_autostart: super::AutostartValue::Absent,
        autostart: false,
    });
    service.handle(Command::HotkeyReplaced(Ok(())));
    assert!(service.pending_hotkey.is_none());
    assert!(service.settings.warning.is_none());
    assert_eq!(service.status()["hotkey"], "Ctrl+Alt+O");
    Ok(())
}

// 覆盖 O-15：损坏的设置只进入内存默认值，不被启动或读取过程静默覆盖；
// 用户明确保存以后才用新设置替换。
#[test]
fn corrupt_settings_survive_load_until_explicit_save() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let file = temp.path().join("settings.json");
    fs::write(&file, b"{broken config")?;
    let mut settings = Settings::load(temp.path());
    assert_eq!(settings.hotkey, "Ctrl+Alt+O");
    assert!(settings.warning.is_some());
    assert_eq!(fs::read(&file)?, b"{broken config");
    settings.hotkey = "Ctrl+Shift+PageDown".into();
    settings.save(temp.path())?;
    let value: serde_json::Value = serde_json::from_slice(&fs::read(&file)?)?;
    assert_eq!(value["hotkey"], "Ctrl+Shift+PageDown");
    Ok(())
}

// 覆盖 S8-01：真实 message-only 托盘线程冻结 >2s 时，
// 保存不得立即报「快捷键更新超时」失败终态——
// 排队操作在托盘线程恢复后仍会执行，立即报失败会造成「保存失败、实际
// 热键已变、重启回旧值」。
#[test]
fn timeout_hotkey_save_is_not_reported_failure() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let (mut service, _work_rx) = ready_busy_service(temp.path());
    service.phase = ActivityPhase::Idle;
    let (events, _event_rx) = mpsc::channel();
    let native = tray::ReviewTray::start(events).map_err(std::io::Error::other)?;
    service.tray = native.handle.clone();
    let outcome = service.save_settings("Ctrl+Shift+PageDown", false);
    assert!(outcome.is_ok(), "超时未回执不得立即报失败终态：{outcome:?}");
    assert!(
        service.pending_hotkey.is_some(),
        "超时后必须进入待确认状态等待迟到回执"
    );
    assert_eq!(
        service.settings.hotkey, "Ctrl+Alt+O",
        "回执到达前内存热键保持旧值（保证失败/超时路径重启一致）"
    );
    // 模拟托盘线程在 >2s 后完成注册并回执成功：reconcile 为真实终态
    // （成功），继续完成保存收尾（设置文件落地、内存与文件一致）。
    service.handle(Command::HotkeyReplaced(Ok(())));
    assert_eq!(
        service.settings.hotkey, "Ctrl+Shift+PageDown",
        "迟到成功回执应按成功处理"
    );
    assert!(service.pending_hotkey.is_none());
    assert!(service.settings.warning.is_none(), "成功收尾后不得残留警告");
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(temp.path().join("settings.json"))?)?;
    assert_eq!(saved["hotkey"], "Ctrl+Shift+PageDown");
    assert_eq!(service.status()["hotkey"], "Ctrl+Shift+PageDown");
    Ok(())
}

// 覆盖 S8-01：迟到回执为真失败（如快捷键冲突）时按失败上报——内存与
// 设置文件保持旧热键（托盘 Replace 的失败路径不改托盘状态，无需回滚）。
#[test]
fn delayed_hotkey_failure_reports_and_keeps_old() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let (mut service, _work_rx) = ready_busy_service(temp.path());
    service.phase = ActivityPhase::Idle;
    service.pending_hotkey = Some(PendingHotkey::Saving {
        hotkey: "Ctrl+Shift+PageDown".into(),
        old_hotkey: "Ctrl+Alt+O".into(),
        old_autostart: super::AutostartValue::Absent,
        autostart: false,
    });
    service.handle(Command::HotkeyReplaced(Err(
        "快捷键冲突；原快捷键保持有效".into()
    )));
    assert_eq!(service.settings.hotkey, "Ctrl+Alt+O");
    assert!(
        service
            .settings
            .warning
            .as_deref()
            .is_some_and(|warning| warning.contains("快捷键冲突")),
        "迟到失败必须上报真实原因"
    );
    assert!(service.pending_hotkey.is_none());
    assert!(
        !temp.path().join("settings.json").exists(),
        "失败路径不得落盘新热键"
    );
    Ok(())
}

// 覆盖 S8-01：等待线程的有界时限也耗尽（托盘长时间未泵消息）时的自愈——
// 旧键被重新排队（FIFO 排在仍在等待的新键替换之后），内存与文件保持旧键，
// 上报「确认超时」而非「更新失败」。
#[test]
fn hotkey_receipt_final_timeout_self_heals_to_old() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let (mut service, _work_rx) = ready_busy_service(temp.path());
    service.phase = ActivityPhase::Idle;
    service.pending_hotkey = Some(PendingHotkey::Saving {
        hotkey: "Ctrl+Shift+PageDown".into(),
        old_hotkey: "Ctrl+Alt+O".into(),
        old_autostart: super::AutostartValue::Absent,
        autostart: false,
    });
    service.handle(Command::HotkeyReceiptTimedOut);
    assert_eq!(
        service.settings.hotkey, "Ctrl+Alt+O",
        "回执最终超时后内存保持旧键（文件未写，重启一致）"
    );
    assert!(
        service
            .settings
            .warning
            .as_deref()
            .is_some_and(|warning| warning.contains("快捷键确认超时")),
        "自愈路径上报确认超时与已排队恢复"
    );
    assert!(
        matches!(
            service.pending_hotkey,
            Some(PendingHotkey::Restoring { .. })
        ),
        "恢复操作也必须等待回执，不能假定旧键已恢复"
    );
    service.handle(Command::HotkeyReplaced(
        Err("原热键已被其他应用占用".into()),
    ));
    assert_eq!(service.settings.hotkey, "Ctrl+Shift+PageDown");
    assert!(service.pending_hotkey.is_none());
    Ok(())
}

// 覆盖 O-14：恢复旧键的 30 秒等待超时不是注册失败，迟到成功仍须收尾。
#[test]
fn restore_timeout_keeps_receipt_context_until_terminal_result(
) -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let (mut service, _work_rx) = ready_busy_service(temp.path());
    service.phase = ActivityPhase::Idle;
    service.pending_hotkey = Some(PendingHotkey::Restoring {
        new_hotkey: "Ctrl+Shift+PageDown".into(),
    });
    service.handle(Command::HotkeyReceiptTimedOut);
    assert_eq!(service.settings.hotkey, "Ctrl+Alt+O");
    assert!(matches!(
        service.pending_hotkey,
        Some(PendingHotkey::Restoring { .. })
    ));
    assert!(service.save_settings("Ctrl+Alt+O", false).is_err());
    service.handle(Command::HotkeyReplaced(Ok(())));
    assert_eq!(service.settings.hotkey, "Ctrl+Alt+O");
    assert!(service.pending_hotkey.is_none());
    Ok(())
}
