//! 规则面板行构建与联动：RuleSpec 解析、行可见性、容量解析与变更应用（自 gui.rs 拆分）。
use super::{
    apply_theme, default_tools, invalid_number_error, invalidate, refresh_scope_notice, AppWindow,
    Config, DeleteChoice, ModelRc, Rc, RefCell, Result, RuleRow, SharedString, State, Tier, Tool,
    VecModel,
};
use serde::Deserialize;
use slint::Model;

#[derive(Clone, Deserialize)]
pub(super) struct RuleSpec {
    pub(super) section: String,
    pub(super) key: String,
    pub(super) title: String,
    pub(super) hint: String,
    pub(super) kind: String,
    pub(super) choices: Vec<Vec<String>>,
    #[serde(default)]
    tier: Tier,
    #[serde(default = "default_tools")]
    pub(super) tools: Vec<String>,
    /// 附录 D：容量项可用 B/KiB/MiB/GiB/TiB 单位输入；None 表示普通整数。
    #[serde(default)]
    pub(super) unit: Option<String>,
}

/// 选项可带第三项作为该选项的说明；否则回落到规则默认 hint。
/// 用于「选中项改变后，下面那行注释文字跟着变」。
pub(super) fn hint_for(spec: &RuleSpec, value: &serde_json::Value) -> String {
    if spec.kind == "choice" {
        if let Some(pos) = spec
            .choices
            .iter()
            .position(|c| Some(c[0].as_str()) == value.as_str())
        {
            if let Some(hint) = spec.choices[pos].get(2) {
                return hint.clone();
            }
        }
    }
    spec.hint.clone()
}

fn rule_row(spec: &RuleSpec, data: &serde_json::Value) -> RuleRow {
    let value = &data[&spec.key];
    let checked = value.as_bool().unwrap_or(false);
    RuleRow {
        key: spec.key.clone().into(),
        title: spec.title.clone().into(),
        hint: hint_for(spec, value).into(),
        kind: if spec.kind == "bool" {
            0
        } else if spec.kind == "choice" {
            1
        } else {
            2
        },
        checked,
        index: i32::try_from(
            spec.choices
                .iter()
                .position(|c| Some(c[0].as_str()) == value.as_str())
                .unwrap_or(0),
        )
        .unwrap_or(0),
        options: Rc::new(VecModel::from(
            spec.choices
                .iter()
                .map(|c| SharedString::from(c[1].as_str()))
                .collect::<Vec<_>>(),
        ))
        .into(),
        value: value
            .as_str()
            .map_or_else(|| value.to_string(), str::to_owned)
            .into(),
    }
}

/// 解析规则数字输入。附录 D：计数/比例项只接受非负十进制整数；容量项（capacity）
/// 额外接受可选的 B/KiB/MiB/GiB/TiB 单位（不区分大小写），换算后必须为整数字节，
/// 否则按非法输入处理（保留输入并阻止开始，不自动截断）。全程整数运算，不做浮点换算。
pub(super) fn parse_capacity(value: &str, capacity: bool) -> Result<u64, ()> {
    if !capacity {
        return value.parse::<u64>().map_err(|_| ());
    }
    let text = value.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let multiplier: u64 = match unit.trim().to_lowercase().as_str() {
        "" | "b" => 1,
        "kib" => 1 << 10,
        "mib" => 1 << 20,
        "gib" => 1 << 30,
        "tib" => 1 << 40,
        _ => return Err(()),
    };
    let (int_part, frac_part) = match number.split_once('.') {
        Some((whole, frac)) => (whole, frac),
        None => (number, ""),
    };
    let digits_ok = |text: &str| text.bytes().all(|b| b.is_ascii_digit());
    if !digits_ok(int_part) || !digits_ok(frac_part) {
        return Err(());
    }
    if int_part.is_empty() && frac_part.is_empty() {
        return Err(());
    }
    let whole: u64 = if int_part.is_empty() {
        0
    } else {
        int_part.parse().map_err(|_| ())?
    };
    let total = whole.checked_mul(multiplier).ok_or(())?;
    // 从最低小数位向前做十进制乘法；每一位的余数都须为 0，最终才是整数字节。
    // carry 始终小于 multiplier，避免合法 MiB/GiB/TiB 小数的分子或 10^位数溢出。
    let mut fractional_bytes = 0u64;
    for digit in frac_part.bytes().rev() {
        let scaled = u64::from(digit - b'0') * multiplier + fractional_bytes;
        if !scaled.is_multiple_of(10) {
            return Err(());
        }
        fractional_bytes = scaled / 10;
    }
    total.checked_add(fractional_bytes).ok_or(())
}

/// 规则行是否显示：高级层默认隐藏；联动项在依赖未开启且自身仍是默认值时不显示。
/// 已经改过值的行必须保留可见，否则依赖关掉后用户既看不到该行、也无法把它改回去。
fn rule_visible(spec: &RuleSpec, config: &Config, show_advanced: bool) -> bool {
    if !show_advanced && spec.tier == Tier::Advanced {
        return false;
    }
    match spec.key.as_str() {
        "large_threshold_bytes" => {
            config.large_files || config.large_threshold_bytes != 1024 * 1024 * 1024
        }
        // C-08：各清理类别的删除方式只在对应清理开启时才有意义；已经改过值的行保留可见，
        // 否则类别关掉后用户既看不到该行、也无法把它改回默认。
        "junk_delete" => config.clean_junk || config.junk_delete != DeleteChoice::Global,
        "temp_delete" => config.clean_temp || config.temp_delete != DeleteChoice::Global,
        "zero_delete" => config.clean_zero || config.zero_delete != DeleteChoice::Global,
        _ => true,
    }
}

fn visible_rows(state: &State) -> Result<Vec<RuleRow>> {
    let data = serde_json::to_value(&state.config)?;
    let tool = match state.tool {
        Tool::Extract => "extract",
        Tool::Organizer => "organizer",
        // MD/Git 不设规则面板（R-01）：不会进入规则表过滤，占位即可。
        Tool::Md
        | Tool::Git
        | Tool::MarkdownConverter
        | Tool::SnapOcr
        | Tool::AcpHttp
        | Tool::CodeAgent => "",
    };
    Ok(state
        .specs
        .iter()
        .filter(|s| {
            s.section == state.section
                && s.tools.iter().any(|t| t == tool)
                && rule_visible(s, &state.config, state.show_advanced)
        })
        .map(|spec| {
            let mut row = rule_row(spec, &data);
            if let Some(draft) = state.invalid_rule_inputs.get(&spec.key) {
                row.value = draft.as_str().into();
            }
            row
        })
        .collect())
}

fn rule_rows(state: &State) -> Result<ModelRc<RuleRow>> {
    Ok(Rc::new(VecModel::from(visible_rows(state)?)).into())
}

pub(super) fn refresh(ui: &AppWindow, state: &State) {
    if let Ok(rows) = rule_rows(state) {
        ui.set_rules(rows);
    }
    ui.set_theme(match state.config.theme.as_str() {
        "light" => 1,
        "dark" => 2,
        _ => 0,
    });
}

/// 只改单行显示，避免整表重建打断 ComboBox/CheckBox（用户点选后文字停在旧值的根因）。
pub(super) fn patch_rule_row(ui: &AppWindow, key: &str, mutate: impl Fn(&mut RuleRow)) {
    let rules = ui.get_rules();
    let Some(model) = rules.as_any().downcast_ref::<VecModel<RuleRow>>() else {
        return;
    };
    for i in 0..model.row_count() {
        if let Some(mut row) = model.row_data(i) {
            if row.key.as_str() == key {
                mutate(&mut row);
                model.set_row_data(i, row);
                break;
            }
        }
    }
}

/// 只插入/删除可见性变化的行，不重建整表：整表重建会销毁 ListView 里的控件，
/// 在回调中执行会让刚点过的 ComboBox 文字停在旧值（见 on_rule_bool 上的原注释）。
pub(super) fn sync_rules(ui: &AppWindow, state: &State) {
    let Ok(desired) = visible_rows(state) else {
        return;
    };
    let rules = ui.get_rules();
    let Some(model) = rules.as_any().downcast_ref::<VecModel<RuleRow>>() else {
        return;
    };
    let mut index = 0usize;
    for row in desired {
        loop {
            match model.row_data(index) {
                None => {
                    model.push(row.clone());
                    break;
                }
                Some(current) if current.key == row.key => break,
                Some(_) => {
                    if model
                        .row_data(index + 1)
                        .is_some_and(|next| next.key == row.key)
                    {
                        model.remove(index);
                        continue;
                    }
                    model.insert(index, row.clone());
                    break;
                }
            }
        }
        index += 1;
    }
    while model.row_count() > index {
        model.remove(index);
    }
}

/// 合并行的影子键：界面上一个开关同时驱动多个配置键，Config 保留细粒度字段
/// 供引擎与历史任务库继续使用，只是界面不再拆开展示。
fn group_keys(key: &str) -> Vec<&str> {
    match key {
        "fix_extension" => vec!["fix_extension", "detect_type"],
        _ => vec![key],
    }
}

pub(super) fn changed(
    ui: &AppWindow,
    state: &Rc<RefCell<State>>,
    key: &str,
    value: &serde_json::Value,
    rebuild: bool,
) -> bool {
    let tool = state.borrow().tool;
    let mut state = state.borrow_mut();
    let result = (|| {
        for target in group_keys(key) {
            state.config.set_json(target, value.clone())?;
        }
        Ok::<_, anyhow::Error>(())
    })();
    match result {
        Ok(()) => {
            // 立刻反馈规则之间的依赖（例如“修正扩展名”需要先开“检测真实类型”），
            // 不要让用户等到点「开始分析 / 开始解压」才知道配置不成立。
            let validation = match tool {
                Tool::Organizer => state.config.validate_organizer(),
                Tool::Extract => state.config.validate_extract(),
                // 主题设置不属于任一工具的规则面板。
                Tool::Md
                | Tool::Git
                | Tool::MarkdownConverter
                | Tool::SnapOcr
                | Tool::AcpHttp
                | Tool::CodeAgent => Ok(()),
            };
            let validation = invalid_number_error(&state, tool)
                .map_or(validation, |error| Err(anyhow::anyhow!(error)));
            match validation {
                Ok(()) => ui.set_error_text("".into()),
                Err(error) => ui.set_error_text(format!("{error:#}").into()),
            }
            // theme 是纯外观设置，不影响计划内容：不使已生成的计划失效。
            if key != "theme" {
                state.extract_generation += 1;
                if ui.get_confirm_kind() == 1 {
                    ui.set_confirm_kind(0);
                    ui.set_confirm_pending(false);
                }
                refresh_scope_notice(ui, &state.config);
                invalidate(ui, &state);
            }
            if rebuild {
                refresh(ui, &state);
            } else if key == "theme" {
                apply_theme(ui, &state);
            }
            true
        }
        Err(error) => {
            ui.set_error_text(format!("{error:#}").into());
            false
        }
    }
}
