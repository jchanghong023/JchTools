use super::{
    error, ModelDescriptor, ServiceError, ServiceErrorKind, SessionConfigId, SessionConfigKind,
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
    SessionConfigSelectOptions,
};
use std::sync::{Arc, Mutex, PoisonError};

#[derive(Clone)]
pub(super) struct ModelSelector {
    pub(super) id: SessionConfigId,
    pub(super) values: Vec<ModelDescriptor>,
    pub(super) current: String,
}
impl ModelSelector {
    // 请求模型是否在此 Agent 公布的可选择列表内。
    pub(super) fn allows(&self, model_id: &str) -> bool {
        self.values.iter().any(|model| model.id == model_id)
    }
}
pub(super) type SessionModel = Arc<Mutex<Result<ModelSelector, ServiceError>>>;
// 锁层模型可选性判定：SessionModel 是 type alias，不能对其直接 impl 固有方法。
pub(super) fn session_model_allows(
    model: &SessionModel,
    model_id: &str,
) -> Result<bool, ServiceError> {
    model
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(|selector| selector.allows(model_id))
        .map_err(Clone::clone)
}
pub(super) fn model_selector(
    options: &[SessionConfigOption],
    negotiated_id: Option<&SessionConfigId>,
) -> Result<ModelSelector, ServiceError> {
    let option = options
        .iter()
        .find(|option| {
            option.category == Some(SessionConfigOptionCategory::Model)
                && matches!(option.kind, SessionConfigKind::Select(_))
        })
        // category 是 UX 元数据；新会话仍可使用此连接已协商的 selector 身份。
        // 显式 Model 配置优先，允许不同会话协商不同 ID；不猜任意 Select 的用途。
        .or_else(|| negotiated_id.and_then(|id| options.iter().find(|option| &option.id == id)))
        .ok_or_else(|| {
            error(
                ServiceErrorKind::ModelsUnavailable,
                "Agent 未提供可选择的 Model/Select 配置能力",
            )
        })?;
    model_selector_from_option(option)
}
pub(super) fn model_selector_from_option(
    option: &SessionConfigOption,
) -> Result<ModelSelector, ServiceError> {
    let SessionConfigKind::Select(select) = &option.kind else {
        return Err(error(
            ServiceErrorKind::ModelsUnavailable,
            "Agent 模型配置类型不可用",
        ));
    };
    let mut values = Vec::new();
    let mut append = |value: &SessionConfigSelectOption| {
        let id = value.value.to_string();
        if !id.is_empty() && !values.iter().any(|model: &ModelDescriptor| model.id == id) {
            values.push(ModelDescriptor {
                id,
                name: value.name.clone(),
            });
        }
    };
    match &select.options {
        SessionConfigSelectOptions::Ungrouped(options) => {
            for value in options {
                append(value);
            }
        }
        SessionConfigSelectOptions::Grouped(groups) => {
            for group in groups {
                for value in &group.options {
                    append(value);
                }
            }
        }
        _ => {
            return Err(error(
                ServiceErrorKind::ModelsUnavailable,
                "Agent 模型选项格式不受支持",
            ))
        }
    }
    let current = select.current_value.to_string();
    if values.is_empty() || !values.iter().any(|model| model.id == current) {
        return Err(error(
            ServiceErrorKind::ModelsUnavailable,
            "Agent 没有有效的可选择模型",
        ));
    }
    Ok(ModelSelector {
        id: option.id.clone(),
        values,
        current,
    })
}
