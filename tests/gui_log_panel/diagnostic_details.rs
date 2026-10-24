use super::*;

struct DiagnosticConsumer {
    screen: i32,
    name: &'static str,
    page_id: &'static str,
    detail_id: &'static str,
    text_id: &'static str,
    diagnostic_property: &'static str,
    detail_open_property: Option<&'static str>,
    anchor_label: &'static str,
    anchor_type: &'static str,
}

const DIAGNOSTIC_CONSUMERS: [DiagnosticConsumer; 5] = [
    DiagnosticConsumer {
        screen: 7,
        name: "settings",
        page_id: "AppWindow::settings-scroll",
        detail_id: "SettingsPage::snap-settings-detail",
        text_id: "SettingsPage::snap-settings-detail-text",
        diagnostic_property: "snap-error",
        detail_open_property: None,
        anchor_label: "检查 / 修复组件",
        anchor_type: "AppButton",
    },
    DiagnosticConsumer {
        screen: 6,
        name: "snapshot",
        page_id: "AppWindow::snap-page-scroll",
        detail_id: "SnapPage::snap-page-detail",
        text_id: "SnapPage::snap-page-detail-text",
        diagnostic_property: "snap-error",
        detail_open_property: Some("snap-detail-open"),
        anchor_label: "前往设置",
        anchor_type: "AppButton",
    },
    DiagnosticConsumer {
        screen: 5,
        name: "convert",
        page_id: "ConvertPage::converter-scroll",
        detail_id: "ConvertPage::convert-detail-scroll",
        text_id: "ConvertPage::convert-detail-text",
        diagnostic_property: "convert-detail-text",
        detail_open_property: Some("convert-detail-open"),
        anchor_label: "截图与文档组件随包提供；音频资产在设置中按需下载，故障原因见详情。",
        anchor_type: "Muted",
    },
    DiagnosticConsumer {
        screen: 8,
        name: "acp-service",
        page_id: "AppWindow::acp-scroll",
        detail_id: "AcpPage::acp-error-scroll",
        text_id: "AcpPage::acp-error-text",
        diagnostic_property: "acp-service-error",
        detail_open_property: None,
        anchor_label: "退出模型服务",
        anchor_type: "AppButton",
    },
    DiagnosticConsumer {
        screen: 8,
        name: "acp-action",
        page_id: "AppWindow::acp-scroll",
        detail_id: "AcpPage::acp-error-scroll",
        text_id: "AcpPage::acp-error-text",
        diagnostic_property: "acp-action-error",
        detail_open_property: None,
        anchor_label: "退出模型服务",
        anchor_type: "AppButton",
    },
];

impl DiagnosticConsumer {
    fn show(&self, view: &mut View, diagnostic: &str) -> String {
        // 同页的两类 ACP 错误也从全新阅读位置开始；仅注入界面状态，不调用服务。
        view.set("screen", 0);
        view.render();
        view.set("acp-service-error", Value::String("".into()));
        view.set("acp-action-error", Value::String("".into()));
        view.set(self.diagnostic_property, Value::String(diagnostic.into()));
        if let Some(property) = self.detail_open_property {
            view.set(property, true);
        }
        view.set("screen", self.screen);
        view.render();
        if self.diagnostic_property == "acp-action-error" {
            format!("操作错误：{diagnostic}\n")
        } else {
            diagnostic.to_owned()
        }
    }
}

/// U-05：短诊断与后续恢复操作之间不应预留一个多行滚动框的空白。
#[test]
fn diagnostic_details_short_messages_do_not_reserve_empty_multiline_space() {
    with_review_remediation_view(|view| {
        let diagnostic = "诊断样本：组件资产暂不可用。";
        // 各消费者使用同一真实阅读区与操作锚点检查。
        for (width, height) in [(960, 620), (1120, 720)] {
            view.window.set_size(PhysicalSize::new(width, height));
            view.pixels = SharedPixelBuffer::new(width, height);
            for (theme, theme_name) in [(1, "light"), (2, "dark")] {
                view.set("theme", theme);
                for consumer in &DIAGNOSTIC_CONSUMERS {
                    let expected = consumer.show(view, diagnostic);
                    let (text, recovery) = reveal_diagnostic_recovery(view, consumer);
                    let context = format!(
                        "diagnostic-short-{}-{width}x{height}-{theme_name}",
                        consumer.name
                    );
                    view.screenshot(&context);
                    let viewport = diagnostic_area(view, consumer.detail_id);
                    let viewport_position = viewport.absolute_position();
                    let viewport_size = viewport.size();
                    assert!(
                        viewport_size.height >= text.size().height - 0.5
                            && viewport_size.height <= text.size().height * 2.0 + 20.0
                            && (text.absolute_position().y - viewport_position.y).abs() <= 0.5,
                        "{context}：短诊断的实际阅读区应随正文收缩，不得保留长诊断的固定阅读高度；正文 {:?}，阅读区 {viewport_size:?}",
                        text.size()
                    );
                    let gap = recovery.absolute_position().y
                        - (text.absolute_position().y + text.size().height);
                    assert!(
                        gap >= -0.5 && gap <= text.size().height * 2.0 + 20.0,
                        "{context}：单行诊断之后应紧接恢复操作，不得保留多行空白；正文高度 {}，间距 {gap}",
                        text.size().height
                    );
                    assert_eq!(text.accessible_label().as_deref(), Some(expected.as_str()));
                }
            }
        }
    });
}

/// U-05：长诊断保留完整内容，限制阅读区高度，通过真实滚轮读到末尾。
#[test]
fn diagnostic_details_long_messages_are_bounded_and_scroll_to_the_end() {
    with_review_remediation_view(|view| {
        let diagnostic = (0..48)
            .map(|line| format!("诊断记录 {line:02}：D:/测试资料/组件/模型.bin；保留定位上下文。"))
            .collect::<Vec<_>>()
            .join("\n");
        // 服务错误与操作错误分别保留原文，包括操作错误前缀和末尾换行。
        for (width, height) in [(960, 620), (1120, 720)] {
            view.window.set_size(PhysicalSize::new(width, height));
            view.pixels = SharedPixelBuffer::new(width, height);
            for (theme, theme_name) in [(1, "light"), (2, "dark")] {
                view.set("theme", theme);
                for consumer in &DIAGNOSTIC_CONSUMERS {
                    let expected = consumer.show(view, &diagnostic);
                    let (text, recovery) = reveal_diagnostic_recovery(view, consumer);
                    let context = format!(
                        "diagnostic-long-{}-{width}x{height}-{theme_name}",
                        consumer.name
                    );
                    view.screenshot(&format!("{context}-top"));
                    let text_size = text.size();
                    let recovery_position = recovery.absolute_position();
                    let viewport = diagnostic_area(view, consumer.detail_id);
                    let viewport_position = viewport.absolute_position();
                    let viewport_size = viewport.size();
                    let viewport_bottom = viewport_position.y + viewport_size.height;
                    let reading_height = viewport_size.height;
                    assert!(
                        text_size.height > f32::from(u16::try_from(height).unwrap())
                            && reading_height >= 2.0 * 13.0
                            && reading_height
                                <= f32::from(u16::try_from(height).unwrap()) / 3.0,
                        "{context}：长诊断应保留完整正文且阅读区封顶，不能将恢复操作推出窗口；正文 {text_size:?}，阅读区 {reading_height}"
                    );
                    let pointer = LogicalPosition::new(
                        viewport_position.x + viewport_size.width / 2.0,
                        viewport_position.y + viewport_size.height / 2.0,
                    );
                    for _ in 0..32 {
                        let remaining =
                            text.absolute_position().y + text.size().height - viewport_bottom;
                        if remaining <= 0.5 {
                            break;
                        }
                        let before = text.absolute_position().y;
                        view.ui
                            .window()
                            .dispatch_event(WindowEvent::PointerScrolled {
                                position: pointer,
                                delta_x: 0.0,
                                // 最后一步只请求尚未进入视口的距离，避免越过正文末尾后
                                // 把剩余滚轮量正常冒泡给外层页面。
                                delta_y: -remaining.min(viewport_size.height / 2.0),
                            });
                        view.render();
                        assert!(
                            (recovery.absolute_position().y - recovery_position.y).abs() <= 0.5,
                            "{context}：尚有未读正文时只能滚动内部阅读区，恢复操作应保持原位；原位置 {recovery_position:?}，实际位置 {:?}，剩余 {remaining}，视口 {viewport_position:?}/{viewport_size:?}",
                            recovery.absolute_position()
                        );
                        assert!(
                            text.absolute_position().y < before - 0.5,
                            "{context}：未读到末尾前诊断正文必须响应滚轮"
                        );
                    }
                    let bottom = text.absolute_position().y + text.size().height;
                    assert!(
                        bottom <= viewport_bottom + 0.5 && bottom >= viewport_position.y + 13.0,
                        "{context}：完整诊断末尾必须能滚入阅读区，实际底部 {bottom}"
                    );
                    assert_eq!(
                        text.accessible_label().as_deref(),
                        Some(expected.as_str()),
                        "{context}：滚动不能截断、替换或丢失诊断内容"
                    );
                    assert_eq!(text.size(), text_size);
                    view.screenshot(&format!("{context}-end"));
                }
            }
        }
    });
}

#[track_caller]
fn reveal_diagnostic_recovery(
    view: &mut View,
    consumer: &DiagnosticConsumer,
) -> (ElementHandle, ElementHandle) {
    let page = diagnostic_area(view, consumer.page_id);
    let page_position = page.absolute_position();
    let page_size = page.size();
    for _ in 0..24 {
        let text = diagnostic_element(view, consumer.text_id);
        let viewport = diagnostic_element(view, consumer.detail_id);
        let recovery = page
            .query_descendants()
            .match_type_name(consumer.anchor_type)
            .match_predicate({
                let label = consumer.anchor_label;
                move |element| element.accessible_label().as_deref() == Some(label)
            })
            .find_first();
        if let (Some(text), Some(viewport), Some(recovery)) = (text, viewport, recovery) {
            let viewport_position = viewport.absolute_position();
            let viewport_size = viewport.size();
            let recovery_position = recovery.absolute_position();
            let recovery_size = recovery.size();
            if viewport_position.y >= page_position.y
                && text.absolute_position().y >= viewport_position.y - 0.5
                && text.absolute_position().y < recovery_position.y
                && viewport_position.y + viewport_size.height <= recovery_position.y + 0.5
                && viewport_position.y + viewport_size.height
                    <= page_position.y + page_size.height + 0.5
                && viewport_position.x >= page_position.x
                && viewport_position.x + viewport_size.width
                    <= page_position.x + page_size.width + 0.5
                && recovery_position.y + recovery_size.height
                    <= page_position.y + page_size.height + 0.5
                && recovery_position.x >= page_position.x
                && recovery_position.x + recovery_size.width
                    <= page_position.x + page_size.width + 0.5
            {
                return (text, recovery);
            }
        }
        // 卡片右侧内边距属于外层页面，不把“寻找诊断”误变为滚动诊断正文。
        view.ui
            .window()
            .dispatch_event(WindowEvent::PointerScrolled {
                position: LogicalPosition::new(
                    page_position.x + page_size.width - 12.0,
                    page_position.y + page_size.height / 2.0,
                ),
                delta_x: 0.0,
                delta_y: -60.0,
            });
        view.render();
    }
    panic!(
        "滚动整页后诊断阅读区和后续操作必须同时位于窗口内：screen={:?}，窗口 {}x{}",
        view.ui.get_property("screen"),
        view.pixels.width(),
        view.pixels.height()
    );
}

#[track_caller]
fn diagnostic_area(view: &View, id: &str) -> ElementHandle {
    diagnostic_element(view, id).expect("诊断阅读区应具有可查询的真实布局")
}

fn diagnostic_element(view: &View, id: &str) -> Option<ElementHandle> {
    // find_by_element_id/label 内部先 find_all，连 .next() 也遍历全部内置节点。
    // 精确限定全限定 id 并 find_first，命中后即停止，避免无关节点的调试元数据警告。
    ElementQuery::from_root(&view.ui).match_id(id).find_first()
}
