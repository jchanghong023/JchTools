"""Real nextest regressions against a tiny, isolated Cargo workspace."""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import time
import unittest
import uuid
from pathlib import Path
from unittest.mock import patch

from scripts import local_gate_plan, make_tmp, test_gate


_LIB = r'''
use std::{env, fs, process::Command, thread, time::{Duration, Instant}};

fn marker(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env::var_os("NATIVE_FIXTURE_DIR").unwrap()).join(name)
}

#[test]
fn ordinary() { fs::write(marker("ordinary"), "ok").unwrap(); }

#[test]
#[ignore]
fn allowed_ignored() { fs::write(marker("allowed"), "ok").unwrap(); }

#[test]
#[ignore]
fn forbidden_ignored() { fs::write(marker("forbidden"), "wrong").unwrap(); }

#[test]
#[ignore]
fn descendant() {
    fs::write(marker("child-ready"), "ok").unwrap();
    thread::sleep(Duration::from_secs(5));
    fs::write(marker("child-survived"), "wrong").unwrap();
}

#[test]
fn running_parent() {
    let _child = Command::new(env::current_exe().unwrap())
        .args(["--exact", "descendant", "--ignored", "--nocapture"])
        .spawn().unwrap();
    thread::sleep(Duration::from_secs(30));
    fs::write(marker("parent-survived"), "wrong").unwrap();
}

#[test]
fn first_failure() {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !marker("child-ready").exists() {
        assert!(Instant::now() < deadline, "child was not started");
        thread::sleep(Duration::from_millis(20));
    }
    panic!("intentional first failure");
}
'''

_GROUP_TEST = r'''
use std::{env, fs, thread, time::Duration};

fn run(name: &str) {
    let root = std::path::PathBuf::from(env::var_os("NATIVE_FIXTURE_DIR").unwrap());
    let lock = root.join("engine-lock");
    fs::create_dir(&lock).expect("engine-session tests overlapped");
    thread::sleep(Duration::from_millis(500));
    fs::write(root.join(name), "ok").unwrap();
    fs::remove_dir(lock).unwrap();
}

#[test]
fn engine_one() { run("engine-one"); }
#[test]
fn engine_two() { run("engine-two"); }
'''

_GUI_FLOW_TEST = r'''
#[test]
fn desktop_canary() {
    std::fs::write(std::path::PathBuf::from(std::env::var_os("NATIVE_FIXTURE_DIR").unwrap()).join("gui-flow"), "ok").unwrap();
}
'''

_ACP_TEST = r'''
fn mark(name: &str) {
    std::fs::write(std::path::PathBuf::from(std::env::var_os("NATIVE_FIXTURE_DIR").unwrap()).join(name), "ok").unwrap();
}

#[test]
fn ordinary_acp() { mark("ordinary-acp"); }


mod gui {
    #[test]
    fn real_gui_save_close_kill_reopen_and_confirmed_service_exit() { super::mark("gui-save"); }
    #[test]
    fn real_gui_pending_configuration_applies_only_after_inflight_completion() { super::mark("gui-pending"); }
    #[test]
    fn ordinary_gui_module_test() { super::mark("ordinary-gui-module"); }
}
'''


class NextestNativeTests(unittest.TestCase):
    def setUp(self) -> None:
        self.root = test_gate.ROOT / ".tmp" / "test-gate" / f"nextest-{uuid.uuid4().hex}"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(self.root)]):
            self.assertEqual(make_tmp.main(), 0)
        self.addCleanup(shutil.rmtree, self.root)
        (self.root / "src").mkdir()
        (self.root / "tests").mkdir()
        (self.root / "worker" / "src").mkdir(parents=True)
        (self.root / "Cargo.toml").write_text(
            '[package]\nname = "native-nextest-fixture"\nversion = "0.1.0"\nedition = "2021"\n'
            '[workspace]\nmembers = ["worker"]\ndefault-members = [".", "worker"]\n', encoding="utf-8"
        )
        (self.root / "src" / "lib.rs").write_text(_LIB, encoding="utf-8")
        (self.root / "worker" / "Cargo.toml").write_text(
            '[package]\nname = "snap-ocr-worker"\nversion = "0.1.0"\nedition = "2021"\n'
            '[lib]\nname = "snap_ocr_worker"\n', encoding="utf-8"
        )
        worker_tests = _GROUP_TEST.replace('"engine-one"', '"worker-one"').replace(
            '"engine-two"', '"worker-two"'
        )
        (self.root / "worker" / "src" / "lib.rs").write_text(worker_tests, encoding="utf-8")
        (self.root / "tests" / "background_settings.rs").write_text(_GROUP_TEST, encoding="utf-8")
        (self.root / "tests" / "gui_flow.rs").write_text(_GUI_FLOW_TEST, encoding="utf-8")
        (self.root / "tests" / "acp_service_process.rs").write_text(_ACP_TEST, encoding="utf-8")

    def _nextest(self, *args: str) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        env["NATIVE_FIXTURE_DIR"] = str(self.root)
        return subprocess.run(
            ["cargo", "nextest", "run", "--config-file", str(test_gate.ROOT / ".config" / "nextest.toml"),
             "--test-threads", str(local_gate_plan.test_worker_count()),
             "--retries", "0", "--max-fail=1:immediate", "--color", "never", "--no-tests", "fail",
             "--status-level", "fail", "--failure-output", "immediate", *args],
            cwd=self.root, env=env, capture_output=True, text=True, timeout=90, check=False,
        )

    def test_ignored_whitelist_and_engine_group(self) -> None:
        normal = self._nextest(
            "-E", "test(=ordinary) or test(=allowed_ignored) or test(=forbidden_ignored) "
            "or binary(=background_settings) or binary_id(=snap-ocr-worker)"
        )
        self.assertEqual(normal.returncode, 0, normal.stdout + normal.stderr)
        for name in ("ordinary", "engine-one", "engine-two", "worker-one", "worker-two"):
            self.assertTrue((self.root / name).exists(), name)
        self.assertFalse((self.root / "allowed").exists())
        self.assertFalse((self.root / "forbidden").exists())
        allowed = self._nextest("--run-ignored", "only", "-E", "test(=allowed_ignored)")
        self.assertEqual(allowed.returncode, 0, allowed.stdout + allowed.stderr)
        self.assertTrue((self.root / "allowed").exists())
        self.assertFalse((self.root / "forbidden").exists())

    def test_narrow_package_and_library_selections_parse_workspace_config(self) -> None:
        for selection in (
            ("-p", "snap-ocr-worker", "--lib", "-E", "test(=engine_one)"),
            ("-p", "native-nextest-fixture", "--lib", "-E", "test(=ordinary)"),
        ):
            result = self._nextest(*selection)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


    def test_desktop_tests_require_explicit_authorization(self) -> None:
        selection = "binary(=gui_flow) or binary(=acp_service_process)"
        normal = self._nextest("-E", selection)
        self.assertEqual(normal.returncode, 0, normal.stdout + normal.stderr)
        for name in ("ordinary-acp", "ordinary-gui-module"):
            self.assertTrue((self.root / name).exists(), name)
        for name in ("gui-flow", "gui-save", "gui-pending"):
            self.assertFalse((self.root / name).exists(), name)

        authorized = self._nextest("--ignore-default-filter", "-E", selection)
        self.assertEqual(authorized.returncode, 0, authorized.stdout + authorized.stderr)
        for name in ("gui-flow", "gui-save", "gui-pending"):
            self.assertTrue((self.root / name).exists(), name)

    def test_first_failure_terminates_parallel_test_and_descendant(self) -> None:
        if local_gate_plan.test_worker_count() < 2:
            self.skipTest("parallel fail-fast requires at least two available CPUs")
        result = self._nextest("-E", "test(=first_failure) or test(=running_parent)")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("first_failure", result.stdout + result.stderr)
        self.assertTrue((self.root / "child-ready").exists())
        time.sleep(5.5)
        self.assertFalse((self.root / "child-survived").exists())
        self.assertFalse((self.root / "parent-survived").exists())
