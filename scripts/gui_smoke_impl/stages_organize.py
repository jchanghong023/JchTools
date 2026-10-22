"""S1-S4：目录整理与递归解压阶段。.

由 gui_smoke.py 按域拆出；行为与拆分前一致，入口 scripts/gui_smoke.py 负责 re-export。
"""

from __future__ import annotations

import os
import sys
from pathlib import Path
from typing import TYPE_CHECKING

if not __package__:
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

if TYPE_CHECKING:

    from pywinauto.application import WindowSpecification

from scripts.gui_smoke_impl import config, convert_ui, driver, postconditions, records


def s1_launch_and_exit(exe: str) -> None:
    driver.run_stage("S1", exe, lambda _window: print("S1 PASS：窗口启动并可见"))


def goto_organizer(window: WindowSpecification) -> None:
    """启动落在注册表第一个工具（递归解压）；目录整理用例先切过去."""
    driver.click(window, driver.find_button(window, "目录整理"))


def analyze_until_ready(window: WindowSpecification, data: str) -> frozenset[str]:
    """S2/S3 共用前缀：切到目录整理、设置数据集并分析到 ready，返回全部旧任务 ID。."""
    goto_organizer(window)
    driver.setup_directory(window, data)
    baseline = records.task_ids(data)
    # C-01：分析只读，不再弹破坏性确认框——点击后直接进入分析。
    driver.click(window, driver.find_button(window, "开始分析"))
    records.wait_task_status(data, "ready", baseline)
    return baseline


def s2_analyze_only(exe: str, data: str) -> None:
    def body(window: WindowSpecification) -> None:
        root = Path(data)
        before_paths = frozenset(path.relative_to(root) for path in root.rglob("*"))
        before_files = {
            path.relative_to(root): (postconditions.file_digest(path), path.stat().st_mtime_ns)
            for path in root.rglob("*")
            if path.is_file()
        }
        _ = analyze_until_ready(window, data)
        _ = driver.find_button(window, "确认并执行整理").wait("visible enabled", timeout=config.TIMEOUT)
        after_paths = frozenset(path.relative_to(root) for path in root.rglob("*"))
        after_files = {
            path.relative_to(root): (postconditions.file_digest(path), path.stat().st_mtime_ns)
            for path in root.rglob("*")
            if path.is_file()
        }
        driver.require(
            before_paths == after_paths and before_files == after_files,
            "分析阶段不得改动测试数据（路径、内容或修改时间发生变化）",
        )
        print("S2 PASS：分析完成（计划已生成，执行按钮可用；分析阶段未改动文件）")

    driver.run_stage("S2", exe, body)


def s3_full_organize(exe: str, data: str) -> None:
    root = Path(data)
    expected = postconditions.prepare_organize_witnesses(root)
    git_before = postconditions.tree_snapshot(root / "__gui_witness" / "project")

    def body(window: WindowSpecification) -> None:
        baseline = analyze_until_ready(window, data)
        driver.open_confirm(window, "确认并执行整理")
        driver.confirm_dialog(window)
        records.wait_task_status(data, "finished", baseline)
        postconditions.verify_organize_witnesses(root, expected, git_before)
        for parent, dirs, files in os.walk(data):
            if ".git" in dirs or ".git" in files:
                dirs.clear()
                continue
            if Path(parent) != Path(data) and not dirs and not files:
                msg = f"整理成功后仍残留空目录：{parent}"
                raise RuntimeError(msg)
        before_repeat = postconditions.tree_snapshot(root)
        # 相同目录已保留在页面，重复 set_edit_text 不触发目录变化，状态仍显示上次结束；
        # 从真实「开始分析」入口重新建计划，避免把“目录已就绪”当成唯一空闲状态。
        baseline = records.task_ids(data)
        _ = convert_ui.wait_button(window, "开始分析", config.TIMEOUT, enabled=True)
        driver.click(window, driver.find_button(window, "开始分析"))
        records.wait_task_status(data, "ready", baseline)
        driver.open_confirm(window, "确认并执行整理")
        driver.confirm_dialog(window)
        records.wait_task_status(data, "finished", baseline)
        driver.require(
            postconditions.tree_snapshot(root) == before_repeat,
            "相同默认规则再次整理改变了路径、内容或修改时间",
        )
        print("S3 PASS：默认分类、两类去重保留最新、不同名保留、内容冲突保留、Git整树归类及再次整理幂等")

    driver.run_stage("S3", exe, body)


def s4_full_extract(exe: str, data: str) -> None:
    """递归解压全链路：成功原包删除、既有文件不变，冲突另存、嵌套内容完整落盘."""
    root = Path(data)
    originals: dict[Path, str] = {}

    def snapshot_originals() -> None:
        originals.update(
            {path.relative_to(root): postconditions.file_digest(path) for path in root.rglob("*") if path.is_file()}
        )

    def body(window: WindowSpecification) -> None:
        # 启动页即递归解压，无需切换。
        driver.setup_directory(window, data)
        baseline = records.task_ids(data)
        driver.open_confirm(window, "开始解压")
        driver.confirm_dialog(window, extraction=True)
        records.wait_task_status(data, "finished", baseline)
        postconditions.verify_extraction_results(root, originals)
        print("S4 PASS：成功原包及分卷删除（隔离目录无多余项）；既有内容保留，冲突自动改名，嵌套内容正确落盘")

    driver.run_stage("S4", exe, body, pre=snapshot_originals)


