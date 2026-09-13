#!/usr/bin/env python3
"""JchTools GUI OS 级冒烟测试（pywinauto / UIA）。

三条关键路径冒烟：
  S1 启动并正常退出；
  S2 选择目录 → 解压与分析 → 计划生成（不执行）；
  S3 全链路：解压与分析 → 确认执行 → 整理完成。

用法：
    python scripts/gui_smoke.py --exe target/debug/JchTools.exe --data <已生成的测试数据目录>

依赖：pip install pywinauto（需要可交互桌面会话）。
"""
from __future__ import annotations
import argparse
import os
import subprocess
import sys
import time
from pathlib import Path

from pywinauto import Application, Desktop

TIMEOUT = 60


def wait_window(pid: int, timeout: int = TIMEOUT):
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        try:
            app = Application(backend="uia").connect(process=pid, timeout=1)
            window = app.window(title="JchTools")
            if window.exists():
                return app, window
        except Exception as exc:  # noqa: BLE001
            last = exc
        time.sleep(0.5)
    raise RuntimeError(f"等待 JchTools 窗口超时：{last}")


def find_button(window, title: str):
    return window.child_window(title=title, control_type="Button")


def wait_text(window, pattern: str, timeout: int = TIMEOUT):
    deadline = time.time() + timeout
    while time.time() < deadline:
        for text in window.descendants(control_type="Text"):
            if pattern in (text.window_text() or ""):
                return text
        time.sleep(0.5)
    raise RuntimeError(f"等待界面文本超时：{pattern}")


def set_directory(window, path: str):
    # 目录输入框是与「选择目录…」按钮同排的 Edit 控件。
    button = find_button(window, "选择目录…")
    button_rect = button.rectangle()
    candidates = [
        edit for edit in window.descendants(control_type="Edit")
        if abs(edit.rectangle().top - button_rect.top) < 20
    ]
    if not candidates:
        raise RuntimeError("未找到目录输入框")
    edit = min(candidates, key=lambda e: e.rectangle().left)
    edit.set_edit_text(path)


def close_app(window):
    close = find_button(window, "关闭")
    if close.exists():
        close.click_input()


def _wait_exit_or_kill(proc: subprocess.Popen, timeout: int = 15) -> None:
    """等待进程退出；超时则强制结束，避免异常路径泄漏进程。"""
    try:
        proc.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=10)


def s1_launch_and_exit(exe: str):
    proc = subprocess.Popen([exe])
    window = None
    try:
        _, window = wait_window(proc.pid)
        print("S1 PASS：窗口启动并可见")
    finally:
        if window is not None:
            try:
                close_app(window)
            except Exception:
                pass
        _wait_exit_or_kill(proc)
    print("S1 PASS：进程已退出")


def s2_analyze_only(exe: str, data: str):
    proc = subprocess.Popen([exe])
    window = None
    try:
        _, window = wait_window(proc.pid)
        set_directory(window, data)
        find_button(window, "开始解压与分析").click_input()
        checkbox = window.child_window(title="我已确认目录、规则及可能的永久删除行为", control_type="CheckBox")
        checkbox.wait("visible", timeout=TIMEOUT).click_input()
        find_button(window, "确认").click_input()
        wait_text(window, "解压与分析完成")
        print("S2 PASS：解压与分析完成")
    finally:
        if window is not None:
            try:
                close_app(window)
            except Exception:
                pass
        _wait_exit_or_kill(proc)
    print("S2 PASS：进程已退出")


def s3_full_organize(exe: str, data: str):
    proc = subprocess.Popen([exe])
    window = None
    try:
        _, window = wait_window(proc.pid)
        set_directory(window, data)
        find_button(window, "开始解压与分析").click_input()
        checkbox = window.child_window(title="我已确认目录、规则及可能的永久删除行为", control_type="CheckBox")
        checkbox.wait("visible", timeout=TIMEOUT).click_input()
        find_button(window, "确认").click_input()
        wait_text(window, "解压与分析完成")
        find_button(window, "确认并执行整理").click_input()
        checkbox2 = window.child_window(title="我已确认目录、规则及可能的永久删除行为", control_type="CheckBox")
        checkbox2.wait("visible", timeout=TIMEOUT).click_input()
        find_button(window, "确认").click_input()
        wait_text(window, "整理结束")
        print("S3 PASS：全链路整理完成")
    finally:
        if window is not None:
            try:
                close_app(window)
            except Exception:
                pass
        _wait_exit_or_kill(proc)
    print("S3 PASS：进程已退出")


def main() -> int:
    parser = argparse.ArgumentParser(description="JchTools GUI 冒烟测试")
    parser.add_argument("--exe", default="target/debug/JchTools.exe")
    parser.add_argument("--data", default=".tmp/gui-smoke/data")
    args = parser.parse_args()
    exe = Path(args.exe).resolve()
    data = Path(args.data).resolve()
    if not exe.is_file():
        raise RuntimeError(f"找不到 {exe}")
    if not data.is_dir():
        raise RuntimeError(f"找不到测试数据目录 {data}")
    # S3 会真实执行整理：只允许对一次性测试副本操作。
    # 仓库内仅放行 .tmp/（默认 .tmp/gui-smoke/data 就在这里）；其它路径拒绝。
    repo = Path(__file__).resolve().parent.parent
    home = Path.home().resolve()
    under_repo_tmp = repo / ".tmp" in (data, *data.parents)
    if (data == repo or repo in data.parents) and not under_repo_tmp:
        raise RuntimeError("拒绝在仓库目录内执行 GUI 整理冒烟（请使用 .tmp/gui-smoke/data 或其它副本）")
    if not under_repo_tmp and (data == home or home in data.parents):
        raise RuntimeError("拒绝在用户主目录内执行 GUI 整理冒烟（请使用一次性测试副本）")
    # 盘符根：parts 只有 ('D:\\',) 一层；'D:\\foo' 是 2 层，不得误杀。
    if data.drive and len(data.parts) <= 1:
        raise RuntimeError(f"拒绝在盘符根目录执行整理冒烟：{data}")

    s1_launch_and_exit(str(exe))
    # S2 分析会真实解压改写语料；S3 前从旁路副本恢复，保证“干净语料上的完整链路”。
    import shutil, tempfile
    scratch = Path(tempfile.mkdtemp(prefix="jchtools-gui-smoke-", dir=str(repo / ".tmp" if (repo / ".tmp").is_dir() else None)))
    try:
        fresh = scratch / "data"
        shutil.copytree(data, fresh)
        s2_analyze_only(str(exe), str(fresh))
        shutil.rmtree(fresh, ignore_errors=True)
        shutil.copytree(data, fresh)
        s3_full_organize(str(exe), str(fresh))
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
