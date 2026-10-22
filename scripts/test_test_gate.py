"""test_gate 预算会计与真实模型授权边界回归单测.

覆盖两个已确认的预算缺陷；当前三级门共用非编译预算，真正编译独占区间扣除：

1. ``run_logged`` 的有限等待预算必须按绝对截止（started+timeout）计算，进程
   启动与 Job 绑定消耗的时间要从预算中扣除；
2. ``execute_gate`` 的非编译工作与最终汇总共用时钟；即使阶段报告成功，
   非编译墙钟越界也必须失败，不得报退出码 0。

确定性：用注入假时钟替代 ``time.monotonic``，不真实睡眠 61 秒；真实预算进程
无限等待直至被终止，并直接核对传入真实 wait 的剩余预算，避免宿主调度改变结论。
日志及真实清理夹具仅写 .tmp/test-gate、.tmp/parallel-review 与 .tmp/acp-review。

完整门授权边界仅调用真实计划器；合成 Cargo artifact 日志交由生产解析器读取，
计划替身只记录原语，预算与 Job 自检仅监督单条隔离命令，不执行完整门、远程或模型调用。
"""

from __future__ import annotations

import contextlib
import dataclasses
import hashlib
import io
import json
import mmap
import os
import shutil
import stat
import struct
import subprocess
import sys
import tempfile
import textwrap
import threading
import time
import unittest
import zipfile
from collections.abc import Callable
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import cast
from unittest.mock import MagicMock, patch

from scripts import (
    bazel_action,
    bazel_gate,
    bazel_service,
    gate_runtime,
    local_gate_plan,
    make_tmp,
    test_gate,
    test_timing,
    xberg_test_engine,
)
from scripts.bazel_gate import BazelSession, _label


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



class _PlanningGateRun:
    """只接收真实计划器的阶段，不执行 argv，也不伪造模型响应。."""

    def __init__(self, log_dir: Path, logs: dict[str, Path]) -> None:
        self.log_dir: Path = log_dir
        self.logs: dict[str, Path] = logs
        self.stages: list[gate_runtime.Stage] = []
        self.batches: list[list[gate_runtime.Stage]] = []

    def run(self, stages: list[gate_runtime.Stage], *, parallel: bool = True) -> list[gate_runtime.Result]:
        self.batches.append(list(stages))
        self.stages.extend(stages)
        for stage in stages:
            if stage.name.endswith("-list"):
                log = self.log_dir / f"{stage.name}.log"
                if stage.argv[1:3] == ["-m", "scripts.gate_unittest"]:
                    listing = 'JCHTOOLS_TEST_CASES=["planning.case"]\n'
                else:
                    listing = "ignored_case: test\n"
                    if not stage.name.endswith("-ignored-list"):
                        listing = "ordinary_case: test\n" + listing
                _ = log.write_text(listing, encoding="utf-8")
                self.logs[stage.name] = log
        return [
            gate_runtime.Result(
                stage.name,
                gate_runtime.STATUS_OK,
                "planning-only synthetic prerequisite; command not executed",
                log=self.logs.get(stage.name),
            )
            for stage in stages
        ]



# 按主题拆分的用例模块在此回接；类定义已钉回本模块路径（见下方 __module__ 修正），
# `python -m unittest scripts.test_test_gate.GateFailFastTests` 与
# local_gate_plan 的用例 ID 前缀 scripts.test_test_gate.<Class>.<method> 保持不变。
from scripts.test_test_gate_bazel_units import (
    GateNativeCompilerObservationTests,
    GateNativeExternalCacheTests,
    GateNativeTimeoutTests,
    GateToolContentIdentityTests,
)
from scripts.test_test_gate_cargo_artifacts import (
    CargoArtifactBatchRestoreTests,
    CargoArtifactEncodingTests,
    CargoArtifactSnapshotTests,
    GateCachedPayloadTests,
)
from scripts.test_test_gate_failfast import (
    GateBlockedFailureOutputTests,
    GateFailFastTests,
)
from scripts.test_test_gate_planning_units import (
    AcceptanceCoverageGapTests,
    GitSnapshotEncodingTests,
    OpenCodeAuthorizationBoundaryTests,
    TemporaryCleanupPathTests,
)
from scripts.test_test_gate_runner_units import (
    GateUnittestCasesTests,
    TimingBudgetAndEncodingTests,
    TimingResultIntegrityTests,
    UnittestFailureSignalTests,
    XbergCachedIdentityTests,
)
from scripts.test_test_gate_service_lifecycle import (
    GateImmutablePublicationTests,
    GateServerPidPublicationTests,
    PersistentBazelLifecycleTests,
    PublishedTextConcurrencyTests,
)
from scripts.test_test_gate_supervision import (
    FastcheckWallClockGuardTests,
    GateCleanupCancellationTests,
    GateLogIsolationTests,
    GateProgressPublicationTests,
    OwnedCommandLaunchCancellationTests,
    RunLoggedSetupBudgetTests,
)

_TEST_GATE_CLASSES = (
    GateToolContentIdentityTests,
    GateNativeTimeoutTests,
    GateNativeExternalCacheTests,
    GateNativeCompilerObservationTests,
    CargoArtifactEncodingTests,
    GateCachedPayloadTests,
    CargoArtifactSnapshotTests,
    CargoArtifactBatchRestoreTests,
    GateFailFastTests,
    GateBlockedFailureOutputTests,
    OwnedCommandLaunchCancellationTests,
    RunLoggedSetupBudgetTests,
    GateLogIsolationTests,
    GateProgressPublicationTests,
    FastcheckWallClockGuardTests,
    GateCleanupCancellationTests,
    PublishedTextConcurrencyTests,
    PersistentBazelLifecycleTests,
    GateServerPidPublicationTests,
    GateImmutablePublicationTests,
    GitSnapshotEncodingTests,
    AcceptanceCoverageGapTests,
    TemporaryCleanupPathTests,
    OpenCodeAuthorizationBoundaryTests,
    TimingBudgetAndEncodingTests,
    TimingResultIntegrityTests,
    XbergCachedIdentityTests,
    UnittestFailureSignalTests,
    GateUnittestCasesTests,
)
for _test_class in _TEST_GATE_CLASSES:
    _test_class.__module__ = "scripts.test_test_gate"


if __name__ == "__main__":
    _ = unittest.main()
