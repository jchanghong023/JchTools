#!/usr/bin/env python3
"""JchTools GUI OS 级冒烟测试（pywinauto / UIA）.

四条关键路径冒烟（2026-09-18 两工具拆分后）：
  S1 启动并正常退出；
  S2 目录整理：选择目录 → 开始分析（只读，无确认框）→ 计划生成（不执行）；
  S3 目录整理全链路：开始分析 → 确认执行 → 整理完成；
  S4 递归解压全链路：开始解压 → 一段确认 → 解压结束；完整成功的原包/分卷删除，既有内容保留，
     冲突自动改名、嵌套内容落盘，失败包保留在「解压失败」（H-07/X-05/X-06）。

用法：
    python scripts/gui_smoke.py --exe target/debug/JchTools.exe --data <已生成的测试数据目录>

依赖：pip install pywinauto（需要可交互桌面会话）。
"""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
from collections import Counter
from pathlib import Path
from typing import cast

import comtypes
import pywintypes
import win32con
import win32gui
from pywinauto import Application, controls, findbestmatch, findwindows, timings
from pywinauto.application import ProcessNotFoundError, WindowSpecification

TIMEOUT = 60
# 分析/执行完成等待用更长上限：真实数据、冷启动与杀软扫描都会让真实耗时远离秒级。
# 只放宽「等结果」的上限，元素出现仍按 TIMEOUT 快速失败，避免掩盖界面迟迟不响应的问题。
COMPLETION_TIMEOUT = 240
# 目录输入框与「选择目录…」按钮视为同一行的纵坐标容差（像素）。
EDIT_ROW_TOLERANCE_PX = 20
DEFAULT_EXE = "target/debug/JchTools.exe"
DEFAULT_DATA = ".tmp/gui-smoke/data"
EXTRACT_ACK = "我已确认：成功原包及分卷永久删除（不可恢复）"
ORGANIZE_ACK = "我已确认目录、规则及可能的永久删除行为（不可恢复）"

# pywinauto/pywin32 窗口操作在窗口建立/销毁竞态下抛出的瞬态错误族；
# 此元组是唯一放行集合，出现新瞬态类型须显式补充并说明：
# - pywintypes.error / pywintypes.com_error：pywin32 的 Win32 调用与 COM 调用基础错误；
# - comtypes.COMError：UIA 后端经 comtypes 访问 COM 元素，窗口消失时立即抛出；
# - ProcessNotFoundError：Application().connect() 在进程不存在/已退出时抛出；
# - timings.TimeoutError：window.wait() 等待条件超时抛出（继承 RuntimeError，与上无交集）；
# - findwindows.ElementNotFoundError / findbestmatch.MatchError /
#   controls.InvalidWindowHandle / controls.InvalidElement：pywinauto 解析控件规格失败
#   的四种错误（WindowSpecification.__getattribute__ 解析包装器时抛出，
#   与 window.exists()/wait() 内部捕获的是同一组）。
TRANSIENT_GUI_ERRORS: tuple[type[Exception], ...] = (
    pywintypes.error,
    pywintypes.com_error,
    comtypes.COMError,
    ProcessNotFoundError,
    timings.TimeoutError,
    findwindows.ElementNotFoundError,
    findbestmatch.MatchError,
    controls.InvalidWindowHandle,
    controls.InvalidElement,
)

# 任务库（task.sqlite3）读取容忍集，与 GUI 瞬态错误分开维护：
# 任务库可能正被运行中的任务写入或损坏，读不出的任务一律跳过（原为 except Exception，
# 收窄为该处实际会发生的类型）：sqlite3.Error 覆盖库打不开/被锁/损坏；
# ValueError 覆盖元数据 JSON 解析失败（json.JSONDecodeError 及其子类）。
TASK_RECORD_ERRORS: tuple[type[Exception], ...] = (sqlite3.Error, ValueError)


class _CliArgs(argparse.Namespace):
    """命令行参数容器：以固定字段取代 Namespace 的动态属性（运行期行为与默认 Namespace 一致）."""

    exe: str
    data: str

    def __init__(self) -> None:
        super().__init__()
        self.exe = DEFAULT_EXE
        self.data = DEFAULT_DATA


def wait_window(pid: int, timeout: int = TIMEOUT) -> tuple[Application, WindowSpecification]:
    deadline = time.time() + timeout
    last: Exception | None = None
    while time.time() < deadline:
        try:
            app = Application(backend="uia").connect(process=pid, timeout=1)
            window = app.window(title="JchTools")
            if window.exists():
                return app, window
        except TRANSIENT_GUI_ERRORS as exc:
            last = exc
        time.sleep(0.5)
    msg = f"等待 JchTools 窗口超时：{last}"
    raise RuntimeError(msg)


def find_button(window: WindowSpecification, title: str) -> WindowSpecification:
    return window.child_window(title=title, control_type="Button")


def activate(window: WindowSpecification) -> None:
    """把应用窗口提到前台，并抬到 z 序最上层.

    click_input 是真实鼠标点击：窗口被资源管理器等程序遮挡时，点击会落到遮挡窗口上。
    SetWindowPos(HWND_TOP) 只改 z 序、不抢焦点，在后台进程里也能生效，比 set_focus 可靠。
    """
    with contextlib.suppress(*TRANSIENT_GUI_ERRORS):
        _ = win32gui.ShowWindow(window.handle, win32con.SW_RESTORE)
        win32gui.SetWindowPos(
            window.handle,
            win32con.HWND_TOP,
            0,
            0,
            0,
            0,
            win32con.SWP_NOMOVE | win32con.SWP_NOSIZE | win32con.SWP_SHOWWINDOW,
        )
    with contextlib.suppress(*TRANSIENT_GUI_ERRORS):
        _ = window.set_focus()
    time.sleep(0.3)


def click(window: WindowSpecification, control: WindowSpecification) -> None:
    activate(window)
    control.click_input()
    time.sleep(0.3)


def confirm_dialog(window: WindowSpecification, timeout: int = TIMEOUT, *, extraction: bool = False) -> None:
    """勾选「我已确认…」并点「确认」，以**对话框消失**作为成功判据.

    点击可能落在对话框滑入动画的空档或未生效，所以按当前状态重试：
    未勾选就点复选框，已勾选就点确认，直到对话框关闭；
    只检查「点击没报错」会把没生效的点击当成成功，后续等待必然超时。
    """
    checkbox = window.child_window(title=EXTRACT_ACK if extraction else ORGANIZE_ACK, control_type="CheckBox")
    ok = find_button(window, "确认")
    deadline = time.time() + timeout
    attempts = 0
    while time.time() < deadline:
        _ = checkbox.wait("visible", timeout=10)
        time.sleep(0.3)  # 让对话框完成滑入，避免点到动画中途的位置
        attempts += 1
        click(window, ok if ok.is_enabled() else checkbox)
        for _ in range(20):
            time.sleep(0.3)
            try:
                if not checkbox.is_visible():
                    return
            except TRANSIENT_GUI_ERRORS:
                return
        print(f"  确认框仍未关闭，重试第 {attempts} 次")
    msg = "确认对话框没有关闭：勾选或确认点击未生效"
    raise RuntimeError(msg)


def state_dir() -> Path:
    r"""与 src/config.rs::state_dir 一致：%LOCALAPPDATA%\JchTools\data."""
    local = os.environ.get("LOCALAPPDATA")
    if not local:
        msg = "缺少 LOCALAPPDATA，无法定位任务目录"
        raise RuntimeError(msg)
    return Path(local) / "JchTools" / "data"


def _task_records(data: str) -> list[tuple[str, object]]:
    """列出针对 data 的任务（目录名排序，形如 %Y%m%dT%H%M%S-uuid）."""
    tasks = state_dir() / "tasks"
    want = os.path.normcase(str(Path(data).resolve()))
    records: list[tuple[str, object]] = []
    for task in tasks.glob("*"):
        db = task / "task.sqlite3"
        if not db.is_file():
            continue
        with contextlib.suppress(*TASK_RECORD_ERRORS):
            conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
            rows: dict[str, object] = dict(conn.execute("SELECT key,value FROM metadata").fetchall())
            conn.close()
            root_raw = rows.get("root", '""')
            status_raw = rows.get("status", '""')
            # json.loads 只接受 str/bytes/bytearray；其余类型原本也是 TypeError 后跳过，等价。
            # 解析出的 root 非字符串时原本 removeprefix 抛 AttributeError 后跳过，等价。
            if isinstance(root_raw, (str, bytes, bytearray)) and isinstance(status_raw, (str, bytes, bytearray)):
                # cast(object) 是诚实放宽：json.loads 的 Any 在此进入类型化世界，JSON 值本就属于 object；
                # basedpyright all 模式对「Any 赋给 object」也报 reportAny，cast 是唯一无 suppression 出口。
                root = cast("object", json.loads(root_raw))
                if isinstance(root, str):
                    if os.path.normcase(root.removeprefix("\\\\?\\")) != want:
                        continue
                    records.append((task.name, json.loads(status_raw)))
    # 任务目录名唯一，按首元素排序与按整个元组排序结果一致。
    records.sort(key=lambda record: record[0])
    return records


def newest_task(data: str) -> str:
    records = _task_records(data)
    return records[-1][0] if records else ""


def wait_task_status(data: str, expected: str, after: str = "", timeout: int = COMPLETION_TIMEOUT) -> None:
    """等待这次点击新建的任务跑到 expected 状态.

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
                msg = f"任务 {name} 状态为 {status}"
                raise RuntimeError(msg)
        time.sleep(0.5)
    msg = f"等待任务状态 {expected} 超时（目录 {data}）"
    raise RuntimeError(msg)


def set_directory(window: WindowSpecification, path: str) -> None:
    # 目录输入框是与「选择目录…」按钮同排的 Edit 控件。
    button = find_button(window, "选择目录…")
    button_rect = button.rectangle()
    candidates = [
        edit
        for edit in window.descendants(control_type="Edit")
        if abs(edit.rectangle().top - button_rect.top) < EDIT_ROW_TOLERANCE_PX
    ]
    if not candidates:
        msg = "未找到目录输入框"
        raise RuntimeError(msg)
    edit = min(candidates, key=lambda e: e.rectangle().left)
    edit.set_edit_text(path)


def setup_directory(window: WindowSpecification, path: str, timeout: int = TIMEOUT) -> None:
    """设置目录并确认界面已接受它.

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
    msg = "目录未被界面接受：状态栏始终没有出现「目录已就绪」"
    raise RuntimeError(msg)


def open_confirm(window: WindowSpecification, button_title: str, timeout: int = TIMEOUT) -> None:
    """点击会弹出确认框的按钮，并确认确认框已经出现.

    上一个对话框的关闭动画期间点击会被吞掉，按钮显示 enabled 但不生效；
    因此以「确认框出现」为判据重试按钮点击。
    """
    title = EXTRACT_ACK if button_title == "开始解压" else ORGANIZE_ACK
    checkbox = window.child_window(title=title, control_type="CheckBox")
    deadline = time.time() + timeout
    while time.time() < deadline:
        button = find_button(window, button_title)
        _ = button.wait("visible enabled", timeout=timeout)
        click(window, button)
        try:
            _ = checkbox.wait("visible", timeout=5)
        except TRANSIENT_GUI_ERRORS:
            print(f"  「{button_title}」后确认框未出现，重试")
        else:
            return


def close_app(window: WindowSpecification) -> None:
    """按用户路径请求关闭：激活窗口后点标题栏「关闭」.

    合成鼠标点击在本环境可能落空或点到同窗其他控件（实测 S2 会误开确认层并卡住），
    因此 _wait_exit_or_kill 的后续重试允许升级为 WM_CLOSE 兜底；首次尝试只走这条
    用户路径，避免「标题栏按钮点击失效」这类回归被兜底掩盖。
    """
    with contextlib.suppress(*TRANSIENT_GUI_ERRORS):
        for button in window.descendants(control_type="Button"):
            if (button.window_text() or "") == "关闭":
                # 与 click() 同口径：先激活窗口再点，避免合成点击落到别的窗口。
                activate(window)
                button.click_input()
                time.sleep(0.3)
                return


def _escalate_close(window: WindowSpecification) -> None:
    """关闭兜底：向窗口发标准 WM_CLOSE，走同一条 on_close_requested 路径，不依赖坐标命中."""
    with contextlib.suppress(*TRANSIENT_GUI_ERRORS):
        win32gui.PostMessage(window.handle, win32con.WM_CLOSE, 0, 0)


def _wait_exit_or_kill(
    proc: subprocess.Popen[bytes],
    timeout: int = 15,
    window: WindowSpecification | None = None,
) -> tuple[bool, int | None]:
    """等待进程退出，超时则强制结束（避免异常路径泄漏进程），并返回是否被强杀与退出码.

    合成鼠标点击可能落空（窗口未在前台时点到了别的窗口，实测 S2 稳定复现且随后手动
    点击同一按钮立即退出）：首次尝试仍只点标题栏按钮，之后的重试才补发 WM_CLOSE，
    把「点击落空」与「关闭路径真的挂死」区分开——后者会耗尽全部重试并最终被强杀、
    由断言判失败。
    """
    deadline = time.time() + timeout
    attempts = 0
    while time.time() < deadline:
        try:
            _ = proc.wait(timeout=min(3.0, max(0.1, deadline - time.time())))
        except subprocess.TimeoutExpired:
            attempts += 1
            if window is not None:
                with contextlib.suppress(*TRANSIENT_GUI_ERRORS):
                    if attempts <= 1:
                        close_app(window)
                    else:
                        _escalate_close(window)
        else:
            return False, proc.returncode
    proc.kill()
    _ = proc.wait(timeout=10)
    return True, proc.returncode


def assert_clean_exit(tag: str, *, killed: bool, code: int | None) -> None:
    """S1-S4 的退出断言：被强杀或非 0 退出码都算失败（AGENTS §3.4 不得把未验证当作通过）."""
    if killed:
        msg = f"{tag}：多次点击关闭后进程仍未退出，已被强杀（关闭路径可能挂死或窗口无法退出）"
        raise RuntimeError(msg)
    if code != 0:
        msg = f"{tag}：进程退出码 {code}（预期 0）"
        raise RuntimeError(msg)


def s1_launch_and_exit(exe: str) -> None:
    proc = subprocess.Popen([exe])
    window: WindowSpecification | None = None
    try:
        _, window = wait_window(proc.pid)
        print("S1 PASS：窗口启动并可见")
    finally:
        if window is not None:
            with contextlib.suppress(*TRANSIENT_GUI_ERRORS):
                close_app(window)
        killed, code = _wait_exit_or_kill(proc, window=window)
    assert_clean_exit("S1", killed=killed, code=code)
    print("S1 PASS：进程已退出")


def goto_organizer(window: WindowSpecification) -> None:
    """启动落在注册表第一个工具（递归解压）；目录整理用例先切过去."""
    click(window, find_button(window, "目录整理"))


def s2_analyze_only(exe: str, data: str) -> None:
    proc = subprocess.Popen([exe])
    window: WindowSpecification | None = None
    try:
        _, window = wait_window(proc.pid)
        goto_organizer(window)
        setup_directory(window, data)
        baseline = newest_task(data)
        # C-01：分析只读，不再弹破坏性确认框——点击后直接进入分析。
        click(window, find_button(window, "开始分析"))
        wait_task_status(data, "ready", baseline)
        _ = find_button(window, "确认并执行整理").wait("visible enabled", timeout=TIMEOUT)
        print("S2 PASS：分析完成（计划已生成，执行按钮可用；分析阶段未改动文件）")
    finally:
        if window is not None:
            with contextlib.suppress(*TRANSIENT_GUI_ERRORS):
                close_app(window)
        killed, code = _wait_exit_or_kill(proc, window=window)
    assert_clean_exit("S2", killed=killed, code=code)
    print("S2 PASS：进程已退出")


def s3_full_organize(exe: str, data: str) -> None:
    proc = subprocess.Popen([exe])
    window: WindowSpecification | None = None
    try:
        _, window = wait_window(proc.pid)
        goto_organizer(window)
        setup_directory(window, data)
        baseline = newest_task(data)
        click(window, find_button(window, "开始分析"))
        wait_task_status(data, "ready", baseline)
        open_confirm(window, "确认并执行整理")
        confirm_dialog(window)
        wait_task_status(data, "finished", baseline)
        for parent, dirs, files in os.walk(data):
            if ".git" in dirs or ".git" in files:
                dirs.clear()
                continue
            if Path(parent) != Path(data) and not dirs and not files:
                msg = f"整理成功后仍残留空目录：{parent}"
                raise RuntimeError(msg)
        print("S3 PASS：全链路整理完成（任务状态 finished）")
    finally:
        if window is not None:
            with contextlib.suppress(*TRANSIENT_GUI_ERRORS):
                close_app(window)
        killed, code = _wait_exit_or_kill(proc, window=window)
    assert_clean_exit("S3", killed=killed, code=code)
    print("S3 PASS：进程已退出")


# 自动解压白名单（与 src/rules.rs::archive_name 同口径）：只有这些后缀会被解压。
# 其它格式（.cab/.iso/.wim/.lzh/.cpio/.docx/.msi 等）即使 7-Zip 能打开也不碰，
# 因此在核对里必须按「既有文件」处理——字节不变、不得消失、不得进隔离目录。
ARCHIVE_SUFFIXES = (
    ".zip",
    ".7z",
    ".rar",
    ".tar",
    ".gz",
    ".bz2",
    ".xz",
    ".zst",
    ".lzma",
    ".z",
    ".tgz",
    ".tbz2",
    ".txz",
    ".tzst",
)


def file_digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def verify_original_disposition(root: Path, originals: dict[Path, str], quarantined: Counter[str]) -> None:
    """逐个原包核对：成功组的原包与分卷必须已删除且不得出现在「解压失败」，失败组必须完整隔离，既有文件必须原样保留."""
    successful_sections = {
        "07-压缩包-各格式",
        "08-压缩包-冲突",
        "09-压缩包-嵌套",
        "10-压缩包-超深",
        "13-压缩包-分卷",
        "14-大文件",
    }
    for relative, digest in originals.items():
        current = root / relative
        is_archive = relative.suffix.lower() in ARCHIVE_SUFFIXES or relative.suffix[1:].isdigit()
        if is_archive and relative.parts[0] in successful_sections:
            if current.exists():
                msg = f"完整成功后仍保留原包或分卷：{relative}"
                raise RuntimeError(msg)
            continue
        if current.is_file():
            if is_archive:
                msg = f"失败原包未隔离：{relative}"
                raise RuntimeError(msg)
            if file_digest(current) != digest:
                msg = f"解压修改了既有文件：{relative}"
                raise RuntimeError(msg)
            continue
        if not is_archive or quarantined[digest] == 0:
            msg = f"解压丢失失败原包、分卷或既有文件：{relative}"
            raise RuntimeError(msg)
        quarantined[digest] -= 1
    # X-05 与 X-06 是互斥的两种结果：「原包不在原位」不等于「按 X-05 永久删除」——
    # 被防护上限误伤而按 X-06 移入「解压失败」的原包，原位同样不存在。全部预期
    # 隔离项按 digest 配平后，「解压失败」不得再有剩余项：成功组的包若被误伤隔离，
    # 其 digest 不会出现在任何配平里，在此被拦下（否则 S4 对该类回归假绿，
    # 例如 max_ratio 上限被调小后 14-大文件 的全零包被整包推进「解压失败」）。
    leftovers = {digest: count for digest, count in quarantined.items() if count > 0}
    if leftovers:
        msg = (
            "「解压失败」存在无法对应到任何失败原包/分卷的多余隔离项"
            f"（成功包可能被误伤隔离）：{leftovers}"
        )
        raise RuntimeError(msg)


# 与 make_tmp.py 的 07 分区一致：同内容的单文件压缩流个数（gz / bz2 / xz / lzma）。
STREAM_PAYLOAD_CASES = ("single.txt.gz", "single.txt.bz2", "single.txt.xz", "single.txt.lzma")


def verify_extracted_outputs(root: Path) -> None:
    """核对解压产物内容、分卷体积、复合压缩的中间层与嵌套原包清理."""
    expected = {
        "08-压缩包-冲突/说明 (1).txt": b"archive version B, different content and length\n",
        "08-压缩包-冲突/等长 (1).txt": b"fedcba9876543210",
        "09-压缩包-嵌套/level5.txt": b"innermost payload\n",
    }
    for relative, content in expected.items():
        if (root / relative).read_bytes() != content:
            msg = f"解压输出内容不符合测试语料：{relative}"
            raise RuntimeError(msg)
    # X-01：复合扩展名整体识别，中间 tar 层不得留在正式位置（`.tar.lzma` 这类
    # 引擎不自动拆 tar 的组合，靠把中间 tar 当嵌套包继续解开并清理）。
    leftovers = [str(path.relative_to(root)) for path in (root / "07-压缩包-各格式").rglob("*.tar")]
    if leftovers:
        msg = f"中间 tar 层不得留在正式位置：{leftovers}"
        raise RuntimeError(msg)
    # 同内容的单文件压缩流（gz/bz2/xz/lzma）都应解出；同名冲突自动改名，内容不变。
    payload = b"single member payload\n" * 8
    expected_copies = len(STREAM_PAYLOAD_CASES)
    copies = sum(
        1 for path in (root / "07-压缩包-各格式").rglob("*") if path.is_file() and path.read_bytes() == payload
    )
    if copies != expected_copies:
        msg = f"单文件压缩流应各解出一份内容（含 .lzma），实得 {copies} 份"
        raise RuntimeError(msg)
    if (root / "13-压缩包-分卷/big.bin").stat().st_size != 2 * 1024 * 1024 + 12345:
        msg = "分卷原包删除前必须完整解出 big.bin"
        raise RuntimeError(msg)
    for path in (root / "09-压缩包-嵌套").rglob("*"):
        if path.is_file() and path.suffix.lower() in ARCHIVE_SUFFIXES:
            msg = f"嵌套成功原包未清理：{path.relative_to(root)}"
            raise RuntimeError(msg)


def verify_extraction_results(root: Path, originals: dict[Path, str]) -> None:
    """核对成功源包删除、失败包完整隔离、既有文件不变与真实解压结果."""
    quarantined = Counter(file_digest(path) for path in (root / "解压失败").rglob("*") if path.is_file())
    verify_original_disposition(root, originals, quarantined)
    verify_extracted_outputs(root)


def s4_full_extract(exe: str, data: str) -> None:
    """递归解压全链路：成功原包删除、既有文件不变，冲突另存、嵌套内容完整落盘."""
    proc = subprocess.Popen([exe])
    window: WindowSpecification | None = None
    root = Path(data)
    originals = {path.relative_to(root): file_digest(path) for path in root.rglob("*") if path.is_file()}
    try:
        _, window = wait_window(proc.pid)
        # 启动页即递归解压，无需切换。
        setup_directory(window, data)
        baseline = newest_task(data)
        open_confirm(window, "开始解压")
        confirm_dialog(window, extraction=True)
        wait_task_status(data, "finished", baseline)
        verify_extraction_results(root, originals)
        print("S4 PASS：成功原包及分卷删除（隔离目录无多余项）；既有内容保留，冲突自动改名，嵌套内容正确落盘")
    finally:
        if window is not None:
            with contextlib.suppress(*TRANSIENT_GUI_ERRORS):
                close_app(window)
        killed, code = _wait_exit_or_kill(proc, window=window)
    assert_clean_exit("S4", killed=killed, code=code)
    print("S4 PASS：进程已退出")


def main() -> int:
    parser = argparse.ArgumentParser(description="JchTools GUI 冒烟测试")
    _ = parser.add_argument("--exe", default=DEFAULT_EXE)
    _ = parser.add_argument("--data", default=DEFAULT_DATA)
    args = parser.parse_args(namespace=_CliArgs())
    exe = Path(args.exe).resolve()
    data = Path(args.data).resolve()
    if not exe.is_file():
        msg = f"找不到 {exe}"
        raise RuntimeError(msg)
    if not data.is_dir():
        msg = f"找不到测试数据目录 {data}"
        raise RuntimeError(msg)
    # S3 会真实执行整理：只允许对一次性测试副本操作。
    # 仓库内仅放行 .tmp/（默认 .tmp/gui-smoke/data 就在这里）；其它路径拒绝。
    repo = Path(__file__).resolve().parent.parent
    home = Path.home().resolve()
    under_repo_tmp = repo / ".tmp" in (data, *data.parents)
    if (data == repo or repo in data.parents) and not under_repo_tmp:
        msg = "拒绝在仓库目录内执行 GUI 整理冒烟（请使用 .tmp/gui-smoke/data 或其它副本）"
        raise RuntimeError(msg)
    if not under_repo_tmp and (data == home or home in data.parents):
        msg = "拒绝在用户主目录内执行 GUI 整理冒烟（请使用一次性测试副本）"
        raise RuntimeError(msg)
    # 盘符根：parts 只有 ('D:\\',) 一层；'D:\\foo' 是 2 层，不得误杀。
    if data.drive and len(data.parts) <= 1:
        msg = f"拒绝在盘符根目录执行整理冒烟：{data}"
        raise RuntimeError(msg)

    s1_launch_and_exit(str(exe))
    # S4 解压与 S3 整理都会真实改写语料；每个阶段前都从旁路副本恢复，
    # 保证“干净语料上的完整链路”（顺序：S4 解压 → S2 只分析 → S3 整理）。
    # H-06/附录 E：所选根的任一祖先直接含 .git 时两工具拒绝整次处理。仓库根本身
    # 带 .git，冒烟副本若继续放在仓库 .tmp/ 下会整次被拒（S4 确认框不再出现）。
    # 改放系统临时目录（AGENTS §2 允许的 tempfile 例外；语料源目录不受影响）。
    scratch = Path(tempfile.mkdtemp(prefix="jchtools-gui-smoke-"))
    try:
        fresh = scratch / "data"
        _ = shutil.copytree(data, fresh)
        s4_full_extract(str(exe), str(fresh))
        shutil.rmtree(fresh, ignore_errors=True)
        _ = shutil.copytree(data, fresh)
        s2_analyze_only(str(exe), str(fresh))
        shutil.rmtree(fresh, ignore_errors=True)
        _ = shutil.copytree(data, fresh)
        s3_full_organize(str(exe), str(fresh))
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
