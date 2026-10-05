#!/usr/bin/env python3
"""JchTools GUI OS 级冒烟测试（pywinauto / UIA）.

关键路径冒烟（2026-09-18 两工具拆分后；S6-S14 为转 Markdown GUI E2E 扩展）：
  S1 启动并正常退出；
  S2 目录整理：选择目录 → 开始分析（只读，无确认框）→ 计划生成（不执行）；
  S3 目录整理全链路：开始分析 → 确认执行 → 整理完成；
  S4 递归解压全链路：开始解压 → 一段确认 → 解压结束；完整成功的原包/分卷删除，既有内容保留，
     冲突自动改名、嵌套内容落盘，失败包保留在「解压失败」（H-07/X-05/X-06）。
  S5 转 Markdown 基本链路：启动 → 切到「转 Markdown」→ 选输入/输出 → 开始 → 停止 → 关闭
     （T 分区附录 A GUI E2E 最小段；需 Xberg 已配置且组件就绪，默认序列不含 S5，须显式 --stages 请求）。
  S6 Xberg 目录选择与保存：设置页填入有效目录 → 「使用此目录」校验并持久保存
     （XB-20；需 JCHTOOLS_SMOKE_XBERG_DIR 指向包含 xberg.exe 的有效目录，应用配置经
     JCHTOOLS_TEST_STATE_DIR 隔离到临时目录，不写真实用户配置）。
  S7 重启恢复：S6 保存后完全退出，再启动时设置页自动恢复来源与目录（XB-18/XB-21）。
  S8 无效目录提示与重新选择：不存在的路径与缺 xberg.exe 的目录都被明确拒绝，
     且界面不锁死、可再次输入重试（T-05/T-06）。
  S9 组件缺失状态与初始化入口：未配置时转换页明确提示未就绪、开始按钮禁用、
     初始化入口与「前往设置」可见且不自动下载（XB-19/O-06；无需任何资产）。
  S10 输出层级与同名策略：保留层级两份同名输入各自成文；平铺只处理排序第一份、
     其余按重复跳过计数（T-11；需真实组件，Xberg 目录可真实，状态目录仍须隔离）。
     S5/S10-S14/S17 受统一隔离守卫：独立调用需验收编排布置的隔离环境变量在场
     （JCHTOOLS_TEST_STATE_DIR 与 JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT），缺则拒绝
     启动并提示经 acceptance.ps1 入口执行；确需独立运行时显式 --allow-isolated-run。
  S11 已有结果跳过：重复运行不覆盖既有产物，跳过数量如实显示（T-12）。
  S12 输出子树排除：输出目录位于输入内时整棵输出子树不作为新输入（T-09）。
  S13 部分失败与完成统计：单文件失败不终止批次，成功/失败可区分（T-16/T-24）。
  S14 运行中关闭确认与安全停止：转换运行中点标题栏「关闭」弹出「停止任务并关闭」，
     确认后当前文件结束、进程退出码 0（U-09/T-23；需 JCHTOOLS_S5_MEDIA 媒体样本）。
  S15 MD 合并/拆分真实 GUI：验证标题下移、原文件不变与 UTF-8 分片无损还原。
  S16 Git 真实 GUI：使用一次性仓库和本地 bare 远端，验证逐文件提交及推送结果。
  S17 在已保存有效 Xberg 的隔离配置下，从转换页初始化本地 notice 并核对就绪状态。

用法：
    python scripts/gui_smoke.py --exe target/debug/JchTools.exe --data <已生成的测试数据目录>
    受隔离守卫的阶段（S5/S10-S14/S17）独立调用时须已布置隔离环境变量或加
    --allow-isolated-run；推荐经 scripts/acceptance.ps1 编排执行。

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
import struct
import subprocess
import sys
import tempfile
import time
import zipfile
import zlib
from collections import Counter
from pathlib import Path
from typing import TYPE_CHECKING, cast

import comtypes
import pywintypes
import win32api
import win32con
import win32gui
import win32job
import win32process
from pywinauto import Application, controls, findbestmatch, findwindows, timings
from pywinauto.application import ProcessNotFoundError, WindowSpecification
from pywinauto.uia_defines import NoPatternInterfaceError

if TYPE_CHECKING:
    from collections.abc import Callable, Mapping

    from pywinauto.base_wrapper import BaseWrapper

TIMEOUT = 60
# 分析/执行完成等待用更长上限：真实数据、冷启动与杀软扫描都会让真实耗时远离秒级。
# 只放宽「等结果」的上限，元素出现仍按 TIMEOUT 快速失败，避免掩盖界面迟迟不响应的问题。
COMPLETION_TIMEOUT = 240
# 目录输入框与「选择目录…」按钮视为同一行的纵坐标容差（像素）。
EDIT_ROW_TOLERANCE_PX = 20
PHYSICAL_OVERLAP_THRESHOLD = 0.8
CONFIRM_CLOSED_OBSERVATIONS = 2
MD_SPLIT_LIMIT_BYTES = 1024
STOP_MEDIA_FILE_COUNT = 16
DEFAULT_EXE = "target/debug/JchTools.exe"
DEFAULT_DATA = ".tmp/gui-smoke/data"
# 冒烟阶段清单：S1-S4 无需转 Markdown 资产；S5 需要（默认序列不含 S5，须显式 --stages 请求）；
# S6-S9 只需隔离的应用配置目录（JCHTOOLS_TEST_STATE_DIR）；S10-S14 复用验收环境的真实组件。
SUPPORTED_STAGES = (
    "S1",
    "S2",
    "S3",
    "S4",
    "S5",
    "S6",
    "S7",
    "S8",
    "S9",
    "S10",
    "S11",
    "S12",
    "S13",
    "S14",
    "S15",
    "S16",
    "S17",
)
DEFAULT_STAGES = "S1,S2,S3,S4,S15,S16"
CONVERT_BUSY_TIMEOUT = 60  # 点击「开始转换」后等待「停止任务」出现的上限（秒）
_S5_LAST_ATTEMPT = 2  # S5 重按「开始转换」的末次序号（共 3 次，0 起）
CONVERT_STOP_TIMEOUT = 300  # 停止请求后等待「开始转换」恢复可用的上限（秒）
# 转 Markdown 页「选择目录…」应有行数（输入 / 输出；Xberg 目录在设置页，XB-20）。
CONVERT_DIR_ROWS = 2
EXTRACT_ACK = "我已确认：成功原包及分卷永久删除（不可恢复）"
ORGANIZE_ACK = "我已确认目录、规则及可能的永久删除行为（不可恢复）"
# 设置/转换页状态文案锚点（与 ui/app.slint 的文案保持同步，改动必须两侧同改）。
SETTINGS_SAVED_TEXT = "配置已持久保存"
CONVERT_NEED_RUNTIME_TEXT = "请先保存共享 Xberg 运行目录"
CONVERT_DONE_MARKER = "总耗时"
STOP_AND_CLOSE_TITLE = "停止任务并关闭"
STOP_AND_CLOSE_BUTTON = "停止并关闭"
# S11-07 统一隔离守卫：这些阶段的 run_stage 不自建隔离环境（不同于 S6-S9 的
# isolated_state_env），独立调用会直接读写真实用户状态目录（config.sqlite3）并
# 可能连接/唤起常驻服务（保存目录会触发 ensure_snap_supervisor）。因此要求经验收
# 编排布置的隔离环境（下述两个变量由 acceptance.ps1 的 gui-smoke / markdown 分支
# 统一布置）或显式 --allow-isolated-run 才放行；Xberg 运行目录本身允许指向真实
# 引擎目录——状态隔离必须，组件目录可真实。
ISOLATION_GUARDED_STAGES = ("S5", "S10", "S11", "S12", "S13", "S14", "S17")
ISOLATION_REQUIRED_ENV_KEYS = ("JCHTOOLS_TEST_STATE_DIR", "JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT")

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
    stages: str
    list_stages: bool
    allow_isolated_run: bool

    def __init__(self) -> None:
        super().__init__()
        self.exe = DEFAULT_EXE
        self.data = DEFAULT_DATA
        self.stages = DEFAULT_STAGES
        self.list_stages = False
        self.allow_isolated_run = False


class _OwnedProcessTree:
    """Windows Job Object 持有所属进程树；父 GUI 退出后仍能安全回收后台."""

    def __init__(self, proc: subprocess.Popen[bytes]) -> None:
        # types-pywin32 的 Job API 返回值/参数漏标；按实际 Win32 签名收口，不修改第三方桩。
        create_job = cast("Callable[[object, str], int]", win32job.CreateJobObject)
        query_information = cast("Callable[[int, int], dict[str, object]]", vars(win32job)["QueryInformationJobObject"])
        set_information = cast(
            "Callable[[int, int, dict[str, object]], None]", vars(win32job)["SetInformationJobObject"]
        )
        job = create_job(None, "")
        assigned = False
        try:
            information = query_information(job, win32job.JobObjectExtendedLimitInformation)
            limits = cast("dict[str, int]", information["BasicLimitInformation"])
            limits["LimitFlags"] |= win32job.JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            set_information(job, win32job.JobObjectExtendedLimitInformation, information)
            process = win32api.OpenProcess(win32con.PROCESS_SET_QUOTA | win32con.PROCESS_TERMINATE, 0, proc.pid)
            try:
                win32job.AssignProcessToJobObject(job, process)
            finally:
                win32api.CloseHandle(process)
            assigned = True
        finally:
            if not assigned:
                win32api.CloseHandle(job)
        self._job: int | None = job

    def terminate(self, proc: subprocess.Popen[bytes]) -> None:
        """只终止本轮 Job Object 中的 GUI 与后台，不按名称或已退出父 PID 操作."""
        self._kill_tree()
        with contextlib.suppress(OSError, ProcessLookupError):
            if proc.poll() is None:
                proc.kill()

    def close(self) -> None:
        """正常或异常退出后回收所属后台并释放 Job 句柄；重复关闭无副作用."""
        job = self._job
        if job is None:
            return
        try:
            self._kill_tree()
        finally:
            self._job = None
            win32api.CloseHandle(job)

    def _kill_tree(self) -> None:
        if self._job is not None:
            terminate_job = cast("Callable[[int, int], None]", vars(win32job)["TerminateJobObject"])
            terminate_job(self._job, 0)


def own_process_tree(proc: subprocess.Popen[bytes]) -> _OwnedProcessTree:
    """为各真实 GUI 验收驱动提供同一套进程所有权与收尾策略."""
    return _OwnedProcessTree(proc)


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


def _same_physical_control(left: BaseWrapper, right: BaseWrapper) -> bool:
    left_rect = left.rectangle()
    right_rect = right.rectangle()
    width = max(0, min(left_rect.right, right_rect.right) - max(left_rect.left, right_rect.left))
    height = max(0, min(left_rect.bottom, right_rect.bottom) - max(left_rect.top, right_rect.top))
    overlap = width * height
    left_area = max(1, (left_rect.right - left_rect.left) * (left_rect.bottom - left_rect.top))
    right_area = max(1, (right_rect.right - right_rect.left) * (right_rect.bottom - right_rect.top))
    return overlap / min(left_area, right_area) >= PHYSICAL_OVERLAP_THRESHOLD


def find_button(window: WindowSpecification, title: str) -> WindowSpecification:
    """返回同一物理按钮组中唯一的可见控件，去除 Slint 外/内层重复节点.

    未找到当前可见节点时仍返回可轮询的 WindowSpecification。旧调用约定会
    对结果调用 ``exists()`` 判断按钮是否尚未出现；此处直接抛错会把正常的
    「运行态按钮尚未出现」误报成阶段失败。真正需要按钮的 ``wait``/``click``
    仍会在超时后失败。
    """
    matching = [button for button in window.descendants(control_type="Button") if (button.window_text() or "") == title]
    buttons: list[BaseWrapper] = []
    for button in matching:
        if (button.window_text() or "") != title:
            continue
        if any(_same_physical_control(button, existing) for existing in buttons):
            continue
        buttons.append(button)
    visible = [button for button in buttons if button.is_visible()]
    candidates = visible or buttons
    if not candidates:
        return window.child_window(title=title, control_type="Button")
    candidates.sort(key=lambda button: (button.rectangle().top, button.rectangle().left))
    chosen_index = next(index for index, button in enumerate(matching) if button is candidates[0])
    return window.child_window(title=title, control_type="Button", found_index=chosen_index)


def activate(window: WindowSpecification | BaseWrapper) -> None:
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


def click(window: WindowSpecification, control: WindowSpecification | BaseWrapper) -> None:
    activate(window)
    try:
        invoke = getattr(control, "invoke", None)
    except TRANSIENT_GUI_ERRORS:
        invoke = None
    if callable(invoke):
        try:
            _ = invoke()
        except (NoPatternInterfaceError, *TRANSIENT_GUI_ERRORS):
            pass
        else:
            time.sleep(0.3)
            return
    target_pid = win32process.GetWindowThreadProcessId(window.handle)[1]
    foreground_pid = win32process.GetWindowThreadProcessId(win32gui.GetForegroundWindow())[1]
    if foreground_pid != target_pid:
        message = "鼠标兜底拒绝操作：前台窗口 PID 不属于被测 GUI"
        raise RuntimeError(message)
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
        closed_observations = 0
        for _ in range(20):
            time.sleep(0.3)
            try:
                # Slint 删除/重建 UIA 节点时旧 CheckBox 可能暂时不可读；
                # 用新枚举且非空的页面树连续两次确认模态标题消失。
                texts = [text.window_text() or "" for text in window.descendants(control_type="Text")]
                if texts and "确认文件处理范围" not in texts:
                    closed_observations += 1
                else:
                    closed_observations = 0
                if closed_observations >= CONFIRM_CLOSED_OBSERVATIONS:
                    return
            except TRANSIENT_GUI_ERRORS:
                closed_observations = 0
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
        and edit.rectangle().right <= button_rect.left
    ]
    if not candidates:
        msg = "未找到目录输入框"
        raise RuntimeError(msg)
    # 侧栏搜索框在小窗口下可能与目录输入框同高；取紧挨「选择目录…」左侧的 Edit。
    edit = min(candidates, key=lambda e: button_rect.left - e.rectangle().right)
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
    """先发 WM_CLOSE；鼠标兜底前验证前台窗口 PID 属于被测 GUI."""
    handle = window.handle
    target_pid = win32process.GetWindowThreadProcessId(handle)[1]
    win32gui.PostMessage(handle, win32con.WM_CLOSE, 0, 0)
    time.sleep(0.3)
    with contextlib.suppress(*TRANSIENT_GUI_ERRORS, RuntimeError):
        if not window.exists():
            return
    foreground = win32gui.GetForegroundWindow()
    foreground_pid = win32process.GetWindowThreadProcessId(foreground)[1]
    if foreground_pid != target_pid:
        message = "关闭兜底拒绝操作：前台窗口 PID 不属于被测 GUI"
        raise RuntimeError(message)
    for button in [b for b in window.descendants(control_type="Button") if (b.window_text() or "") == "关闭"]:
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
    owner: _OwnedProcessTree | None = None,
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
                with contextlib.suppress(*TRANSIENT_GUI_ERRORS, RuntimeError):
                    if attempts <= 1:
                        close_app(window)
                    else:
                        _escalate_close(window)
        else:
            return False, proc.returncode
    if owner is not None:
        owner.terminate(proc)
    else:
        with contextlib.suppress(OSError, ProcessLookupError):
            proc.kill()
    with contextlib.suppress(subprocess.TimeoutExpired, OSError):
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


def run_stage(  # noqa: PLR0913 - 脚手架的收尾钩子与子进程环境天然成组
    tag: str,
    exe: str,
    body: Callable[[WindowSpecification], None],
    *,
    pre: Callable[[], None] | None = None,
    after: Callable[[], None] | None = None,
    env: dict[str, str] | None = None,
) -> None:
    """S1-S14 共用的启动/拆除脚手架：Popen →（pre）→ wait_window → body → 统一收尾.

    拆除顺序（含异常路径）为：窗口非 None 才 close_app；随后等待/结束本次 Job
    Object 的 GUI 及其子服务；最后无论前面哪一步失败都执行 after。pre 也在统一
    try/finally 内，因此媒体缺失等前置失败不会泄漏已启动的 GUI。Job Object 只绑定
    本函数刚启动的 PID，不能触碰其他用户实例。env 非 None 时传给子进程（S6-S9 用
    JCHTOOLS_TEST_STATE_DIR 隔离应用配置，不写真实用户配置）。
    收尾断言与最终 PASS 行统一在此打印。
    """
    proc = subprocess.Popen([exe], env=env)
    owner: _OwnedProcessTree | None = None
    window: WindowSpecification | None = None
    try:
        owner = _OwnedProcessTree(proc)
        if pre is not None:
            pre()
        _, window = wait_window(proc.pid)
        effective_env = os.environ if env is None else env
        if effective_env.get("JCHTOOLS_TEST_STATE_DIR"):
            isolated_db = Path(effective_env["JCHTOOLS_TEST_STATE_DIR"]) / "config.sqlite3"
            deadline = time.time() + TIMEOUT
            while not isolated_db.is_file() and time.time() < deadline:
                time.sleep(0.1)
            if not isolated_db.is_file():
                message = "GUI 未建立隔离配置；请先 cargo build --features test-hooks，禁止写入真实用户配置"
                raise RuntimeError(message)
        body(window)
    finally:
        if window is not None:
            with contextlib.suppress(*TRANSIENT_GUI_ERRORS, RuntimeError):
                close_app(window)
        try:
            killed, code = _wait_exit_or_kill(proc, window=window, owner=owner)
        finally:
            try:
                if after is not None:
                    after()
            finally:
                if owner is not None:
                    owner.close()
    assert_clean_exit(tag, killed=killed, code=code)
    print(f"{tag} PASS：进程已退出")


def s1_launch_and_exit(exe: str) -> None:
    run_stage("S1", exe, lambda _window: print("S1 PASS：窗口启动并可见"))


def goto_organizer(window: WindowSpecification) -> None:
    """启动落在注册表第一个工具（递归解压）；目录整理用例先切过去."""
    click(window, find_button(window, "目录整理"))


def analyze_until_ready(window: WindowSpecification, data: str) -> str:
    """S2/S3 共用前缀：切到目录整理、设置数据集并分析到 ready，返回任务基线."""
    goto_organizer(window)
    setup_directory(window, data)
    baseline = newest_task(data)
    # C-01：分析只读，不再弹破坏性确认框——点击后直接进入分析。
    click(window, find_button(window, "开始分析"))
    wait_task_status(data, "ready", baseline)
    return baseline


def s2_analyze_only(exe: str, data: str) -> None:
    def body(window: WindowSpecification) -> None:
        _ = analyze_until_ready(window, data)
        _ = find_button(window, "确认并执行整理").wait("visible enabled", timeout=TIMEOUT)
        print("S2 PASS：分析完成（计划已生成，执行按钮可用；分析阶段未改动文件）")

    run_stage("S2", exe, body)


def s3_full_organize(exe: str, data: str) -> None:
    def body(window: WindowSpecification) -> None:
        baseline = analyze_until_ready(window, data)
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

    run_stage("S3", exe, body)


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
        msg = f"「解压失败」存在无法对应到任何失败原包/分卷的多余隔离项（成功包可能被误伤隔离）：{leftovers}"
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
    # X-06：10-压缩包-超深 的第 17 层是按深度上限预期隔离的失败项。
    # 先严格核对这个有意保留的嵌套包，再从隔离计数中扣除一份；后续
    # verify_original_disposition 的 leftovers 检查仍会拒绝所有其他多余项。
    deep_failed = root / "解压失败" / "d17.zip"
    if not deep_failed.is_file():
        msg = "超深压缩包应按 X-06 隔离 d17.zip"
        raise RuntimeError(msg)
    try:
        with zipfile.ZipFile(deep_failed) as archive:
            members = archive.namelist()
    except (OSError, zipfile.BadZipFile) as exc:
        msg = "X-06 隔离的 d17.zip 不是有效压缩包"
        raise RuntimeError(msg) from exc
    if members != ["d18.zip"]:
        msg = f"X-06 隔离的 d17.zip 内容不符合预期：{members}"
        raise RuntimeError(msg)
    quarantined[file_digest(deep_failed)] -= 1
    verify_original_disposition(root, originals, quarantined)
    verify_extracted_outputs(root)


def s4_full_extract(exe: str, data: str) -> None:
    """递归解压全链路：成功原包删除、既有文件不变，冲突另存、嵌套内容完整落盘."""
    root = Path(data)
    originals: dict[Path, str] = {}

    def snapshot_originals() -> None:
        originals.update({path.relative_to(root): file_digest(path) for path in root.rglob("*") if path.is_file()})

    def body(window: WindowSpecification) -> None:
        # 启动页即递归解压，无需切换。
        setup_directory(window, data)
        baseline = newest_task(data)
        open_confirm(window, "开始解压")
        confirm_dialog(window, extraction=True)
        wait_task_status(data, "finished", baseline)
        verify_extraction_results(root, originals)
        print("S4 PASS：成功原包及分卷删除（隔离目录无多余项）；既有内容保留，冲突自动改名，嵌套内容正确落盘")

    run_stage("S4", exe, body, pre=snapshot_originals)


def goto_converter(window: WindowSpecification) -> None:
    click(window, find_button(window, "转 Markdown"))


def converter_directory_rows(window: WindowSpecification) -> list[BaseWrapper]:
    """转 Markdown 页自上而下两行「选择目录…」按钮：输入 / 输出（Xberg 目录在设置页，XB-20）."""
    buttons = [b for b in window.descendants(control_type="Button") if (b.window_text() or "") == "选择目录…"]
    unique: list[BaseWrapper] = []
    for button in buttons:
        if not button.is_visible() or any(_same_physical_control(button, existing) for existing in unique):
            continue
        unique.append(button)
    unique.sort(key=lambda b: (b.rectangle().top, b.rectangle().left))
    buttons = unique
    if len(buttons) < CONVERT_DIR_ROWS:
        msg = f"转 Markdown 页「选择目录…」按钮不足两行（实得 {len(buttons)}）"
        raise RuntimeError(msg)
    return buttons


def set_converter_dirs(window: WindowSpecification, input_dir: str, output_dir: str) -> None:
    rows = converter_directory_rows(window)
    for button, value in ((rows[0], input_dir), (rows[1], output_dir)):
        top = button.rectangle().top
        candidates = [
            edit
            for edit in window.descendants(control_type="Edit")
            if abs(edit.rectangle().top - top) < EDIT_ROW_TOLERANCE_PX
        ]
        if not candidates:
            msg = "未找到与「选择目录…」同排的目录输入框"
            raise RuntimeError(msg)
        min(candidates, key=lambda e: e.rectangle().left).set_edit_text(value)


def unique_visible_buttons(window: WindowSpecification, title: str) -> list[BaseWrapper]:
    """按矩形去除 Slint 外层/内层重复 UIA Button 节点."""
    candidates: list[BaseWrapper] = []
    for button in window.descendants(control_type="Button"):
        if (button.window_text() or "") != title or not button.is_visible():
            continue
        if any(_same_physical_control(button, existing) for existing in candidates):
            continue
        candidates.append(button)
    candidates.sort(key=lambda button: (button.rectangle().top, button.rectangle().left))
    return candidates


def wait_button(window: WindowSpecification, title: str, timeout: float, *, enabled: bool = False) -> BaseWrapper:
    """动态等待唯一可见按钮，避免页面切换时缓存空 WindowSpecification."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            buttons = unique_visible_buttons(window, title)
            if buttons and (not enabled or any(button.is_enabled() for button in buttons)):
                return next(button for button in buttons if not enabled or button.is_enabled())
        except TRANSIENT_GUI_ERRORS:
            pass
        time.sleep(0.2)
    message = f"等待可见{('可用' if enabled else '')}按钮超时：{title}"
    raise RuntimeError(message)


def s5_markdown_basic_chain(exe: str) -> None:
    """S5 转 Markdown 基本链路：启动→选输入→开始→停止→关闭.

    未配置/未就绪时「开始转换」保持禁用，wait 超时即失败——不得把「未配置也通过」
    报成基本链路通过。停止按 T-23（当前文件结束后生效、结果保留）；产物内容断言归
    scripts/markdown_acceptance.py，本冒烟只断言链路行为。
    """
    scratch_box: list[Path] = []

    def prepare_scratch() -> None:
        scratch = Path(tempfile.mkdtemp(prefix="jchtools-gui-smoke-s5-"))
        (scratch / "input").mkdir()
        (scratch / "output").mkdir()
        # 空输入的转换瞬时完成，「停止任务」运行态不可观察，开始→停止链路断言失效；
        # 用媒体样本（转录需数秒）保证运行态可观察。样本由环境变量显式提供（真实
        # 组件门控用例同口径），缺样本时明确失败而不是把「没开始」当通过。
        media = os.environ.get("JCHTOOLS_S5_MEDIA", "")
        if not media or not Path(media).is_file():
            message = "S5 需要媒体样本以观察运行态：设置 JCHTOOLS_S5_MEDIA 指向一个真实媒体文件"
            raise RuntimeError(message)
        for index in range(STOP_MEDIA_FILE_COUNT):
            _ = shutil.copyfile(media, scratch / "input" / f"{index:02}_{Path(media).name}")
        scratch_box.append(scratch)

    def body(window: WindowSpecification) -> None:
        scratch = scratch_box[0]
        goto_converter(window)
        set_converter_dirs(window, str(scratch / "input"), str(scratch / "output"))
        _ = wait_button(window, "开始转换", COMPLETION_TIMEOUT, enabled=True)
        # 合成点击偶发落空（点击后仍停在「尚未开始」）：按当前状态重试，与
        # confirm_dialog 同一立场——只检查「点击没报错」会把没生效的点击当成
        # 成功。每次重按后观察一段运行态窗口，重按最多 3 次。
        busy_timeout = CONVERT_BUSY_TIMEOUT // 3
        for attempt in range(3):
            click(window, find_button(window, "开始转换"))
            try:
                _ = wait_button(window, "停止任务", busy_timeout, enabled=True)
                break
            except (timings.TimeoutError, RuntimeError):
                if attempt == _S5_LAST_ATTEMPT:
                    raise
        click(window, find_button(window, "停止任务"))
        _ = wait_button(window, "开始转换", CONVERT_STOP_TIMEOUT, enabled=True)
        _ = wait_text_containing(window, "已停止")
        produced = list((scratch / "output").glob("*.md"))
        require(len(produced) < STOP_MEDIA_FILE_COUNT, "停止后不得继续转换全部后续文件")
        print("S5 PASS：开始→停止链路完成（停止在当前文件后生效，界面回到可开始状态）")

    def cleanup() -> None:
        shutil.rmtree(scratch_box[0], ignore_errors=True)

    run_stage("S5", exe, body, pre=prepare_scratch, after=cleanup)


# ---------------------------------------------------------------- S6-S14：转 Markdown / 设置页阶段。

ATTEMPT_TIMEOUT = 8  # 单次「点击→等文本」的观察窗（秒）；落空即重试


def require(condition: object, message: str) -> None:
    """S6-S14 的断言 helper：bandit B101 禁用 assert，统一 raise 口径."""
    if not condition:
        raise RuntimeError(message)


def click_and_wait_text(
    window: WindowSpecification,
    button_title: str,
    needle: str,
    attempts: int = 5,
) -> str:
    """点击按钮并以 needle 文本出现为准——合成点击可能落空，未出现即重试.

    与 confirm_dialog/open_confirm 同一立场：只检查「点击没报错」会把没生效
    的点击当成功，后续等待必然超时。
    """
    for _attempt in range(attempts):
        click(window, find_button(window, button_title))
        try:
            return wait_text_containing(window, needle, timeout=ATTEMPT_TIMEOUT)
        except RuntimeError:
            continue
    msg = f"点击「{button_title}」后始终未出现「{needle}」（点击可能一直落空）"
    raise RuntimeError(msg)


def goto_settings(window: WindowSpecification) -> None:
    """切到设置页：等待设置专属「下载 Xberg」控件，避免命中转换页状态文案."""
    for _attempt in range(5):
        click(window, find_button(window, "设置"))
        try:
            _ = find_button(window, "下载 Xberg").wait("visible", timeout=ATTEMPT_TIMEOUT)
        except (RuntimeError, timings.TimeoutError):
            continue
        else:
            return
    message = "点击「设置」后始终未出现设置页专属「下载 Xberg」控件"
    raise RuntimeError(message)


def set_settings_custom_dir(window: WindowSpecification, path: str) -> None:
    """填设置页的 Xberg 目录输入框：取「使用此目录」按钮上方最近的 Edit.

    侧栏搜索框在窗口很上方，紧贴「使用此目录」上方的 Edit 才是目录输入框。
    """
    button = find_button(window, "使用此目录")
    button_rect = button.rectangle()
    candidates = [
        edit for edit in window.descendants(control_type="Edit") if edit.rectangle().bottom <= button_rect.top
    ]
    if not candidates:
        msg = "设置页未找到 Xberg 目录输入框"
        raise RuntimeError(msg)
    edit = min(candidates, key=lambda e: button_rect.top - e.rectangle().bottom)
    edit.set_edit_text(path)


def wait_text_containing(
    window: WindowSpecification,
    needle: str,
    timeout: int = COMPLETION_TIMEOUT,
    exclude: str = "",
) -> str:
    """等待任一 Text 控件包含 needle 并返回其全文（设置/转换页状态断言的共用判据）.

    exclude 非空时跳过与它完全相同的旧文案——重试类断言用它确保等到的是
    新一次操作的结果，而不是仍留在界面上的上一次输出。
    """
    deadline = time.time() + timeout
    last_seen = ""
    while time.time() < deadline:
        try:
            for text in window.descendants(control_type="Text"):
                value = text.window_text() or ""
                if needle in value and value != exclude:
                    return value
                if value:
                    last_seen = value
        except TRANSIENT_GUI_ERRORS:
            pass
        time.sleep(0.5)
    msg = f"等待界面文本「{needle}」超时（最后可见文本：{last_seen[:200]}）"
    raise RuntimeError(msg)


def find_check(window: WindowSpecification, title: str) -> WindowSpecification:
    """按标题返回 CheckBox 控件规格（与 find_button 同构，类型注解一致）.

    可见性由调用处的 wait 保证。
    """
    return window.child_window(title=title, control_type="CheckBox")


def isolated_state_env(scratch_state: Path) -> dict[str, str]:
    """S6-S9 的子进程环境：应用配置与截图资产根全部隔离到临时目录.

    JCHTOOLS_TEST_STATE_DIR（src/xberg_settings.rs，test-hooks 构建识别）隔离
    config.sqlite3，避免冒烟改写真实用户配置（XB-18）；JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT
    （src/snap_ocr_assets.rs）同时隔离截图服务管道名与 worker 安装位置——S6/S7
    保存有效目录会触发 ensure_snap_supervisor，不隔离会 ping 真实用户会话的
    后台服务、改写其 launcher.json 甚至启动真实 worker（XB-22/XB-25）。
    """
    env = dict(os.environ)
    env["JCHTOOLS_TEST_STATE_DIR"] = str(scratch_state)
    env["JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT"] = str(scratch_state / "snap-assets")
    return env


def smoke_xberg_dir() -> str:
    """S6/S7 需要的有效 Xberg 目录（含 xberg.exe；由验收环境提供）."""
    directory = os.environ.get("JCHTOOLS_SMOKE_XBERG_DIR", "")
    if not directory or not Path(directory).joinpath("xberg.exe").is_file():
        message = "S6/S7 需要包含 xberg.exe 的有效 Xberg 目录：设置 JCHTOOLS_SMOKE_XBERG_DIR 指向该目录"
        raise RuntimeError(message)
    return str(Path(directory).resolve())


def s6_save_xberg_directory(exe: str) -> None:
    """S6 设置页选择并保存 Xberg 目录：校验通过、状态显示已持久保存."""
    state = Path(tempfile.mkdtemp(prefix="jchtools-gui-smoke-s6-"))
    directory = smoke_xberg_dir()
    try:

        def body(window: WindowSpecification) -> None:
            goto_settings(window)
            set_settings_custom_dir(window, directory)
            click(window, find_button(window, "使用此目录"))
            _ = wait_text_containing(window, SETTINGS_SAVED_TEXT)

        run_stage("S6", exe, body, env=isolated_state_env(state))
    finally:
        shutil.rmtree(state, ignore_errors=True)
    print("S6 PASS：Xberg 目录经「使用此目录」校验并持久保存（应用配置已隔离）")


def s7_restart_restores_saved_directory(exe: str) -> None:
    """S7 重启恢复：保存后完全退出，再启动时来源与目录自动恢复（XB-18/XB-21）."""
    state = Path(tempfile.mkdtemp(prefix="jchtools-gui-smoke-s7-"))
    directory = smoke_xberg_dir()
    env = isolated_state_env(state)
    try:

        def save(window: WindowSpecification) -> None:
            goto_settings(window)
            set_settings_custom_dir(window, directory)
            click(window, find_button(window, "使用此目录"))
            _ = wait_text_containing(window, SETTINGS_SAVED_TEXT)

        def restored(window: WindowSpecification) -> None:
            goto_settings(window)
            _ = wait_text_containing(window, SETTINGS_SAVED_TEXT)
            _ = wait_text_containing(window, "当前来源：用户提供的目录")

        run_stage("S7-first", exe, save, env=env)
        run_stage("S7", exe, restored, env=env)
    finally:
        shutil.rmtree(state, ignore_errors=True)
    print("S7 PASS：重启后自动恢复保存的 Xberg 目录与来源，无需重新输入")


def s8_invalid_directory_is_rejected_and_retryable(exe: str) -> None:
    """S8 无效目录提示与重新选择：不存在的路径与缺 xberg.exe 的目录都被明确拒绝."""
    state = Path(tempfile.mkdtemp(prefix="jchtools-gui-smoke-s8-"))
    empty = state / "empty-dir"
    empty.mkdir()
    env = isolated_state_env(state)
    try:

        def body(window: WindowSpecification) -> None:
            goto_settings(window)
            # 校验失败文案固定以「Xberg 资产 … 缺失」指认首个缺失成员
            # （不存在的路径与空目录都走这条口径；初始文案不含该前缀）。
            # 1) 不存在的路径：明确报错并指认缺失（不是无声失败）。
            set_settings_custom_dir(window, str(state / "does-not-exist"))
            click(window, find_button(window, "使用此目录"))
            first = wait_text_containing(window, "Xberg 资产")
            require("缺失" in first, f"错误应指认缺失项：{first}")
            # 2) 存在但缺 xberg.exe 的目录：同样被拒绝（排除上一次的旧文案）。
            set_settings_custom_dir(window, str(empty))
            click(window, find_button(window, "使用此目录"))
            second = wait_text_containing(window, "Xberg 资产", exclude=first)
            require("缺失" in second, f"错误应指认缺失项：{second}")
            # 3) 重新选择：界面未锁死，可再次输入并触发校验（状态重新进入处理中）。
            set_settings_custom_dir(window, str(state / "another-invalid"))
            click(window, find_button(window, "使用此目录"))
            _ = wait_text_containing(window, "Xberg 资产", exclude=second)

        run_stage("S8", exe, body, env=env)
    finally:
        shutil.rmtree(state, ignore_errors=True)
    print("S8 PASS：无效目录明确拒绝且可重新选择（提示指认缺失项，不锁死界面）")


def s9_unconfigured_state_shows_reason_and_no_autostart(exe: str) -> None:
    """S9 组件缺失状态与设置页指引：未配置时如实显示未就绪，不自动下载."""
    state = Path(tempfile.mkdtemp(prefix="jchtools-gui-smoke-s9-"))
    env = isolated_state_env(state)
    try:

        def body(window: WindowSpecification) -> None:
            # 切页点击可能落空：以目标状态文本出现为准重试（与 goto_settings 同口径）。
            _ = click_and_wait_text(window, "转 Markdown", CONVERT_NEED_RUNTIME_TEXT)
            start = find_button(window, "开始转换")
            _ = start.wait("visible", timeout=TIMEOUT)
            require(not start.is_enabled(), "未配置 Xberg 时「开始转换」必须禁用")
            # 转换页初始化入口已移除（S3-01：初始化集中到设置页，保存/下载自动
            # 完成）；未就绪时的正确指引是「前往设置」。
            _ = find_button(window, "前往设置").wait("visible", timeout=TIMEOUT)
            goto_settings(window)
            # 「尚未下载 Xberg」绑定 settings-downloaded-dir 的静态空值，不依赖
            # 异步 SETTINGS_READY 事件的时序（事件文案在连跑时序下可能尚未到位）。
            _ = wait_text_containing(window, "尚未下载 Xberg")
            _ = find_button(window, "下载 Xberg").wait("visible enabled", timeout=TIMEOUT)
            # O-06：不做任何下载动作——缺失状态如实可见即可，冒烟不得触发真实联网。

        run_stage("S9", exe, body, env=env)
    finally:
        shutil.rmtree(state, ignore_errors=True)
    print("S9 PASS：未配置状态如实显示原因与初始化入口，开始按钮禁用（不自动下载）")


def check_flat_output(window: WindowSpecification) -> None:
    """勾选「平铺输出」.

    Slint 的 Check 不暴露 UIA TogglePattern，无法直读勾选状态；用随 convert-flat
    切换的提示文案（app.slint 输出目录卡片）作为状态锚：点击后等「平铺时同名结果
    只处理排序第一份」出现，未出现按点击落空重试（奇数次点击收敛到勾选态）。
    """
    _ = find_check(window, "平铺输出").wait("visible", timeout=TIMEOUT)
    for _attempt in range(3):
        click(window, find_check(window, "平铺输出"))
        try:
            _ = wait_text_containing(window, "平铺时同名结果只处理排序第一份", timeout=8)
        except RuntimeError:
            continue
        return
    msg = "「平铺输出」未能勾选（状态提示始终未切换为平铺文案）"
    raise RuntimeError(msg)


def produced_snapshot(directory: Path) -> frozenset[tuple[str, int, int]]:
    """输出目录产物快照（相对路径、mtime_ns、大小）；目录不可读时返回空快照.

    转换页没有可观察的任务计数或时间戳文本（收尾统计行的总耗时只保留 0.1s 精度，
    两轮可以逐字相同），用「点击开始前后的产物快照变化」作任务代次锚（S11-06）。

    全有或全无语义（复审修正）：枚举或 stat 中途失败（Windows 写锁/杀软扫描
    句柄）返回空快照而不是部分集合——部分快照相对完整 before 必然「已变化」，
    会把陈旧完成行误判为新完成；空快照在 completion_confirms_new_run 侧被
    视为「无锚」（与 None 同路），宁可退回文本判据也不假锚。
    """
    snapshot: list[tuple[str, int, int]] = []
    try:
        for path in directory.rglob("*"):
            if path.is_file():
                info = path.stat()
                snapshot.append((str(path.relative_to(directory)), info.st_mtime_ns, info.st_size))
    except OSError:
        return frozenset()
    return frozenset(snapshot)


def completion_confirms_new_run(
    value: str,
    previous_done: str,
    produced_before: frozenset[tuple[str, int, int]] | None,
    produced_now: frozenset[tuple[str, int, int]] | None,
) -> bool:
    """S11-06 新一轮批次完成判据（纯函数；单测见 scripts/test_gui_smoke.py）.

    含完成标记且统计行文本不同于上一轮 → 新完成；文本与上一轮逐字相同时，若
    调用方提供了输出快照且快照已变化（新产出/更新了产物文件）也判为新一轮完成。
    空快照按「无锚」处理（与 None 同路）：真实转换必然产出至少一个文件，空快照
    只可能是目录不可读（部分快照已被 produced_snapshot 归零），不得当「已变化」。
    纯跳过批次若统计行也逐字相同则不存在任何可观察锚点（现有各阶段的成功/跳过
    计数必然变化，不命中该死角）。
    """
    if CONVERT_DONE_MARKER not in value:
        return False
    if value != previous_done:
        return True
    if not produced_now:
        return False
    return produced_before is not None and produced_now != produced_before


def _confirm_new_completion(
    value: str,
    previous_done: str,
    produced_dir: Path | None,
    before: frozenset[tuple[str, int, int]] | None,
) -> bool:
    """以 completion_confirms_new_run 判定 value；快照只在完成标记在场后才取（S11-06）."""
    if CONVERT_DONE_MARKER not in value:
        return False
    now = produced_snapshot(produced_dir) if produced_dir is not None else None
    return completion_confirms_new_run(value, previous_done, before, now)


def _wait_completion_with_anchor(
    window: WindowSpecification,
    previous_done: str,
    produced_dir: Path | None,
    before: frozenset[tuple[str, int, int]] | None,
) -> str:
    """已确认进入运行态后等待本次收尾统计行（S11-06：快照锚兜底）.

    不能用 exclude=previous_done 的纯文本等待——统计行与上一轮逐字相同时会把
    新完成误判为旧文案残留而超时假失败，由产物快照锚兜底识别。
    """
    deadline = time.time() + COMPLETION_TIMEOUT
    last_seen = ""
    while time.time() < deadline:
        try:
            for text in window.descendants(control_type="Text"):
                value = text.window_text() or ""
                if _confirm_new_completion(value, previous_done, produced_dir, before):
                    return value
                if value:
                    last_seen = value
        except TRANSIENT_GUI_ERRORS:
            pass
        time.sleep(0.5)
    msg = f"等待新一轮转换收尾统计超时（上一轮统计：{previous_done[:120]}；最后可见文本：{last_seen[:200]}）"
    raise RuntimeError(msg)


def start_conversion_and_wait_done(
    window: WindowSpecification,
    previous_done: str = "",
    produced_dir: Path | None = None,
) -> str:
    """点击「开始转换」（带重试）并等待批次完成，返回新的完成统计行文本.

    小批次可能在 UIA 轮询间隔内整批完成，「停止任务」一闪而过——启动成功的
    判据是「停止任务」出现**或**出现与上一轮不同的完成统计行（总耗时数值必变），
    两者任一即停止重试点击；只有两者都不出现（点击落空或按钮不可用）才重按。
    produced_dir 非空时在统计行与上一轮逐字相同的情况下，以「输出目录产物快照
    相对点击前的变化」作任务代次锚识别新完成（S11-06），避免误按开始导致假失败。
    """
    before = produced_snapshot(produced_dir) if produced_dir is not None else None
    finished = ""
    for attempt in range(3):
        click(window, find_button(window, "开始转换"))
        deadline = time.time() + CONVERT_BUSY_TIMEOUT
        while time.time() < deadline:
            try:
                if find_button(window, "停止任务").exists(timeout=0.1):
                    finished = ""
                    break
                for text in window.descendants(control_type="Text"):
                    value = text.window_text() or ""
                    if _confirm_new_completion(value, previous_done, produced_dir, before):
                        finished = value
                        break
            except TRANSIENT_GUI_ERRORS:
                pass
            if finished:
                break
            time.sleep(0.5)
        if finished:
            return finished
        if attempt == _S5_LAST_ATTEMPT:
            _ = find_button(window, "停止任务").wait("visible enabled", timeout=TIMEOUT)
            msg = "转换既未进入运行态也未完成（开始按钮可能未生效）"
            raise RuntimeError(msg)
    # 上方重试已确认进入运行态（「停止任务」出现过），交给快照锚感知的收尾等待。
    return _wait_completion_with_anchor(window, previous_done, produced_dir, before)


def s10_output_layout_and_flat_duplicate_policy(exe: str) -> None:
    """S10 输出层级（保留/平铺）与同名策略：层次两份、平铺一份+重复跳过计数."""
    scratch = Path(tempfile.mkdtemp(prefix="jchtools-gui-smoke-s10-"))
    source = scratch / "input"
    layered = scratch / "out-layered"
    flat = scratch / "out-flat"
    for part in ("a", "b"):
        (source / part).mkdir(parents=True)
        _ = (source / part / "同名.txt").write_text("same name, different path\n", encoding="utf-8")
    layered.mkdir()
    flat.mkdir()
    try:

        def body(window: WindowSpecification) -> None:
            goto_converter(window)
            # 层次模式：两份同名输入各自成文（T-11 默认保留层级）。
            set_converter_dirs(window, str(source), str(layered))
            metrics = start_conversion_and_wait_done(window, produced_dir=layered)
            require((layered / "a" / "同名_txt.md").is_file(), "层次输出应保留相对层级 a/")
            require((layered / "b" / "同名_txt.md").is_file(), "层次输出应保留相对层级 b/")
            require("重复结果跳过 0" in metrics, f"层次模式不应有同名跳过：{metrics}")
            # 平铺模式：同名只处理排序第一份，其余按重复跳过计数。
            set_converter_dirs(window, str(source), str(flat))
            check_flat_output(window)
            metrics = start_conversion_and_wait_done(window, previous_done=metrics, produced_dir=flat)
            produced = sorted(p.name for p in flat.glob("*.md"))
            require(produced == ["同名_txt.md"], f"平铺只应有一份结果：{produced}")
            require("重复结果跳过 1" in metrics, f"平铺同名跳过必须如实计数：{metrics}")

        run_stage("S10", exe, body)
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
    print("S10 PASS：层级保留两份同名输入；平铺只处理一份并如实计数重复跳过")


S11_SOURCE_FILES = ("one.txt", "two.txt")


def s11_existing_results_are_skipped_untouched(exe: str) -> None:
    """S11 已有结果跳过：重复运行不覆盖既有产物，跳过数量如实显示（T-12）."""
    scratch = Path(tempfile.mkdtemp(prefix="jchtools-gui-smoke-s11-"))
    source = scratch / "input"
    output = scratch / "out"
    source.mkdir()
    output.mkdir()
    for name in S11_SOURCE_FILES:
        _ = (source / name).write_text(f"content of {name}\n", encoding="utf-8")
    try:

        def body(window: WindowSpecification) -> None:
            goto_converter(window)
            set_converter_dirs(window, str(source), str(output))
            first = start_conversion_and_wait_done(window, produced_dir=output)
            require(
                "已有结果跳过 0" in first or "跳过" not in first,
                f"首轮应全部转换：{first}",
            )
            existing = {path.name: (file_digest(path), path.stat().st_mtime_ns) for path in output.glob("*.md")}
            require(
                len(existing) == len(S11_SOURCE_FILES),
                f"前置：两份产物（实得 {list(existing)}）",
            )
            second = start_conversion_and_wait_done(window, previous_done=first, produced_dir=output)
            require(
                f"已有结果跳过 {len(S11_SOURCE_FILES)}" in second,
                f"重复运行必须如实显示跳过数量：{second}",
            )
            for path in output.glob("*.md"):
                before = existing[path.name]
                require(
                    (file_digest(path), path.stat().st_mtime_ns) == before,
                    f"已有结果的内容与修改时间必须保持不变（T-12）：{path.name}",
                )

        run_stage("S11", exe, body)
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
    print("S11 PASS：已有结果跳过不覆盖，内容与修改时间不变，跳过数量如实显示")


def _tiny_png(width: int = 16, height: int = 16) -> bytes:
    """构造最小合法灰度 PNG（无文字，转换成功即可，OCR 内容不作断言）."""
    raw = b"".join(b"\x00" + b"\xff" * width for _ in range(height))

    def chunk(kind: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 0, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw))
        + chunk(b"IEND", b"")
    )


def s12_output_subtree_is_excluded_from_scan(exe: str) -> None:
    """S12 输出子树排除：输出目录位于输入内时整棵输出子树不作为新输入（T-09）."""
    scratch = Path(tempfile.mkdtemp(prefix="jchtools-gui-smoke-s12-"))
    source = scratch / "input"
    output = source / "results"
    (source / "prep").mkdir(parents=True)
    _ = (source / "prep" / "page.png").write_bytes(_tiny_png())
    output.mkdir()
    _ = (output / "inner.png").write_bytes(_tiny_png())
    try:

        def body(window: WindowSpecification) -> None:
            goto_converter(window)
            set_converter_dirs(window, str(source), str(output))
            metrics = start_conversion_and_wait_done(window, produced_dir=output)
            require(
                (output / "prep" / "page_png.md").is_file(),
                f"输出子树外的输入应正常转换：{metrics}",
            )
            require(
                not (output / "inner_png.md").exists(),
                "输出子树内的文件不得作为新输入",
            )
            require(
                not (output / "results" / "inner_png.md").exists(),
                "不得重复嵌套输出子树",
            )
            require((output / "inner.png").is_file(), "输出子树内的源文件保持原样")

        run_stage("S12", exe, body)
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
    print("S12 PASS：输出子树整树排除，不因生成了 Markdown 而重复转换")


def s13_partial_failure_isolated_with_counts(exe: str) -> None:
    """S13 部分失败与完成统计：单文件失败不终止批次，成功/失败可区分（T-16/T-24）."""
    scratch = Path(tempfile.mkdtemp(prefix="jchtools-gui-smoke-s13-"))
    source = scratch / "input"
    output = scratch / "out"
    source.mkdir()
    output.mkdir()
    _ = (source / "good.txt").write_text("convertible text\n", encoding="utf-8")
    _ = (source / "broken.png").write_bytes(b"\x89PNG\r\n\x1a\ntruncated")
    try:

        def body(window: WindowSpecification) -> None:
            goto_converter(window)
            set_converter_dirs(window, str(source), str(output))
            metrics = start_conversion_and_wait_done(window, produced_dir=output)
            require("成功 1" in metrics, f"可转换文件必须成功（统计行：{metrics}）")
            require("失败 1" in metrics, f"损坏文件必须计入失败（统计行：{metrics}）")
            require((output / "good_txt.md").is_file(), "成功产物必须在场")
            require(
                not (output / "broken_png.md").exists(),
                "失败文件不得留下半成品（T-25）",
            )

        run_stage("S13", exe, body)
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
    print("S13 PASS：部分失败不终止批次，成功/失败计数与产物边界一致")


def close_title_bar(window: WindowSpecification) -> None:
    """直接向目标 GUI 窗口发送 WM_CLOSE，并等待停止确认弹窗出现."""
    handle = window.handle
    win32gui.PostMessage(handle, win32con.WM_CLOSE, 0, 0)
    _ = wait_text_containing(window, STOP_AND_CLOSE_TITLE, timeout=TIMEOUT)


def s14_close_during_conversion_confirms_and_stops(exe: str) -> None:
    """S14 运行中关闭确认与安全停止：U-09 确认框 → T-23 安全停止 → 退出码 0."""
    scratch_box: list[Path] = []

    def prepare_scratch() -> None:
        scratch = Path(tempfile.mkdtemp(prefix="jchtools-gui-smoke-s14-"))
        (scratch / "input").mkdir()
        (scratch / "output").mkdir()
        media = os.environ.get("JCHTOOLS_S5_MEDIA", "")
        if not media or not Path(media).is_file():
            message = "S14 需要媒体样本以保持转换运行态：设置 JCHTOOLS_S5_MEDIA 指向一个真实媒体文件"
            raise RuntimeError(message)
        for index in range(STOP_MEDIA_FILE_COUNT):
            _ = shutil.copyfile(media, scratch / "input" / f"{index:02}_{Path(media).name}")
        scratch_box.append(scratch)

    def body(window: WindowSpecification) -> None:
        scratch = scratch_box[0]
        goto_converter(window)
        set_converter_dirs(window, str(scratch / "input"), str(scratch / "output"))
        for attempt in range(3):
            click(window, find_button(window, "开始转换"))
            try:
                _ = find_button(window, "停止任务").wait("visible enabled", timeout=CONVERT_BUSY_TIMEOUT // 3)
                break
            except timings.TimeoutError:
                if attempt == _S5_LAST_ATTEMPT:
                    raise
        close_title_bar(window)
        click(window, find_button(window, STOP_AND_CLOSE_BUTTON))
        # 确认后任务在当前文件结束（T-23），进程随后自然退出；退出断言由 run_stage 收尾执行。

    def cleanup() -> None:
        shutil.rmtree(scratch_box[0], ignore_errors=True)

    run_stage("S14", exe, body, pre=prepare_scratch, after=cleanup)
    print("S14 PASS：运行中关闭弹出「停止任务并关闭」，确认后安全停止并退出")


def set_edit_before_button(window: WindowSpecification, button: BaseWrapper, value: str) -> None:
    """填写选择按钮左侧最近的同排输入框，避免误选文件名或侧栏搜索框."""
    rect = button.rectangle()
    candidates = [
        edit
        for edit in window.descendants(control_type="Edit")
        if abs(edit.rectangle().top - rect.top) < EDIT_ROW_TOLERANCE_PX and edit.rectangle().right <= rect.left
    ]
    require(candidates, "未找到选择按钮左侧输入框")
    min(candidates, key=lambda edit: rect.left - edit.rectangle().right).set_edit_text(value)


def s15_markdown_merge_and_split(exe: str) -> None:
    """覆盖 M-01/M-04/M-08/M-10：真实页面到可核对的合并与拆分产物."""
    with tempfile.TemporaryDirectory(prefix="jchtools-gui-md-") as temporary:
        scratch = Path(temporary)
        source, merged, split = (scratch / name for name in ("input", "merged", "split"))
        for directory in (source, merged, split):
            directory.mkdir()
        originals = {"a.md": b"# Alpha\nfirst\n", "b.md": "# 中文\n第二份\n".encode()}
        for name, content in originals.items():
            _ = (source / name).write_bytes(content)
        split_source = scratch / "document.md"
        original = ("中文与 UTF-8 分片测试\n" * 150).encode()
        _ = split_source.write_bytes(original)

        def body(window: WindowSpecification) -> None:
            click(window, find_button(window, "MD 整理"))
            rows = unique_visible_buttons(window, "选择目录…")
            require(len(rows) == CONVERT_DIR_ROWS, "合并页应有输入与输出目录两行")
            for button, directory in zip(rows, (source, merged), strict=True):
                set_edit_before_button(window, button, str(directory))
            click(window, find_button(window, "开始合并"))
            _ = wait_text_containing(window, "合并完成")
            result = (merged / "merged.md").read_text(encoding="utf-8")
            require("# a.md" in result and "## Alpha" in result, "合并必须保留文件标题并下移正文标题")
            require("# b.md" in result and "## 中文" in result, "中文 Markdown 必须正常合并")
            for name, content in originals.items():
                require((source / name).read_bytes() == content, "合并不得改写输入")
            click(window, window.child_window(title="拆分 MD", control_type="TabItem"))
            files = unique_visible_buttons(window, "选择文件…")
            directories = unique_visible_buttons(window, "选择目录…")
            require(len(files) == 1 and len(directories) == 1, "拆分页应有文件与输出目录入口")
            set_edit_before_button(window, files[0], str(split_source))
            set_edit_before_button(window, directories[0], str(split))
            click(window, find_button(window, "开始拆分"))
            _ = wait_text_containing(window, "拆分完成")
            pieces = sorted(split.glob("document_*.md"))
            require(len(pieces) > 1, "1 KB 上限应产生多个分片")
            require(b"".join(path.read_bytes() for path in pieces) == original, "分片拼接必须逐字节还原")
            require(all(path.stat().st_size <= MD_SPLIT_LIMIT_BYTES for path in pieces), "分片不得超过 1 KB")
            require(split_source.read_bytes() == original, "拆分不得改写原文件")

        run_stage("S15", exe, body)
    print("S15 PASS：MD 合并与拆分产物正确，原文件不变，UTF-8 分片可无损拼接")


def s16_git_commit_and_push(exe: str) -> None:
    """覆盖 G-01/G-04/G-05/G-13：真实 GUI 到本地 bare 远端的逐文件提交推送."""
    git = shutil.which("git")
    require(git is not None, "Git GUI 验证需要可用 git")
    if git is None:
        return
    with tempfile.TemporaryDirectory(prefix="jchtools-gui-git-") as temporary:
        scratch = Path(temporary)
        repo, remote = scratch / "repo", scratch / "remote.git"
        repo.mkdir()

        def git_output(directory: Path, *args: str) -> str:
            result = subprocess.run([git, *args], cwd=directory, capture_output=True, text=True, check=True)
            return result.stdout.strip()

        _ = git_output(scratch, "init", "--bare", str(remote))
        _ = git_output(repo, "init", "--initial-branch=main")
        _ = git_output(repo, "config", "user.name", "Synthetic GUI test")
        _ = git_output(repo, "config", "user.email", "gui-test@local")
        _ = (repo / "one.txt").write_text("initial\n", encoding="utf-8")
        _ = git_output(repo, "add", "one.txt")
        _ = git_output(repo, "commit", "-m", "initial synthetic fixture")
        _ = git_output(repo, "remote", "add", "origin", str(remote))
        _ = git_output(repo, "push", "--set-upstream", "origin", "main")
        _ = (repo / "one.txt").write_text("changed\n", encoding="utf-8")
        _ = (repo / "two.txt").write_text("second\n", encoding="utf-8")

        def body(window: WindowSpecification) -> None:
            click(window, find_button(window, "Git 工具"))
            set_directory(window, str(repo))
            click(window, find_button(window, "开始"))
            _ = wait_text_containing(window, "全部完成：2 / 2")
            require(git_output(repo, "status", "--porcelain") == "", "GUI 操作后工作树必须干净")
            require(
                git_output(repo, "rev-parse", "HEAD") == git_output(remote, "rev-parse", "main"), "远端必须收到全部提交"
            )
            for commit in ("HEAD", "HEAD~1"):
                files = git_output(repo, "diff-tree", "--no-commit-id", "--name-only", "-r", commit).splitlines()
                require(len(files) == 1, "每个 GUI 提交只能包含一个文件")
            require(git_output(remote, "show", "main:one.txt") == "changed", "远端修改内容必须正确")
            require(git_output(remote, "show", "main:two.txt") == "second", "远端新增内容必须正确")

        run_stage("S16", exe, body)
    print("S16 PASS：GUI 逐文件提交并推送成功，本地工作树与远端内容正确")


def s17_initialize_configured_components(exe: str) -> None:
    """覆盖 T-05/T-06：经设置页真实保存入口初始化组件，不伪造就绪标记."""
    engine_dir = os.environ.get("JCHTOOLS_TEST_XBERG_DIR", "").strip()
    require(
        bool(engine_dir) and Path(engine_dir, "xberg.exe").is_file(),
        "S17 需要 JCHTOOLS_TEST_XBERG_DIR 指向含 xberg.exe 的真实测试引擎目录",
    )

    def body(window: WindowSpecification) -> None:
        # 隔离 SQLite 已由 --seed-state 播种目录来源，但许可证 notice 尚未落盘；
        # 转换页初始化入口移除后（S3-01），初始化随「使用此目录」真实保存自动
        # 完成（保存成功即写 notice，S5-03 同源闭环）。保存为异步动作，就绪探
        # 测可能在保存完成前发生，用重试收敛。
        for _attempt in range(3):
            goto_converter(window)
            try:
                _ = wait_text_containing(window, "已安装组件就绪，可离线使用", timeout=ATTEMPT_TIMEOUT)
                break
            except RuntimeError:
                pass
            goto_settings(window)
            set_settings_custom_dir(window, engine_dir)
            click(window, find_button(window, "使用此目录"))
        else:
            msg = "S17：经设置页保存真实引擎目录后组件仍未就绪"
            raise RuntimeError(msg)

    run_stage("S17", exe, body)
    print("S17 PASS：真实 GUI 经设置页保存并自动初始化成功，已配置的 Xberg 可离线使用")


def parse_stages(stages_arg: str) -> list[str]:
    """解析并校验 --stages：逗号分隔、大小写不敏感、未知阶段立即失败."""
    stages = [token.strip().upper() for token in stages_arg.split(",") if token.strip()]
    unknown = [stage for stage in stages if stage not in SUPPORTED_STAGES]
    if unknown:
        msg = f"未知阶段：{unknown}（可选：{list(SUPPORTED_STAGES)}）"
        raise RuntimeError(msg)
    return stages


def missing_isolation_env(env: Mapping[str, str]) -> list[str]:
    """返回隔离守卫要求但当前环境缺失的变量键（纯函数；单测见 scripts/test_gui_smoke.py）."""
    return [key for key in ISOLATION_REQUIRED_ENV_KEYS if not env.get(key)]


def require_orchestrated_isolation(stages: list[str], env: Mapping[str, str], *, allow_isolated_run: bool) -> None:
    """S11-07 统一隔离守卫：受守卫阶段未经编排隔离不得独立运行.

    受守卫阶段（S5/S10-S14/S17）的 run_stage 不自建隔离环境，独立调用会读写真实
    用户状态目录并可能连接常驻服务；在启动任何 GUI 之前整批拒绝，避免半程执行。
    经 acceptance.ps1 编排（或已手工布置同等隔离变量）时两个变量在场，不受影响；
    组件目录（Xberg 运行目录）允许为真实引擎目录，不做隔离检查。
    """
    guarded = [stage for stage in stages if stage in ISOLATION_GUARDED_STAGES]
    if not guarded or allow_isolated_run:
        return
    missing = missing_isolation_env(env)
    if not missing:
        return
    msg = (
        f"阶段 {','.join(guarded)} 独立运行会读写真实应用状态目录并可能连接常驻服务，"
        f"当前缺少隔离环境变量：{','.join(missing)}。"
        "请经 scripts/acceptance.ps1 执行（-WithGuiSmoke / -WithMarkdownAcceptance 会布置隔离"
        "状态目录与资产根）；确需独立运行时显式加 --allow-isolated-run 自担隔离责任。"
    )
    raise RuntimeError(msg)


def run_dataset_stages(exe: str, data: Path, stages: list[str]) -> None:
    """S4/S2/S3 数据集阶段：每阶段前从旁路副本恢复，保证“干净语料上的完整链路”.

    H-06/附录 E：所选根的任一祖先直接含 .git 时两工具拒绝整次处理。仓库根本身
    带 .git，冒烟副本若继续放在仓库 .tmp/ 下会整次被拒（S4 确认框不再出现）。
    改放系统临时目录（AGENTS §2 允许的 tempfile 例外；语料源目录不受影响）。
    """
    scratch = Path(tempfile.mkdtemp(prefix="jchtools-gui-smoke-"))
    try:
        # 顺序 S4→S2→S3 固定（元组序即原顺序）；S3 为末段，随后的清理由
        # finally 的 scratch 删除兜底（循环末尾多出的一次 fresh 删除无输出、
        # 无异常，可观察行为不变）。
        fresh = scratch / "data"
        for stage, runner in (("S4", s4_full_extract), ("S2", s2_analyze_only), ("S3", s3_full_organize)):
            if stage not in stages:
                continue
            _ = shutil.copytree(data, fresh)
            runner(str(exe), str(fresh))
            shutil.rmtree(fresh, ignore_errors=True)
    finally:
        shutil.rmtree(scratch, ignore_errors=True)


def ensure_smoke_target_is_disposable(data: Path) -> None:
    """整理/解压阶段会真实改动数据：只允许对一次性测试副本操作.

    仓库内仅放行 .tmp/（默认 .tmp/gui-smoke/data 就在这里）；用户主目录与
    盘符根一律拒绝。
    """
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


def main() -> int:
    parser = argparse.ArgumentParser(description="JchTools GUI 冒烟测试")
    _ = parser.add_argument("--exe", default=DEFAULT_EXE)
    _ = parser.add_argument("--data", default=DEFAULT_DATA)
    _ = parser.add_argument(
        "--stages", default=DEFAULT_STAGES, help=f"逗号分隔阶段清单（可选：{','.join(SUPPORTED_STAGES)}）"
    )
    _ = parser.add_argument("--list-stages", action="store_true", help="逐行打印支持的阶段号后退出")
    _ = parser.add_argument(
        "--allow-isolated-run",
        action="store_true",
        help="允许受隔离守卫的阶段（S5/S10-S14/S17）在本进程独立运行（默认需经 acceptance.ps1 布置的隔离环境）",
    )
    args = parser.parse_args(namespace=_CliArgs())
    if args.list_stages:
        for stage in SUPPORTED_STAGES:
            print(stage)
        return 0
    stages = parse_stages(args.stages)
    # S11-07：受守卫阶段在启动任何 GUI 之前统一校验隔离环境，缺则拒绝并指引入口。
    require_orchestrated_isolation(stages, os.environ, allow_isolated_run=args.allow_isolated_run)
    exe = Path(args.exe).resolve()
    data = Path(args.data).resolve()
    if not exe.is_file():
        msg = f"找不到 {exe}"
        raise RuntimeError(msg)
    if not data.is_dir():
        msg = f"找不到测试数据目录 {data}"
        raise RuntimeError(msg)
    ensure_smoke_target_is_disposable(data)

    if "S1" in stages:
        s1_launch_and_exit(str(exe))
    run_dataset_stages(str(exe), data, stages)
    stage_runners = {
        "S5": s5_markdown_basic_chain,
        "S6": s6_save_xberg_directory,
        "S7": s7_restart_restores_saved_directory,
        "S8": s8_invalid_directory_is_rejected_and_retryable,
        "S9": s9_unconfigured_state_shows_reason_and_no_autostart,
        "S10": s10_output_layout_and_flat_duplicate_policy,
        "S11": s11_existing_results_are_skipped_untouched,
        "S12": s12_output_subtree_is_excluded_from_scan,
        "S13": s13_partial_failure_isolated_with_counts,
        "S14": s14_close_during_conversion_confirms_and_stops,
        "S15": s15_markdown_merge_and_split,
        "S16": s16_git_commit_and_push,
        "S17": s17_initialize_configured_components,
    }
    for stage, runner in stage_runners.items():
        if stage in stages:
            runner(str(exe))
    return 0


if __name__ == "__main__":
    sys.exit(main())
