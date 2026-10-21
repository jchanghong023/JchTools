//! 设置页：设置快照生成、操作门禁与回包接线（自 gui.rs 拆分）。
use super::*;

/// 所有状态来自唯一生效目录；只检查成员存在，不加载模型或联网。
pub(super) fn settings_payload() -> Result<String, String> {
    let active = xberg_settings::effective_directory()?;
    let migration_warning = xberg_settings::settings()?.migration_warning;
    let feature_status = |scenario: &str| {
        active.as_ref().map_or("未找到运行目录", |path| {
            if jchtools_xberg::xberg_runtime::validate_assets(path, scenario).is_ok() {
                "已安装"
            } else {
                "组件缺失"
            }
        })
    };
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../../../../resources/markdown-assets.json"))
            .map_err(|error| format!("无法读取运行资产清单：{error}"))?;
    let audio_status = |ffmpeg: bool| {
        let Some(root) = active.as_ref() else {
            return "未下载";
        };
        let members = manifest["xberg"]["members"].as_array();
        let (required, present) = members
            .into_iter()
            .flatten()
            .filter_map(|member| member["path"].as_str())
            .filter(|path| {
                if ffmpeg {
                    path.starts_with("ffmpeg/")
                } else {
                    path.starts_with("models/sense_voice_") || path.starts_with("models/vad/")
                }
            })
            .fold((0usize, 0usize), |(required, present), path| {
                (
                    required + 1,
                    present + usize::from(root.join(path).is_file()),
                )
            });
        if required == 0 {
            "资产清单缺失"
        } else if present == required {
            "已就绪"
        } else if present != 0 {
            "资产不完整，请补齐"
        } else {
            "未下载"
        }
    };
    let release = active.as_ref().map(|root| {
        let receipt = std::fs::read(root.join("jchtools-xberg-release.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
        receipt
            .as_ref()
            .filter(|value| value["owner"] == "JchTools-bundled-xberg-v1")
            .and_then(|value| value["tag"].as_str())
            .map_or_else(
                || "当前目录未提供有效的随包发布记录；音频下载需要匹配的发布记录".to_string(),
                |tag| format!("随包版本：{tag}；来源：jchanghong023/xberg 正式发布"),
            )
    });
    Ok(serde_json::json!({
        "active": active.as_ref().map(|path| jchtools_file_core::platform::display_path_text(&path.display().to_string())),
        "release": release,
        "migration_warning": migration_warning,
        "document_status": feature_status("document"),
        "snapshot_status": feature_status("snapshot"),
        "ffmpeg_status": audio_status(true),
        "audio_model_status": audio_status(false),
        "media_status": if active.as_ref().is_some_and(|path|
            jchtools_xberg::xberg_runtime::validate_assets(path, "media").is_ok()) {
            "已就绪"
        } else {
            "未就绪，请主动下载音频资产"
        }
    }).to_string())
}

#[cfg(test)]
pub(super) type SettingsDownload =
    dyn Fn(&std::sync::atomic::AtomicBool) -> Result<PathBuf, String> + Send + Sync;

fn send_settings_result(out: &EventSender, generation: u64, result: Result<String, String>) {
    let event = match result {
        Ok(payload) => Event::Notice(format!("SETTINGS_READY|{generation}|{payload}")),
        Err(error) => {
            // 刷新缺损组件状态，但不替换活动来源或失败文案。
            if let Ok(payload) = settings_payload() {
                let _ = out.send(Event::Notice(format!(
                    "SETTINGS_FEATURES|{generation}|{payload}"
                )));
            }
            Event::Error(format!("SETTINGS_ERROR|{generation}|{error}"))
        }
    };
    let _ = out.send(event);
}

fn settings_blocked(ui: &AppWindow, state: &Rc<RefCell<State>>, allow_load_retry: bool) -> bool {
    !(ui.get_settings_loaded() || allow_load_retry && ui.get_settings_load_failed())
        || ui.get_busy()
        || ui.get_convert_initializing()
        || ui.get_convert_preparing()
        || ui.get_convert_runtime_saving()
        || ui.get_snap_initializing()
        || ui.get_confirm_kind() != 0
        || state.borrow().close_after
}

fn begin_settings(state: &Rc<RefCell<State>>) -> u64 {
    let mut s = state.borrow_mut();
    s.settings_generation = s.settings_generation.wrapping_add(1);
    s.convert_readiness_generation = s.convert_readiness_generation.wrapping_add(1);
    s.settings_generation
}

pub(super) fn wire_settings(ui: &AppWindow, state: &Rc<RefCell<State>>, out: &EventSender) {
    let weak = ui.as_weak();
    let state = state.clone();
    let out = out.clone();
    ui.on_settings_action(move |action| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if action == "cancel" {
            if let Some(cancel) = &state.borrow().convert_init_cancel {
                cancel.store(true, Ordering::Release);
                ui.set_settings_status("正在取消音频资产下载；已验证资产保持不变".into());
            }
            return;
        }
        if settings_blocked(&ui, &state, action == "check") {
            return;
        }
        if action != "download" && action != "check" {
            ui.set_settings_status("未知设置操作".into());
            return;
        }
        if action == "download" && !ui.get_convert_runtime_confirmed() {
            ui.set_settings_status("未找到共享运行目录，请先修复安装包或重新解包".into());
            return;
        }
        let generation = begin_settings(&state);
        let reload_settings = !ui.get_settings_loaded();
        let action = action.to_string();
        let cancel =
            (action == "download").then(|| Arc::new(std::sync::atomic::AtomicBool::new(false)));
        ui.set_convert_initializing(action == "download");
        ui.set_convert_runtime_saving(action == "check");
        ui.set_settings_status("正在处理，请稍候…".into());
        if action == "download" {
            ui.set_settings_audio_failed(false);
        }
        state.borrow_mut().convert_init_cancel.clone_from(&cancel);
        #[cfg(test)]
        let download = state.borrow().settings_download.clone();
        let out = out.clone();
        std::thread::spawn(move || {
            let result = (|| {
                if action == "check" {
                    if reload_settings {
                        // 读取失败后的离线恢复不依赖引擎存在，也不下载或改写来源。
                        return settings_payload();
                    }
                    let saved = xberg_settings::required()?;
                    jchtools_xberg::xberg_runtime::validate_assets(&saved, "engine")?;
                    markdown_assets::ensure_document_notice()?;
                } else {
                    let cancel = cancel
                        .as_ref()
                        .ok_or_else(|| "下载取消信号不可用".to_string())?;
                    #[cfg(test)]
                    if let Some(download) = download {
                        download(cancel)?;
                        return settings_payload();
                    }
                    markdown_assets::download_audio_runtime(cancel, |message| {
                        let _ = out.send(Event::Status(format!(
                            "SETTINGS_PROGRESS|{generation}|{message}"
                        )));
                    })?;
                }
                settings_payload()
            })();
            send_settings_result(&out, generation, result);
        });
    });
}
