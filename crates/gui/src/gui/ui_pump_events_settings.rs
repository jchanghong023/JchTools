//! 界面事件泵的设置协议事件：Notice/Error 中 SETTINGS_* 前缀的代际门解码与设置页上屏（自 ui_pump_events.rs 拆分）。
use super::{start_markdown_readiness, start_snap_readiness, AppWindow, UiPump};

impl UiPump {
    /// Event::Notice 中 SETTINGS_LOADED（启动快照）/SETTINGS_READY（设置操作收尾）/
    /// SETTINGS_FEATURES（资产状态）前缀的解码与上屏；命中任一前缀返回 true（原臂以
    /// continue 结束该前缀分支，不再落到通用尾部），否则返回 false 交回通用提示上屏。
    /// 前缀互斥，判定顺序与拆分前一致；各前缀内代际不匹配时同样视为已消费（丢弃）。
    pub(super) fn apply_notice_settings_protocol(&self, ui: &AppWindow, text: &str) -> bool {
        if let Some(rest) = text.strip_prefix("SETTINGS_LOADED|") {
            let Some((generation, payload)) = rest.split_once('|') else {
                return true;
            };
            if generation.parse::<u64>().ok() != Some(self.state.borrow().settings_generation) {
                return true;
            }
            // 启动快照不是设置操作的终态，不清运行标志或消费关闭请求。
            self.apply_settings_snapshot(ui, payload, false);
            return true;
        }
        if let Some(rest) = text.strip_prefix("SETTINGS_READY|") {
            let Some((generation, payload)) = rest.split_once('|') else {
                return true;
            };
            if generation.parse::<u64>().ok() != Some(self.state.borrow().settings_generation) {
                return true;
            }
            self.apply_settings_snapshot(ui, payload, true);
            return true;
        }
        if let Some(rest) = text.strip_prefix("SETTINGS_FEATURES|") {
            let Some((generation, payload)) = rest.split_once('|') else {
                return true;
            };
            if generation.parse::<u64>().ok() != Some(self.state.borrow().settings_generation) {
                return true;
            }
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) {
                ui.set_settings_document_status(
                    value["document_status"].as_str().unwrap_or("未下载").into(),
                );
                ui.set_settings_snapshot_status(
                    value["snapshot_status"].as_str().unwrap_or("未下载").into(),
                );
                ui.set_settings_media_status(
                    value["media_status"].as_str().unwrap_or("未下载").into(),
                );
                ui.set_settings_ffmpeg_status(
                    value["ffmpeg_status"].as_str().unwrap_or("未下载").into(),
                );
                ui.set_settings_audio_model_status(
                    value["audio_model_status"]
                        .as_str()
                        .unwrap_or("未下载")
                        .into(),
                );
            }
            return true;
        }
        false
    }

    /// Event::Error 中 SETTINGS_LOAD_ERR（加载失败）与 SETTINGS_ERROR（设置操作失败）
    /// 前缀的解码与上屏；命中任一前缀返回 true（原臂以 continue 结束，不再落到通用
    /// 错误条尾部）。SETTINGS_ERROR 收尾还须复位保存/初始化标志并补查两侧就绪真值。
    pub(super) fn apply_error_settings_protocol(&self, ui: &AppWindow, text: &str) -> bool {
        if let Some(rest) = text.strip_prefix("SETTINGS_LOAD_ERR|") {
            let Some((generation, error)) = rest.split_once('|') else {
                return true;
            };
            if generation.parse::<u64>().ok() == Some(self.state.borrow().settings_generation) {
                ui.set_settings_status(error.into());
                if !ui.get_settings_loaded() {
                    ui.set_settings_load_failed(true);
                }
            }
            return true;
        }

        if let Some(rest) = text.strip_prefix("SETTINGS_ERROR|") {
            let Some((generation, error)) = rest.split_once('|') else {
                return true;
            };
            if generation.parse::<u64>().ok() != Some(self.state.borrow().settings_generation) {
                return true;
            }
            ui.set_settings_status(error.into());
            if ui.get_convert_initializing() {
                ui.set_settings_audio_failed(true);
            }
            if !ui.get_settings_loaded() {
                ui.set_settings_load_failed(true);
            }
            ui.set_convert_initializing(false);
            ui.set_convert_runtime_saving(false);
            self.state.borrow_mut().convert_init_cancel = None;
            start_markdown_readiness(ui, &self.state, &self.out);
            start_snap_readiness(ui, &self.state, &self.out);
            // U-09/S1-01：任务运行中不得消费 close_after 提前退出，关闭由
            // 任务终态统一完成（镜像 on_close_requested 的守卫口径）。
            if self.close_after_if_idle(ui) {
                let _ = slint::quit_event_loop();
            }
            return true;
        }
        false
    }
}
