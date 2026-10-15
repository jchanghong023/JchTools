"""覆盖 O-11/P-11：连接验收必须拒绝切页前后的旧状态完整文案。"""

from __future__ import annotations

import unittest
from unittest.mock import Mock, patch

import check_gui_worker_connection as checker


class GuiWorkerConnectionTests(unittest.TestCase):
    def run_check(self, *, initial_texts: list[str], connected_texts: list[str]) -> tuple[int, Mock, Mock]:
        nav = Mock()
        nav.window_text.return_value = checker.TOOL_NAV
        window = Mock()
        navigated = False

        def invoke() -> None:
            nonlocal navigated
            navigated = True

        def descendants(*, control_type: str) -> list[Mock]:
            if control_type == "Button":
                return [nav]
            texts = connected_texts if navigated else initial_texts
            return [Mock(window_text=Mock(return_value=text)) for text in texts]

        nav.invoke.side_effect = invoke
        window.descendants.side_effect = descendants
        application = Mock()
        application.connect.return_value.window.return_value = window
        with patch.object(checker, "Application", return_value=application):
            result = checker.check_connection(123, 1)
        return result, nav, window

    def test_removed_status_prefix_is_rejected_before_navigation(self) -> None:
        result, nav, _ = self.run_check(
            initial_texts=["截图服务已连接 · 托盘和热键独立运行"],
            connected_texts=[checker.CONNECTED_STATUS, checker.MODEL_READY],
        )
        self.assertEqual(result, 1)
        nav.invoke.assert_not_called()
        nav.click_input.assert_not_called()

    def test_removed_status_is_rejected_after_navigation(self) -> None:
        result, _, _ = self.run_check(
            initial_texts=[],
            connected_texts=[checker.CONNECTED_STATUS, checker.MODEL_READY, "截图服务已连接 · 托盘和热键独立运行"],
        )
        self.assertEqual(result, 1)


if __name__ == "__main__":
    unittest.main()
