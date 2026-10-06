"""gui_smoke 纯函数回归单测（S11-06 完成判据与产物快照锚、S11-07 隔离守卫）.

只测纯函数与文件系统快照 helper，不启动 GUI、不写仓库目录（tempfile 例外）；
受 Bandit B101 管控的 assert 逐条附 nosec 说明。
"""

from __future__ import annotations

import os
import tempfile
import unittest
from pathlib import Path

from scripts import gui_smoke

# 与 src/gui.rs 收尾统计行同构的样本：两轮计数与四舍五入后的总耗时可以逐字相同。
DONE_LINE = "成功 2 · 部分提取 0 · 失败 0 · 已有结果跳过 0 · 重复结果跳过 0 · 总耗时 0.4s"
DIFFERENT_LINE = "成功 0 · 部分提取 0 · 失败 0 · 已有结果跳过 2 · 重复结果跳过 0 · 总耗时 0.4s"
ISOLATED_ENV = {
    "JCHTOOLS_TEST_STATE_DIR": r"C:\isolated\state",
    "JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT": r"C:\isolated\snap-assets",
}


def snapshot_names(snapshot: frozenset[tuple[str, int, int]]) -> set[str]:
    return {entry[0] for entry in snapshot}


class ProducedSnapshotTests(unittest.TestCase):
    def test_snapshot_tracks_new_updated_and_unchanged_state(self) -> None:
        with tempfile.TemporaryDirectory(prefix="jchtools-smoke-unit-") as temporary:
            root = Path(temporary)
            empty = gui_smoke.produced_snapshot(root)
            assert empty == frozenset()  # nosec B101: 回归测试断言，不用于产品权限或输入校验。
            (root / "sub").mkdir()
            _ = (root / "sub" / "one_txt.md").write_bytes(b"one\n")
            produced = gui_smoke.produced_snapshot(root)
            assert produced != empty  # nosec B101: 新产物必须改变快照（S11-06 代次锚）。
            assert snapshot_names(produced) == {f"sub{os.sep}one_txt.md"}  # nosec B101: 快照记录相对路径。
            assert gui_smoke.produced_snapshot(root) == produced  # nosec B101: 未再写入时快照稳定。
            _ = (root / "sub" / "one_txt.md").write_bytes(b"one rewritten with different length\n")
            updated = gui_smoke.produced_snapshot(root)
            assert updated != produced  # nosec B101: 内容更新（大小变化）必须改变快照。

    def test_snapshot_of_missing_directory_is_empty(self) -> None:
        with tempfile.TemporaryDirectory(prefix="jchtools-smoke-missing-") as temporary:
            missing = Path(temporary) / "missing"
            assert not missing.exists()  # nosec B101: 本次隔离的子目录必须确实缺失。
            assert gui_smoke.produced_snapshot(missing) == frozenset()  # nosec B101: 目录不可读返回空快照。


class CompletionConfirmsNewRunTests(unittest.TestCase):
    def test_identical_line_with_changed_snapshot_confirms_new_run(self) -> None:
        # S11-06 回归：统计行与上一轮逐字相同时，旧判据（只比文本）识别不出新完成。
        before = frozenset({("one_txt.md", 1, 4)})
        after = frozenset({("one_txt.md", 2, 8)})
        assert gui_smoke.completion_confirms_new_run(DONE_LINE, DONE_LINE, before, after)  # nosec B101: 回归测试断言。

    def test_identical_line_with_unchanged_snapshot_is_not_new(self) -> None:
        same = frozenset({("one_txt.md", 1, 4)})
        assert not gui_smoke.completion_confirms_new_run(DONE_LINE, DONE_LINE, same, same)  # nosec B101: 回归测试断言。

    def test_identical_line_without_snapshot_cannot_confirm(self) -> None:
        assert not gui_smoke.completion_confirms_new_run(DONE_LINE, DONE_LINE, None, None)  # nosec B101: 回归测试断言。
        assert not gui_smoke.completion_confirms_new_run(DONE_LINE, DONE_LINE, None, frozenset())  # nosec B101: 回归测试断言。

    def test_different_line_confirms_without_snapshot(self) -> None:
        assert gui_smoke.completion_confirms_new_run(DIFFERENT_LINE, DONE_LINE, None, None)  # nosec B101: 回归测试断言。

    def test_line_without_done_marker_never_confirms(self) -> None:
        running = "正在处理 one.txt"
        assert not gui_smoke.completion_confirms_new_run(running, DONE_LINE, None, frozenset({("a", 1, 1)}))  # nosec B101: 回归测试断言。


class IsolationGuardTests(unittest.TestCase):
    def test_missing_isolation_env_lists_absent_keys_in_fixed_order(self) -> None:
        assert gui_smoke.missing_isolation_env({}) == list(gui_smoke.ISOLATION_REQUIRED_ENV_KEYS)  # nosec B101: 回归测试断言。
        partial = {"JCHTOOLS_TEST_STATE_DIR": r"C:\isolated\state"}
        assert gui_smoke.missing_isolation_env(partial) == ["JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT"]  # nosec B101: 回归测试断言。
        assert gui_smoke.missing_isolation_env(ISOLATED_ENV) == []  # nosec B101: 回归测试断言。

    def test_guarded_stage_without_isolation_is_refused_with_entry_hint(self) -> None:
        for stages in (["S5"], ["S10", "S11", "S12", "S13"], ["S14"], ["S17"], ["S1", "S11"]):
            message = ""
            try:
                gui_smoke.require_orchestrated_isolation(stages, {}, allow_isolated_run=False)
            except RuntimeError as error:
                message = str(error)
            assert message, f"{stages} 缺隔离环境时必须拒绝启动"  # nosec B101: 回归测试断言，不用于产品权限或输入校验。
            assert "JCHTOOLS_TEST_STATE_DIR" in message  # nosec B101: 拒绝信息必须指认缺失变量。
            assert "acceptance.ps1" in message  # nosec B101: 拒绝信息必须指引编排入口。
            assert "--allow-isolated-run" in message  # nosec B101: 拒绝信息必须给出显式逃生口。

    def test_guard_passes_with_isolated_env_or_unguarded_stages_or_explicit_flag(self) -> None:
        gui_smoke.require_orchestrated_isolation(["S5", "S17"], ISOLATED_ENV, allow_isolated_run=False)
        gui_smoke.require_orchestrated_isolation(["S1", "S4", "S15"], {}, allow_isolated_run=False)
        gui_smoke.require_orchestrated_isolation(["S5", "S14"], {}, allow_isolated_run=True)
