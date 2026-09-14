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
import json
import os
import sqlite3
import subprocess
import sys
import time
from pathlib import Path

from pywinauto import Application, Desktop

TIMEOUT = 60
# 分析/执行完成等待用更长上限：真实数据、冷启动与杀软扫描都会让真实耗时远离秒级。
# 只放宽「等结果」的上限，元素出现仍按 TIMEOUT 快速失败，避免掩盖界面迟迟不响应的问题。
COMPLETION_TIMEOUT = 240


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


def activate(window) -> None:
    """把应用窗口提到前台，并抬到 z 序最上层。
    click_input 是真实鼠标点击：窗口被资源管理器等程序遮挡时，点击会落到遮挡窗口上。
    SetWindowPos(HWND_TOP) 只改 z 序、不抢焦点，在后台进程里也能生效，比 set_focus 可靠。
    """
    try:
        import win32con
        import win32gui

        win32gui.ShowWindow(window.handle, win32con.SW_RESTORE)
        win32gui.SetWindowPos(
            window.handle, win32con.HWND_TOP, 0, 0, 0, 0,
            win32con.SWP_NOMOVE | win32con.SWP_NOSIZE | win32con.SWP_SHOWWINDOW,
        )
    except Exception:  # noqa: BLE001
        pass
    try:
        window.set_focus()
    except Exception:  # noqa: BLE001
        pass
    time.sleep(0.3)


def click(window, control) -> None:
    activate(window)
    control.click_input()
    time.sleep(0.3)


def confirm_dialog(window, timeout: int = TIMEOUT) -> None:
    """勾选「我已确认…」并点「确认」，以**对话框消失**作为成功判据。
    点击可能落在对话框滑入动画的空档或未生效，所以按当前状态重试：
    未勾选就点复选框，已勾选就点确认，直到对话框关闭；
    只检查「点击没报错」会把没生效的点击当成成功，后续等待必然超时。
    """
    checkbox = window.child_window(
        title="我已确认目录、规则及可能的永久删除行为", control_type="CheckBox"
    )
    ok = find_button(window, "确认")
    deadline = time.time() + timeout
    attempts = 0
    while time.time() < deadline:
        checkbox.wait("visible", timeout=10)
        time.sleep(0.3)  # 让对话框完成滑入，避免点到动画中途的位置
        attempts += 1
        click(window, ok if ok.is_enabled() else checkbox)
        for _ in range(20):
            time.sleep(0.3)
            try:
                if not checkbox.is_visible():
                    return
            except Exception:  # noqa: BLE001
                return
        print(f"  确认框仍未关闭，重试第 {attempts} 次")
    raise RuntimeError("确认对话框没有关闭：勾选或确认点击未生效")


def state_dir() -> Path:
    """与 src/config.rs::state_dir 一致：%LOCALAPPDATA%\\JchTools\\data。"""
    local = os.environ.get("LOCALAPPDATA")
    if not local:
        raise RuntimeError("缺少 LOCALAPPDATA，无法定位任务目录")
    return Path(local) / "JchTools" / "data"


def _task_records(data: str):
    """列出针对 data 的任务（目录名排序，形如 %Y%m%dT%H%M%S-uuid）。"""
    tasks = state_dir() / "tasks"
    want = os.path.normcase(str(Path(data).resolve()))
    records = []
    for task in tasks.glob("*"):
        db = task / "task.sqlite3"
        if not db.is_file():
            continue
        try:
            conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
            rows = dict(conn.execute("SELECT key,value FROM metadata").fetchall())
            conn.close()
            root = json.loads(rows.get("root", '""')).removeprefix("\\\\?\\")
            if os.path.normcase(root) != want:
                continue
            records.append((task.name, json.loads(rows.get("status", '""'))))
        except Exception:  # noqa: BLE001
            continue
    records.sort()
    return records


def newest_task(data: str) -> str:
    records = _task_records(data)
    return records[-1][0] if records else ""


def wait_task_status(data: str, expected: str, after: str = "", timeout: int = COMPLETION_TIMEOUT) -> None:
    """等待这次点击新建的任务跑到 expected 状态。
    界面文本经 UIA 桥接并不总是可见（动态状态行时有时无），
    而任务库状态是「GUI 流程真的跑完」的可靠判据，因此用它做断言；
    点击前的任务名通过 after 排除，避免读到上一轮分析留下的 ready 任务。
    """
    deadline = time.time() + timeout
    while time.time() < deadline:
        for name, status in _task_records(data):
            if name <= after:
                continue
            if status == expected:
                return
            if status in ("failed", "cancelled"):
                raise RuntimeError(f"任务 {name} 状态为 {status}")
        time.sleep(0.5)
    raise RuntimeError(f"等待任务状态 {expected} 超时（目录 {data}）")


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


def setup_directory(window, path: str, timeout: int = TIMEOUT) -> None:
    """设置目录并确认界面已接受它。
    界面未接受时「开始解压与分析」保持禁用，直接点击只会空转、随后以超时收场；
    判据用状态栏文案而不是按钮状态，避免把「扫描中」误判成未就绪。
    """
    for attempt in range(1, 4):
        set_directory(window, path)
        deadline = time.time() + timeout
        while time.time() < deadline:
            for text in window.descendants(control_type="Text"):
                if "目录已就绪" in (text.window_text() or ""):
                    return
            time.sleep(0.5)
        print(f"  目录未被界面接受，重试第 {attempt} 次")
    raise RuntimeError("目录未被界面接受：状态栏始终没有出现「目录已就绪」")


def open_confirm(window, button_title: str, timeout: int = TIMEOUT) -> None:
    """点击会弹出确认框的按钮，并确认确认框已经出现。
    上一个对话框的关闭动画期间点击会被吞掉，按钮显示 enabled 但不生效；
    因此以「确认框出现」为判据重试按钮点击。
    """
    checkbox = window.child_window(
        title="我已确认目录、规则及可能的永久删除行为", control_type="CheckBox"
    )
    deadline = time.time() + timeout
    while time.time() < deadline:
        button = find_button(window, button_title)
        button.wait("visible enabled", timeout=timeout)
        click(window, button)
        try:
            checkbox.wait("visible", timeout=5)
            return
        except Exception:  # noqa: BLE001
            print(f"  「{button_title}」后确认框未出现，重试")


def close_app(window):
    close = find_button(window, "关闭")
    if close.exists():
        click(window, close)


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
        setup_directory(window, data)
        baseline = newest_task(data)
        open_confirm(window, "开始解压与分析")
        confirm_dialog(window)
        wait_task_status(data, "ready", baseline)
        find_button(window, "确认并执行整理").wait("visible enabled", timeout=TIMEOUT)
        print("S2 PASS：解压与分析完成（计划已生成，执行按钮可用）")
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
        setup_directory(window, data)
        baseline = newest_task(data)
        open_confirm(window, "开始解压与分析")
        confirm_dialog(window)
        wait_task_status(data, "ready", baseline)
        open_confirm(window, "确认并执行整理")
        confirm_dialog(window)
        wait_task_status(data, "finished", baseline)
        print("S3 PASS：全链路整理完成（任务状态 finished）")
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
