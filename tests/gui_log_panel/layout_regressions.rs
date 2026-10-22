use super::*;

/// U-05/U-10：空失败列表的介绍贴近卡片顶部，空余空间留在介绍与分页之间。
#[test]
fn extract_empty_failure_list_keeps_intro_adjacent_and_pagination_visible() {
    with_review_remediation_view(|view| {
        view.set("screen", 2);
        view.set("panel", 2);
        view.set("busy", false);
        view.set("fail-total", 0);
        view.set("fail-prev-enabled", false);
        view.set("fail-next-enabled", false);
        for (width, height) in [(960, 620), (1120, 720)] {
            view.window.set_size(PhysicalSize::new(width, height));
            view.pixels = SharedPixelBuffer::new(width, height);
            for (theme, theme_name) in [(1, "light"), (2, "dark")] {
                view.set("theme", theme);
                for (ready, state) in [(false, "unassociated"), (true, "empty")] {
                    view.set("fail-ready", ready);
                    view.render();
                    let context = format!("extract-failures-{state}-{width}x{height}-{theme_name}");
                    view.screenshot(&context);
                    // 文本只用于定位现有标题，不将提示措辞变成产品要求。
                    let title_label = "本次解压失败项（共 0 项）";
                    let title = ElementHandle::find_by_accessible_label(&view.ui, title_label)
                        .find(|element| element.size().width > 0.0 && element.size().height > 0.0)
                        .expect("空失败列表应保留真实标题");
                    let card = ElementHandle::find_by_element_type_name(&view.ui, "Surface")
                        .find(|element| {
                            element
                                .query_descendants()
                                .match_predicate(move |child| {
                                    child.accessible_label().as_deref() == Some(title_label)
                                })
                                .find_first()
                                .is_some()
                        })
                        .expect("失败列表标题应位于真实卡片中");
                    let previous = view.button("上一页");
                    let next = view.button("下一页");
                    let pagination_top = previous
                        .absolute_position()
                        .y
                        .min(next.absolute_position().y);
                    let title_position = title.absolute_position();
                    let title_size = title.size();
                    let hint = card
                        .query_descendants()
                        .match_inherits("Text")
                        .find_all()
                        .into_iter()
                        .filter(|element| {
                            let position = element.absolute_position();
                            (position.x - title_position.x).abs() <= 1.0
                                && position.y > title_position.y + 0.5
                                && position.y < pagination_top
                                && element.size().width > 0.0
                                && element.size().height > 0.0
                        })
                        .min_by(|first, second| {
                            first
                                .absolute_position()
                                .y
                                .total_cmp(&second.absolute_position().y)
                        })
                        .expect("空失败列表应在标题下保留真实状态提示");
                    let card_position = card.absolute_position();
                    let card_size = card.size();
                    let hint_position = hint.absolute_position();
                    let hint_size = hint.size();
                    let title_top_gap = title_position.y - card_position.y;
                    let hint_gap = hint_position.y - (title_position.y + title_size.height);
                    assert!(
                        title_top_gap >= 0.0
                            && title_top_gap <= 24.0
                            && title_size.height <= 30.0,
                        "{context}：标题应贴近卡片顶部且不得拉伸成空白块；卡片 {card_position:?}/{card_size:?}，标题 {title_position:?}/{title_size:?}"
                    );
                    assert!(
                        hint_gap >= -0.5 && hint_gap <= 16.0 && hint_size.height <= 40.0,
                        "{context}：空态提示应紧接标题，不得被卡片剩余空间垂直拉散；标题 {title_position:?}/{title_size:?}，提示 {hint_position:?}/{hint_size:?}，间距 {hint_gap}"
                    );
                    let window_width = f32::from(u16::try_from(width).unwrap());
                    let window_height = f32::from(u16::try_from(height).unwrap());
                    for button in [previous, next] {
                        let position = button.absolute_position();
                        let size = button.size();
                        let bottom = position.y + size.height;
                        let card_bottom_gap = card_position.y + card_size.height - bottom;
                        assert!(
                            position.x >= card_position.x
                                && position.x + size.width
                                    <= card_position.x + card_size.width + 0.5
                                && position.x + size.width <= window_width + 0.5
                                && position.y >= window_height / 2.0
                                && position.y >= hint_position.y + hint_size.height
                                && bottom <= window_height + 0.5
                                && card_bottom_gap >= -0.5
                                && card_bottom_gap <= 24.0,
                            "{context}：分页按钮应完整留在窗口下部、卡片底部，不能跟随介绍上浮或被裁掉；按钮 {position:?}/{size:?}，卡片底部间距 {card_bottom_gap}"
                        );
                    }
                }
            }
        }
    });
}

// 覆盖 U-05：规则行的启用勾选框必须保持键控自然宽度并右缘对齐成列。
// 修复前 Check 组件未声明 horizontal-stretch: 0，被 HorizontalLayout 连同
// 提示文字长度一起拉伸，同一列勾选框的横向位置随每行说明长度漂移。
#[test]
fn gui_findings_rule_check_column_keeps_natural_width_and_right_edge() {
    with_review_remediation_view(|view| {
        let rule = |key: &str, title: &str, hint: &str, checked: bool| {
            Value::Struct(
                [
                    ("key", Value::String(key.into())),
                    ("title", Value::String(title.into())),
                    ("hint", Value::String(hint.into())),
                    ("kind", Value::Number(0.0)),
                    ("checked", Value::Bool(checked)),
                    ("index", Value::Number(0.0)),
                    (
                        "options",
                        Value::Model(slint::ModelRc::new(slint::VecModel::from(
                            Vec::<Value>::new(),
                        ))),
                    ),
                    ("value", Value::String(String::new().into())),
                ]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
            )
        };
        // 三行说明长度差异显著：修复前勾选框宽度分别为 225 / 167 / 197.5px。
        view.set(
            "rules",
            Value::Model(slint::ModelRc::new(slint::VecModel::from(vec![
                rule("k1", "同名去重", "短说明", true),
                rule(
                    "k2",
                    "副本同名去重",
                    "名称仅差副本标记（如 [1]、「- Copy」「（副本）」）的同尺寸文件也视为重要候选参与内容去重。",
                    true,
                ),
                rule(
                    "k3",
                    "不同名去重",
                    "默认关闭。开启后同尺寸文件均成为候选，不再受名称限制；仍只按整文件哈希判定。",
                    false,
                ),
            ]))),
        );
        view.set("screen", 0);
        view.set("panel", 0);
        view.render();
        let checks: Vec<_> = ElementHandle::find_by_accessible_label(&view.ui, "启用")
            .filter(|element| {
                element.accessible_role() == Some(i_slint_backend_testing::AccessibleRole::Checkbox)
            })
            .filter(|element| element.size().width > 0.0)
            .collect();
        assert_eq!(
            checks.len(),
            3,
            "三条 kind=0 规则应各呈现一个启用勾选框：{:?}",
            checks
                .iter()
                .map(|e| (e.id(), e.size()))
                .collect::<Vec<_>>()
        );
        let widths: Vec<_> = checks.iter().map(|e| e.size().width).collect();
        let rights: Vec<_> = checks
            .iter()
            .map(|e| e.absolute_position().x + e.size().width)
            .collect();
        for pair in widths.windows(2) {
            assert!(
                (pair[0] - pair[1]).abs() <= 0.5,
                "启用勾选框不得随提示文字长度被布局拉伸：宽度 {widths:?}"
            );
        }
        for pair in rights.windows(2) {
            assert!(
                (pair[0] - pair[1]).abs() <= 0.5,
                "启用勾选框必须右缘对齐成列：右缘 {rights:?}"
            );
        }
    });
}

// 覆盖 U-05：键帽样式的热键展示必须保持键帽自然宽度，不得被布局拉成输入框。
#[test]
fn gui_findings_hotkey_keycap_keeps_natural_width() {
    with_review_remediation_view(|view| {
        view.set("screen", 6);
        view.render();
        // Kbd 内的键帽文字（24px 行高）与「新热键」草稿文字（非 24px 行高）同名，
        // 以行高区分后断言键帽文字未被拉伸成整行宽。
        let caps: Vec<_> = ElementHandle::find_by_accessible_label(&view.ui, "Ctrl+Alt+O")
            .filter(|element| (element.size().height - 24.0).abs() <= 0.5)
            .collect();
        assert_eq!(
            caps.len(),
            1,
            "当前生效热键应呈现一个键帽文字：{:?}",
            caps.iter().map(|e| (e.id(), e.size())).collect::<Vec<_>>()
        );
        let width = caps[0].size().width;
        assert!(
            width < 200.0,
            "热键键帽必须保持自然宽度，不得被布局横向拉伸：{width}px"
        );
    });
}
