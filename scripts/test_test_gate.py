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
from scripts.bazel_gate import BazelSession, bazel_label


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
