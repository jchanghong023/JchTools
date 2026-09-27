"""覆盖 O-11：真实桌面主窗口可连接已运行的 OCR worker."""

from __future__ import annotations

import argparse
import time
from typing import cast

from pywinauto import Application


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    _ = parser.add_argument("--pid", type=int, required=True)
    args = parser.parse_args()
    window = Application(backend="uia").connect(process=cast("int", args.pid)).window(title="JchTools")
    reconnect = next(
        button
        for button in window.descendants(control_type="Button")
        if button.window_text() in ("启动 / 重连", "刷新连接")
    )
    _ = window.set_focus()
    reconnect.click_input()
    deadline = time.monotonic() + 10
    labels: list[str] = []
    while time.monotonic() < deadline:
        labels = [str(item.window_text() or "") for item in window.descendants(control_type="Text")]
        if "截图服务已连接 · 托盘和热键独立运行" in labels and "模型：就绪" in labels:
            print("PASS 真实主窗口已连接 worker 且模型 ready")
            return 0
        time.sleep(0.2)
    print("FAIL 真实主窗口未连接 worker：")
    print("\n".join(text for text in labels if "截图服务" in text or "模型：" in text or "管道" in text))
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
