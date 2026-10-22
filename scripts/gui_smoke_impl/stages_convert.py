"""S5/S10-S14：转 Markdown 阶段与转换驱动入口。.

由 gui_smoke.py 按域拆出；行为与拆分前一致，入口 scripts/gui_smoke.py 负责 re-export。
"""

from __future__ import annotations

import os
import shutil
import sys
import tempfile
import time
from pathlib import Path
from typing import TYPE_CHECKING

from pywinauto import timings

if not __package__:
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

if TYPE_CHECKING:
    from pywinauto.application import WindowSpecification

from scripts.gui_smoke_impl import config, convert_ui, driver, postconditions


def goto_converter(window: WindowSpecification) -> None:
    driver.click(window, driver.find_button(window, "转 Markdown"))


def set_converter_dirs(window: WindowSpecification, input_dir: str, output_dir: str) -> None:
    rows = convert_ui.converter_directory_rows(window)
    for button, value in ((rows[0], input_dir), (rows[1], output_dir)):
        top = button.rectangle().top
        candidates = [
            edit
            for edit in window.descendants(control_type="Edit")
            if abs(edit.rectangle().top - top) < config.EDIT_ROW_TOLERANCE_PX
        ]
        if not candidates:
            msg = "未找到与「选择目录…」同排的目录输入框"
            raise RuntimeError(msg)
        _ = min(candidates, key=lambda e: e.rectangle().left).set_edit_text(value)


def s5_markdown_basic_chain(exe: str) -> None:
    """S5 转 Markdown 基本链路：启动→选输入→开始→停止→关闭.

    未配置/未就绪时「开始转换」保持禁用，wait 超时即失败——不得把「未配置也通过」
    报成基本链路通过。覆盖 T-23/P-12：观察当前媒体运行态，取消后已有成品逐字节
    不变、当前及后续产物不存在，临时文件和图片也不得留下。
    """
    scratch_box: list[Path] = []

    def prepare_scratch() -> None:
        scratch = Path(tempfile.mkdtemp(prefix="jchtools-gui-smoke-s5-"))
        scratch_box.append(scratch)
        (scratch / "input").mkdir()
        (scratch / "output").mkdir()
        # 空输入的转换瞬时完成，「停止任务」运行态不可观察，开始→停止链路断言失效；
        # 用媒体样本（转录需数秒）保证运行态可观察。样本由环境变量显式提供（真实
        # 组件门控用例同口径），缺样本时明确失败而不是把「没开始」当通过。
        media = os.environ.get("JCHTOOLS_S5_MEDIA", "")
        if not media or not Path(media).is_file():
            message = "S5 需要媒体样本以观察运行态：设置 JCHTOOLS_S5_MEDIA 指向一个真实媒体文件"
            raise RuntimeError(message)
        _ = (scratch / "input" / "00_complete.txt").write_text("completed before cancellation\n", encoding="utf-8")
        for index in range(1, config.STOP_MEDIA_FILE_COUNT + 1):
            _ = shutil.copyfile(media, scratch / "input" / f"{index:02}_{Path(media).name}")

    def body(window: WindowSpecification) -> None:
        scratch = scratch_box[0]
        goto_converter(window)
        set_converter_dirs(window, str(scratch / "input"), str(scratch / "output"))
        _ = convert_ui.wait_button(window, "开始转换", config.COMPLETION_TIMEOUT, enabled=True)
        # 合成点击偶发落空（点击后仍停在「尚未开始」）：按当前状态重试，与
        # confirm_dialog 同一立场——只检查「点击没报错」会把没生效的点击当成
        # 成功。每次重按后观察一段运行态窗口，重按最多 3 次。
        busy_timeout = config.CONVERT_BUSY_TIMEOUT // 3
        for attempt in range(3):
            driver.click(window, driver.find_button(window, "开始转换"))
            try:
                _ = convert_ui.wait_button(window, "停止任务", busy_timeout, enabled=True)
                break
            except (timings.TimeoutError, RuntimeError):
                if attempt == config.S5_LAST_ATTEMPT:
                    raise
        current = f"01_{Path(os.environ['JCHTOOLS_S5_MEDIA']).name}"
        current_md = f"{Path(current).stem}_{Path(current).suffix.lstrip('.')}.md"
        _ = convert_ui.wait_text_containing(window, f"正在处理 {current}")
        before = postconditions.snapshot_output_bytes(scratch / "output")
        driver.require("00_complete_txt.md" in before, "停止测试必须先观察一份已完成产物")
        driver.require(len(before) == 1, "当前媒体尚未完成时才可发出停止请求")
        driver.click(window, driver.find_button(window, "停止任务"))
        _ = convert_ui.wait_button(window, "开始转换", config.CONVERT_STOP_TIMEOUT, enabled=True)
        _ = convert_ui.wait_text_containing(window, "已停止")
        # 停止落地前的时延内当前文件可能已自然完成（其成品按 T-23 保留），取消条目
        # 因此可能落在下一文件上；断言「存在媒体文件的确认取消条目」这一合同不变量。
        _ = convert_ui.wait_log_matching(window, r"已取消：\d{2}_[^\r\n]+；未完成结果不提交")
        postconditions.verify_cancelled_outputs(scratch / "output", before, current_md)
        print("S5 PASS：当前转换及时取消；已完成产物保留，后续/临时产物均未提交")

    def cleanup() -> None:
        if scratch_box:
            shutil.rmtree(scratch_box.pop(), ignore_errors=True)

    driver.run_stage("S5", exe, body, pre=prepare_scratch, after=cleanup)


# ---------------------------------------------------------------- S6-S14：转 Markdown / 设置页阶段。


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
    before = convert_ui.produced_snapshot(produced_dir) if produced_dir is not None else None
    finished = ""
    for attempt in range(3):
        driver.click(window, driver.find_button(window, "开始转换"))
        deadline = time.time() + config.CONVERT_BUSY_TIMEOUT
        while time.time() < deadline:
            try:
                if driver.find_button(window, "停止任务").exists(timeout=0.1):
                    return convert_ui.wait_completion_with_anchor(window, previous_done, produced_dir, before)
                for text in window.descendants(control_type="Text"):
                    value = text.window_text() or ""
                    if convert_ui.confirm_new_completion(value, previous_done, produced_dir, before):
                        finished = value
                        break
            except driver.TRANSIENT_GUI_ERRORS:
                pass
            if finished:
                break
            time.sleep(0.5)
        if finished:
            return finished
        if attempt == config.S5_LAST_ATTEMPT:
            _ = driver.find_button(window, "停止任务").wait("visible enabled", timeout=config.TIMEOUT)
            msg = "转换既未进入运行态也未完成（开始按钮可能未生效）"
            raise RuntimeError(msg)
    msg = "转换既未进入运行态也未完成（开始按钮可能未生效）"
    raise RuntimeError(msg)


def s10_output_layout_and_flat_duplicate_policy(exe: str) -> None:
    """S10 输出层级（保留/平铺）与同名策略：层次两份、平铺一份+重复跳过计数."""
    scratch = Path(tempfile.mkdtemp(prefix="jchtools-gui-smoke-s10-"))
    source = scratch / "input"
    layered = scratch / "out-layered"
    flat = scratch / "out-flat"
    for part in ("a", "b"):
        (source / part).mkdir(parents=True)
        _ = (source / part / "同名.txt").write_text(f"stable first input {part}\n", encoding="utf-8")
    layered.mkdir()
    flat.mkdir()
    try:

        def body(window: WindowSpecification) -> None:
            goto_converter(window)
            # 层次模式：两份同名输入各自成文（T-11 默认保留层级）。
            set_converter_dirs(window, str(source), str(layered))
            metrics = start_conversion_and_wait_done(window, produced_dir=layered)
            driver.require((layered / "a" / "同名_txt.md").is_file(), "层次输出应保留相对层级 a/")
            driver.require((layered / "b" / "同名_txt.md").is_file(), "层次输出应保留相对层级 b/")
            driver.require("重复结果跳过 0" in metrics, f"层次模式不应有同名跳过：{metrics}")
            # 平铺模式：同名只处理排序第一份，其余按重复跳过计数。
            set_converter_dirs(window, str(source), str(flat))
            convert_ui.check_flat_output(window)
            metrics = start_conversion_and_wait_done(window, previous_done=metrics, produced_dir=flat)
            produced = sorted(p.name for p in flat.glob("*.md"))
            driver.require(produced == ["同名_txt.md"], f"平铺只应有一份结果：{produced}")
            result = (flat / "同名_txt.md").read_text(encoding="utf-8")
            driver.require(
                "stable first input a" in result and "stable first input b" not in result,
                "平铺须选排序第一份的实际内容",
            )
            driver.require("重复结果跳过 1" in metrics, f"平铺同名跳过必须如实计数：{metrics}")

        driver.run_stage("S10", exe, body)
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
    print("S10 PASS：层级保留两份同名输入；平铺只处理一份并如实计数重复跳过")


def s11_existing_results_are_skipped_untouched(exe: str) -> None:
    """S11 已有结果跳过：重复运行不覆盖既有产物，跳过数量如实显示（T-12）."""
    scratch = Path(tempfile.mkdtemp(prefix="jchtools-gui-smoke-s11-"))
    source = scratch / "input"
    output = scratch / "out"
    source.mkdir()
    output.mkdir()
    for name in config.S11_SOURCE_FILES:
        _ = (source / name).write_text(f"content of {name}\n", encoding="utf-8")
    try:

        def body(window: WindowSpecification) -> None:
            goto_converter(window)
            set_converter_dirs(window, str(source), str(output))
            first = start_conversion_and_wait_done(window, produced_dir=output)
            driver.require(
                "已有结果跳过 0" in first or "跳过" not in first,
                f"首轮应全部转换：{first}",
            )
            existing = {
                path.name: (postconditions.file_digest(path), path.stat().st_mtime_ns) for path in output.glob("*.md")
            }
            driver.require(
                len(existing) == len(config.S11_SOURCE_FILES),
                f"前置：两份产物（实得 {list(existing)}）",
            )
            second = start_conversion_and_wait_done(window, previous_done=first, produced_dir=output)
            driver.require(
                f"已有结果跳过 {len(config.S11_SOURCE_FILES)}" in second,
                f"重复运行必须如实显示跳过数量：{second}",
            )
            final = {
                path.name: (postconditions.file_digest(path), path.stat().st_mtime_ns) for path in output.glob("*.md")
            }
            driver.require(
                final == existing,
                "已有结果的文件集合、内容与修改时间必须保持不变（T-12）",
            )

        driver.run_stage("S11", exe, body)
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
    print("S11 PASS：已有结果跳过不覆盖，内容与修改时间不变，跳过数量如实显示")


def s12_output_subtree_is_excluded_from_scan(exe: str) -> None:
    """S12 输出子树排除：输出目录位于输入内时整棵输出子树不作为新输入（T-09）."""
    scratch = Path(tempfile.mkdtemp(prefix="jchtools-gui-smoke-s12-"))
    source = scratch / "input"
    output = source / "results"
    (source / "prep").mkdir(parents=True)
    _ = (source / "prep" / "page.png").write_bytes(postconditions.tiny_png())
    output.mkdir()
    _ = (output / "inner.png").write_bytes(postconditions.tiny_png())
    try:

        def body(window: WindowSpecification) -> None:
            goto_converter(window)
            set_converter_dirs(window, str(source), str(output))
            metrics = start_conversion_and_wait_done(window, produced_dir=output)
            driver.require(
                (output / "prep" / "page_png.md").is_file(),
                f"输出子树外的输入应正常转换：{metrics}",
            )
            driver.require(
                not (output / "inner_png.md").exists(),
                "输出子树内的文件不得作为新输入",
            )
            driver.require(
                not (output / "results" / "inner_png.md").exists(),
                "不得重复嵌套输出子树",
            )
            driver.require((output / "inner.png").is_file(), "输出子树内的源文件保持原样")

        driver.run_stage("S12", exe, body)
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
            driver.require("成功 1" in metrics, f"可转换文件必须成功（统计行：{metrics}）")
            driver.require("失败 1" in metrics, f"损坏文件必须计入失败（统计行：{metrics}）")
            driver.require((output / "good_txt.md").is_file(), "成功产物必须在场")
            driver.require(
                not (output / "broken_png.md").exists(),
                "失败文件不得留下半成品（T-25）",
            )

        driver.run_stage("S13", exe, body)
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
    print("S13 PASS：部分失败不终止批次，成功/失败计数与产物边界一致")


def s14_close_during_conversion_confirms_and_stops(exe: str) -> None:
    """S14 运行中关闭确认与安全停止：U-09 确认框 → T-23 安全停止 → 退出码 0."""
    scratch_box: list[Path] = []

    def prepare_scratch() -> None:
        scratch = Path(tempfile.mkdtemp(prefix="jchtools-gui-smoke-s14-"))
        scratch_box.append(scratch)
        (scratch / "input").mkdir()
        (scratch / "output").mkdir()
        media = os.environ.get("JCHTOOLS_S5_MEDIA", "")
        if not media or not Path(media).is_file():
            message = "S14 需要媒体样本以保持转换运行态：设置 JCHTOOLS_S5_MEDIA 指向一个真实媒体文件"
            raise RuntimeError(message)
        _ = (scratch / "input" / "00_complete.txt").write_text("completed before closing\n", encoding="utf-8")
        for index in range(1, config.STOP_MEDIA_FILE_COUNT + 1):
            _ = shutil.copyfile(media, scratch / "input" / f"{index:02}_{Path(media).name}")

    def body(window: WindowSpecification) -> None:
        scratch = scratch_box[0]
        goto_converter(window)
        set_converter_dirs(window, str(scratch / "input"), str(scratch / "output"))
        for attempt in range(3):
            driver.click(window, driver.find_button(window, "开始转换"))
            try:
                _ = driver.find_button(window, "停止任务").wait(
                    "visible enabled", timeout=config.CONVERT_BUSY_TIMEOUT // 3
                )
                break
            except timings.TimeoutError:
                if attempt == config.S5_LAST_ATTEMPT:
                    raise
        current = f"01_{Path(os.environ['JCHTOOLS_S5_MEDIA']).name}"
        current_md = f"{Path(current).stem}_{Path(current).suffix.lstrip('.')}.md"
        _ = convert_ui.wait_text_containing(window, f"正在处理 {current}")
        before = postconditions.snapshot_output_bytes(scratch / "output")
        driver.require("00_complete_txt.md" in before and len(before) == 1, "关闭前须确认首份已完成且当前媒体尚未完成")
        convert_ui.close_title_bar(window)
        driver.click(window, driver.find_button(window, config.STOP_AND_CLOSE_BUTTON))
        convert_ui.wait_window_closed_after_cancel(window)
        postconditions.verify_cancelled_outputs(scratch / "output", before, current_md)

    def cleanup() -> None:
        if scratch_box:
            shutil.rmtree(scratch_box.pop(), ignore_errors=True)

    driver.run_stage("S14", exe, body, pre=prepare_scratch, after=cleanup)
    print("S14 PASS：运行中关闭确认后当前转换及时取消，成品保留，无后续/临时产物，正常退出")
