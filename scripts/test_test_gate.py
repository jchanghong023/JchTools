"""Aggregate local gate behavior and authorization boundary tests."""

from __future__ import annotations

import unittest

from scripts.test_test_gate_direct import DirectGateTests
from scripts.test_test_gate_nextest import NextestNativeTests
from scripts.test_test_gate_planning_units import (
    AcceptanceCoverageGapTests,
    GitSnapshotEncodingTests,
    OpenCodeAuthorizationBoundaryTests,
    TemporaryCleanupPathTests,
)
from scripts.test_test_gate_runner_units import (
    NativeUnittestReportTests,
    XbergCachedIdentityTests,
)

_TEST_GATE_CLASSES = (
    DirectGateTests,
    NextestNativeTests,
    GitSnapshotEncodingTests,
    AcceptanceCoverageGapTests,
    TemporaryCleanupPathTests,
    OpenCodeAuthorizationBoundaryTests,
    XbergCachedIdentityTests,
    NativeUnittestReportTests,
)
for _test_class in _TEST_GATE_CLASSES:
    _test_class.__module__ = "scripts.test_test_gate"


if __name__ == "__main__":
    unittest.main()
