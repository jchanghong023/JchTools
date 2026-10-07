"""test_gate 预算会计回归单测（fastcheck 60 秒硬上限的确定性反证）.

覆盖两个已确认的预算缺陷（独立复核反例：最后阶段启动 6s + 等待 55s →
总墙钟 61s 超过 60s 预算，仍报 Overall PASS 退出码 0）：

1. ``run_logged`` 的等待预算必须按绝对截止（started+timeout）计算，进程启动与
   Job 绑定消耗的时间要从预算中扣除；
2. ``cmd_fastcheck`` 必须有终局墙钟守卫：各阶段即使都未单独超时，总墙钟越界
   也 ``MUST`` 判 FAIL，不得报退出码 0。

确定性：用注入假时钟替代 ``time.monotonic``，不真实睡眠 61 秒；唯一真实进程
用 0.4s 睡眠命令，预算差额远大于进程启动抖动。只写 .tmp/test-gate 日志目录，
不触碰仓库其它位置。
"""

from __future__ import annotations

import sys
import unittest
from unittest.mock import patch

from scripts import test_gate


class _FakeMonotonic:
    """假墙钟：按预约值序列读数，之后恒定；测试内可再手动推进。."""

    def __init__(self, values: list[float]) -> None:
        self._scheduled: list[float] = list(values)
        self._current: float = values[-1] if values else 0.0

    def __call__(self) -> float:
        if self._scheduled:
            self._current = self._scheduled.pop(0)
        return self._current

    def advance(self, seconds: float) -> None:
        self._current += seconds


class AcceptanceCoverageGapTests(unittest.TestCase):
    """acceptance 覆盖缺口判定：仅剩环境受限条目（C08/C09）NOT RUN 时不得阻塞测试门。."""

    def test_only_environment_blocked_items_are_not_a_gap(self) -> None:
        summary = "NOT RUN  markdown-acceptance（存在未执行条目：C08、C09；缺资产或 --only 子集不能冒充完整验收）"
        text = f"PASS  snap-ocr-worker-root\n{summary}"
        assert test_gate.acceptance_coverage_gaps(text) == []  # nosec B101: 环境受限条目不构成缺口。

    def test_other_not_run_items_remain_gaps(self) -> None:
        summary = "NOT RUN  markdown-acceptance（存在未执行条目：A25、C08；详见 markdown-acceptance.log）"
        text = f"PASS  snap-ocr-worker-root\n{summary}"
        gaps = test_gate.acceptance_coverage_gaps(text)
        assert gaps  # nosec B101: A25 缺样本不属环境受限，不得放行。

    def test_not_run_without_item_ids_remains_gap(self) -> None:
        summary = "NOT RUN  markdown-acceptance（存在未执行条目；详见 markdown-acceptance.log）"
        text = f"PASS  snap-ocr-worker-root\n{summary}"
        gaps = test_gate.acceptance_coverage_gaps(text)
        assert gaps  # nosec B101: 提取不到条目号时 fail-closed，不得静默放行。


class RunLoggedSetupBudgetTests(unittest.TestCase):
    """run_logged 必须把进程启动/Job 绑定耗时计入阶段预算（绝对截止）。."""

    def test_setup_time_is_charged_against_stage_timeout(self) -> None:
        # 假时钟：started=100.0；等待预算计算点已到 101.85（启动消耗 1.85s），
        # 清理读数点 101.95。阶段预算 2.0s → 修复后等待预算只剩 0.15s。
        clock = _FakeMonotonic([100.0, 101.85, 101.95])
        # 真实命令睡眠 0.4s（加解释器启动 ≈0.5s）：远大于修复后的 0.15s 等待预算
        # （必须 TIMEOUT），又远小于修复前固定的 1.5s 等待预算（修复前会 PASS）。
        argv = [sys.executable, "-c", "import time; time.sleep(0.4)"]
        with patch.object(test_gate, "_monotonic", clock):
            result = test_gate.run_logged("budget-setup-regression", argv, timeout=2.0)
        assert result.status == test_gate.STATUS_TIMED_OUT  # nosec B101: 回归断言，不用于产品输入校验。
        assert "总预算" in result.detail  # nosec B101: 回归断言，不用于产品输入校验。


class FastcheckWallClockGuardTests(unittest.TestCase):
    """cmd_fastcheck 终局守卫：总墙钟越界时整体必须失败（不得报退出码 0）。."""

    def _run_fastcheck(self, clock: _FakeMonotonic, last_stage_end: float) -> int:
        def fake_runner(
            name: str,
            argv: list[str],
            *,
            timeout: float,
            env_extra: dict[str, str] | None = None,
        ) -> test_gate.StageResult:
            _ = argv, timeout, env_extra
            if name == "cargo-test":
                # 复现独立复核反例：最后阶段启动 6s + 等待 55s → 累计 61s 越过 60s 预算。
                clock.advance(last_stage_end - clock())
            else:
                clock.advance(0.1)
            return test_gate.StageResult(name, test_gate.STATUS_OK, "模拟阶段（未真实执行命令）")

        with (
            patch.object(test_gate, "_monotonic", clock),
            patch("shutil.which", return_value="cargo"),
            patch.object(test_gate, "run_logged", fake_runner),
        ):
            return test_gate.cmd_fastcheck(60.0)

    def test_total_wall_clock_over_budget_fails_even_when_all_stages_ok(self) -> None:
        clock = _FakeMonotonic([0.0])
        exit_code = self._run_fastcheck(clock, last_stage_end=61.0)
        assert exit_code != 0  # nosec B101: 总墙钟 61s > 60s 硬上限时不得报成功。

    def test_total_wall_clock_within_budget_still_passes(self) -> None:
        # 正向对照：终局守卫不得误伤预算内的正常 fastcheck。
        clock = _FakeMonotonic([0.0])
        exit_code = self._run_fastcheck(clock, last_stage_end=59.5)
        assert exit_code == 0  # nosec B101: 各阶段与总墙钟都在预算内时必须仍是 0。


if __name__ == "__main__":
    _ = unittest.main()
