"""覆盖 O-11：真实桌面主窗口可自动连接已运行的 OCR worker.

旧版脚本点击「启动 / 重连」「刷新连接」并期望「截图服务已连接 · 托盘和热键
独立运行」；这些按钮与文案已随 XB-20~XB-23 的后台自动连接改版移除
（commit 5d5e509，现为 ui/app.slint 的「重试后台连接」按钮与自动连接）。
现行为：打开主窗口或切到截图 OCR 页即自动连接后台，页面显示
「后台已连接 · 托盘和热键独立运行」与「模型：就绪」。本脚本不点击任何
连接按钮（自动连接本身即被测行为），并断言旧按钮与旧文案不在场。
"""

from __future__ import annotations

import argparse
import time
from typing import cast

from pywinauto import Application

CONNECTED_STATUS = "后台已连接 · 托盘和热键独立运行"
MODEL_READY = "模型：就绪"
TOOL_NAV = "截图 OCR"
REMOVED_BUTTONS = ("启动 / 重连", "刷新连接")
REMOVED_STATUS = "截图服务已连接"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    _ = parser.add_argument("--pid", type=int, required=True)
    _ = parser.add_argument("--timeout", type=int, default=30, help="等待自动连接与模型就绪的秒数")
    args = parser.parse_args()
    window = Application(backend="uia").connect(process=cast("int", args.pid)).window(title="JchTools")

    def texts() -> list[str]:
        return [str(item.window_text() or "") for item in window.descendants(control_type="Text")]

    def buttons() -> list[str]:
        return [str(item.window_text() or "") for item in window.descendants(control_type="Button")]

    # 旧按钮与旧文案必须不在场：脚本期望与当前 UI 保持一致（切换页面前先查一次）。
    for label in (*REMOVED_BUTTONS, REMOVED_STATUS):
        if label in buttons() or label in texts():
            print(f"FAIL 旧 UI 元素不应存在：{label}")
            return 1

    # 切到「截图 OCR」页：侧栏入口触发自动连接链路，不点任何连接按钮。
    nav = next(
        (button for button in window.descendants(control_type="Button") if button.window_text() == TOOL_NAV),
        None,
    )
    if nav is None:
        print(f"FAIL 未找到侧栏「{TOOL_NAV}」入口")
        return 1
    _ = window.set_focus()
    nav.click_input()

    deadline = time.monotonic() + cast("int", args.timeout)
    labels: list[str] = []
    while time.monotonic() < deadline:
        labels = texts()
        if CONNECTED_STATUS in labels and MODEL_READY in labels:
            # 到场后复核旧按钮未随页面切换重新出现（其旧位置即本页）。
            stale = [label for label in REMOVED_BUTTONS if label in buttons()]
            if stale:
                print(f"FAIL 旧连接按钮不应存在：{stale}")
                return 1
            print("PASS 真实主窗口已自动连接 worker 且模型 ready")
            return 0
        time.sleep(0.2)
    print("FAIL 真实主窗口未自动连接 worker：")
    print("\n".join(text for text in labels if "后台" in text or "模型：" in text or "服务" in text))
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
