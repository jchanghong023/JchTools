"""test_test_gate 按主题拆分的用例模块：Cargo 产物编码 / 快照捕获与批量恢复单元.

类经 scripts/test_test_gate.py 根模块回接并钉回 `scripts.test_test_gate`
模块路径；用例 ID 与拆分前的 `scripts.test_test_gate.<Class>.<method>`
完全一致，断言与夹具逐字保留。
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


class CargoArtifactEncodingTests(unittest.TestCase):
    """实际读取 PowerShell 5.1 UTF-8 BOM 日志，不能丢掉首个唯一 Cargo artifact。"""

    def test_single_artifact_with_or_without_bom_is_discovered(self) -> None:
        workspace = test_gate.ROOT / ".tmp" / "test-gate" / "cargo-bom-regression"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        with tempfile.TemporaryDirectory(dir=workspace) as temporary:
            root = Path(temporary)
            artifact = {
                "reason": "compiler-artifact",
                "profile": {"test": True},
                "target": {"name": "中文测试", "kind": ["test"]},
                "executable": str(root / "测试.exe"),
            }
            for encoding in ("utf-8-sig", "utf-8"):
                with self.subTest(encoding=encoding):
                    log = root / f"{encoding}.log"
                    log.write_text(json.dumps(artifact, ensure_ascii=False) + "\n", encoding=encoding)
                    if encoding == "utf-8-sig":
                        self.assertTrue(log.read_bytes().startswith(b"\xef\xbb\xbf"))
                    discovered = local_gate_plan._test_binary_stages(
                        gate_runtime.Result("root-test-build", "PASS", "合成 Cargo 日志", log=log)
                    )
                    self.assertEqual(len(discovered), 1, "唯一 artifact 因 UTF-8 BOM 被丢失")
                    self.assertEqual(discovered[0].argv[0], artifact["executable"])
                    self.assertTrue(discovered[0].watch_test_failures)

    def test_same_name_lib_bin_and_integration_artifacts_all_reach_reports(self) -> None:
        workspace = test_gate.ROOT / ".tmp" / "test-gate" / "crate-artifacts"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        with tempfile.TemporaryDirectory(dir=workspace) as temporary:
            root = Path(temporary)
            executables = {kind: str(root / f"{kind}.exe") for kind in ("lib", "bin", "test")}
            log = root / "cargo.json"
            log.write_text(
                "\n".join(
                    json.dumps(
                        {
                            "reason": "compiler-artifact",
                            "profile": {"test": True},
                            "target": {"name": "jchocr", "kind": [kind]},
                            "executable": executable,
                        }
                    )
                    for kind, executable in executables.items()
                ),
                encoding="utf-8",
            )
            stages = local_gate_plan._test_binary_stages(
                gate_runtime.Result("root-test-build", "PASS", "合成 Cargo 日志", log=log)
            )
            self.assertCountEqual([stage.argv[0] for stage in stages], executables.values())
            self.assertEqual(len({stage.name for stage in stages}), len(executables))
            # 真实调度与汇总消费者执行隔离 Python 子进程；不把合成日志冒称 Cargo 验证。
            for stage in stages:
                stage.argv = [sys.executable, "-c", "import sys; print(sys.argv[1])", stage.argv[0]]
            output = io.StringIO()
            with patch.object(test_gate, "LOG_DIR", root / "logs"), contextlib.redirect_stdout(output):
                code = test_gate.execute_gate("fastcheck", 60.0, stages_override=stages)
            self.assertEqual(code, 0)
            reports = output.getvalue().splitlines()
            report_positions = []
            logs = []
            for stage in stages:
                matching = [i for i, line in enumerate(reports) if line.startswith(f"PASS {stage.name}:")]
                self.assertEqual(len(matching), 1, "每个实际执行的 artifact 必须独立报告")
                report_positions.append(matching[0])
                # 门按级别创建日志子目录；消费者应读取报告公开的路径，不猜内部布局或文件名。
                self.assertLess(matching[0] + 1, len(reports))
                log_line = reports[matching[0] + 1]
                self.assertTrue(log_line.startswith("  log="), "每个 artifact 必须报告实际日志路径")
                artifact_log = Path(log_line.removeprefix("  log="))
                logs.append(artifact_log)
                self.assertEqual(
                    artifact_log.read_text(encoding="utf-8").splitlines(),
                    [stage.argv[-1]],
                    "每个 artifact 的独立日志必须包含自身子进程输出，不能混入其它 artifact",
                )
            self.assertEqual(len(set(report_positions)), len(executables))
            self.assertEqual(len(logs), len(executables))
            self.assertEqual(len(set(logs)), len(executables), "不同 artifact 不能共享日志路径")
            recorded = "\n".join(path.read_text(encoding="utf-8") for path in logs)
            for executable in executables.values():
                self.assertIn(executable, recorded)

    def test_artifact_desktop_exclusions_preserve_other_test_cases(self) -> None:
        workspace = test_gate.ROOT / ".tmp" / "test-gate" / "artifact-desktop-boundary"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        with tempfile.TemporaryDirectory(dir=workspace) as temporary:
            root = Path(temporary)
            names = ("gui_flow", "acp_service_process", "jchtools_gui", "jchtools_file_core")
            log = root / "cargo.json"
            log.write_text(
                "\n".join(
                    json.dumps(
                        {
                            "reason": "compiler-artifact",
                            "profile": {"test": True},
                            "target": {"name": name, "kind": ["test"] if index < 2 else ["lib"]},
                            "executable": str(root / f"{name}.exe"),
                        }
                    )
                    for index, name in enumerate(names)
                ),
                encoding="utf-8",
            )
            stages = local_gate_plan._test_binary_stages(
                gate_runtime.Result("root-test-build", "PASS", "合成 Cargo 日志", log=log)
            )
            self.assertCountEqual(
                [stage.argv[0] for stage in stages],
                [str(root / f"{name}.exe") for name in names[1:]],
            )
            for stage in stages:
                skipped = [
                    stage.argv[index + 1] for index, argument in enumerate(stage.argv[:-1]) if argument == "--skip"
                ]
                expected = (
                    [
                        "real_gui_save_close_kill_reopen_and_confirmed_service_exit",
                        "real_gui_pending_configuration_applies_only_after_inflight_completion",
                    ]
                    if stage.argv[0] == str(root / "acp_service_process.exe")
                    else []
                )
                self.assertEqual(skipped, expected)
                self.assertNotIn("--ignored", stage.argv)
                self.assertTrue(stage.watch_test_failures)


class GateCachedPayloadTests(unittest.TestCase):
    """缓存结果必须恢复原始数据；封套或损坏输出不能冒充通过。"""

    def setUp(self) -> None:
        workspace = test_gate.ROOT / ".tmp" / "test-gate" / "cached-payload"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        temporary = tempfile.TemporaryDirectory(dir=workspace)
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def _frame(self, body: bytes) -> bytes:
        raw = self.root / "raw.log"
        raw.write_bytes(body)
        output = io.BytesIO()
        with patch.object(sys, "stdout", MagicMock(buffer=output)):
            self.assertEqual(bazel_action._publish_log(raw, None, 0), 0)
        return b"Executing tests from //fixture:target\r\n------------------\r\n" + output.getvalue()

    def _consume(self, frame: bytes) -> gate_runtime.Result:
        native = self.root / "test.log"
        native.write_bytes(frame)
        return bazel_gate._native_result(
            gate_runtime.Stage("cached-payload", [sys.executable]),
            {
                "log": str(self.root / "consumer.log"),
                "environment": {},
                "result_file": str(self.root / "result"),
                "TEMP": None,
                "TMP": None,
            },
            {
                "result": {
                    "status": "PASSED",
                    "cachedLocally": True,
                    "testActionOutput": [{"name": "test.log", "uri": native.as_uri()}],
                },
            },
            self.root / "evidence.jsonl",
            timed_out=False,
        )

    def test_cached_listings_restore_exact_bytes_including_empty_and_binary(self) -> None:
        for body in (b"", b"alpha: test\r\nbeta: benchmark\n", b"no-final-newline\x00\xff"):
            with self.subTest(body=body):
                (self.root / "consumer.log").write_bytes(b"stale host data")
                result = self._consume(self._frame(body))
                self.assertEqual(result.status, "PASS")
                self.assertEqual((self.root / "consumer.log").read_bytes(), body)

    def test_damaged_cached_payload_never_passes_even_with_existing_host_log(self) -> None:
        frame = self._frame(b"alpha: test\n")
        for broken in (
            b"wrapper only",
            frame[:-1],
            frame + b"junk",
            frame.replace(b"alpha", b"omega", 1),
            frame.replace(b"PAYLOAD_V1", b"PAYLOAD_V9", 1),
        ):
            with self.subTest(frame=broken):
                (self.root / "consumer.log").write_bytes(b"stale host data")
                result = self._consume(broken)
                self.assertNotEqual(result.status, "PASS")
                self.assertNotEqual(result.returncode, 0)


class CargoArtifactSnapshotTests(unittest.TestCase):
    """使用真实磁盘文件验证产物恢复；真实 Cargo/Bazel 跨变体由隔离原生回归覆盖。"""

    def setUp(self) -> None:
        workspace = test_gate.ROOT / ".tmp" / "cargo-artifact-unit"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        self.temporary = tempfile.TemporaryDirectory(dir=workspace)
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        with patch.object(
            sys,
            "argv",
            ["make_tmp.py", "workspace", "--destination", str(self.root / ".tmp/bazel-gate")],
        ):
            self.assertEqual(make_tmp.main(), 0)
        self.target = self.root / "target/debug"
        self.target.mkdir(parents=True)
        self.program = self.target / "fixture.exe"
        self.depfile = self.target / "fixture.d"
        self.dll = self.target / "runtime.dll"
        self.pdb = self.target / "fixture.pdb"
        self.expected = {
            self.program: b"synthetic executable bytes; not a product program",
            self.depfile: b"fixture.exe: src/main.rs\n",
            self.dll: b"synthetic runtime library",
            self.pdb: b"synthetic symbol data",
        }
        for path, payload in self.expected.items():
            _ = path.write_bytes(payload)
        self.log = self.root / "cargo.log"
        _ = self.log.write_text(
            json.dumps(
                {
                    "reason": "compiler-artifact",
                    "target": {"kind": ["bin"], "name": "fixture"},
                    "profile": {"test": False},
                    "executable": str(self.program),
                    "filenames": [str(self.program), str(self.pdb)],
                }
            )
            + "\n",
            encoding="utf-8",
        )
        self.directory = self.root / "snapshot.cargo"
        self.manifest = self.root / "snapshot.cargo.json"
        bazel_action.capture_cargo_artifacts(self.root, self.log, self.directory, self.manifest)

    def _rewrite_snapshot(
        self,
        entries: list[tuple[str, bytes]],
        files: list[dict[str, object]],
        *,
        refresh_member_digests: bool = False,
    ) -> None:
        payload = self.directory / "payload.zip"
        with zipfile.ZipFile(payload, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=1) as archive:
            for name, data in entries:
                info = bazel_action._cargo_zip_info(name)
                with archive.open(info, "w", force_zip64=True) as writer:
                    _ = writer.write(data)
                if refresh_member_digests:
                    for record in files:
                        if record["path"] == name:
                            record["member_sha256"] = bazel_action._cargo_member_digest(archive, info)
            archive.writestr("metadata.json", json.dumps({"version": 3, "files": files}, sort_keys=True))
        self.manifest.write_text(
            json.dumps(
                {
                    "version": 3,
                    "files": files,
                    "payload_sha256": hashlib.sha256(payload.read_bytes()).hexdigest(),
                }
            ),
            encoding="utf-8",
        )

    def _snapshot_entries(self) -> list[tuple[str, bytes]]:
        with zipfile.ZipFile(self.directory / "payload.zip") as archive:
            return [
                (info.filename, archive.read(info)) for info in archive.infolist() if info.filename != "metadata.json"
            ]

    def test_member_digest_survives_writer_utf_flag_normalization_and_zip64_headers(self) -> None:
        for name in ("target/debug/ascii.dll", "target/debug/运行.dll"):
            with self.subTest(name=name):
                buffer = io.BytesIO()
                with zipfile.ZipFile(buffer, "w") as archive:
                    info = bazel_action._cargo_zip_info(name)
                    with archive.open(info, "w", force_zip64=True) as writer:
                        _ = writer.write(b"member with a finalized ZIP64 local header")
                    # force_zip64 的本地尺寸槽始终为占位值，真实尺寸在 extra field。
                    self.assertEqual(buffer.getvalue()[18:26], b"\xff" * 8)
                    flags = info.flag_bits
                    try:
                        # 在旧 Python 上也复现 3.13 的 ASCII 写侧内存 UTF 位差异。
                        if name.isascii():
                            info.flag_bits |= 0x800
                        expected = bazel_action._cargo_member_digest(archive, info)
                    finally:
                        info.flag_bits = flags
                _ = buffer.seek(0)
                with zipfile.ZipFile(buffer) as archive:
                    persisted = archive.getinfo(name)
                    self.assertEqual(bazel_action._cargo_member_digest(archive, persisted), expected)

    def test_payload_is_single_deterministic_compressed_file(self) -> None:
        second = self.root / "second.cargo"
        second_manifest = self.root / "second.cargo.json"
        bazel_action.capture_cargo_artifacts(self.root, self.log, second, second_manifest)
        self.assertEqual([path.name for path in self.directory.iterdir()], ["payload.zip"])
        self.assertEqual((self.directory / "payload.zip").read_bytes(), (second / "payload.zip").read_bytes())
        self.assertEqual(self.manifest.read_bytes(), second_manifest.read_bytes())
        with zipfile.ZipFile(self.directory / "payload.zip") as archive:
            self.assertTrue(all(info.compress_type == zipfile.ZIP_DEFLATED for info in archive.infolist()))

    def _bundled_engine(self) -> Path:
        engine = self.root / "resources/7zip/7z.exe"
        engine.parent.mkdir(parents=True)
        _ = engine.write_bytes(b"isolated engine placeholder; subprocess is patched")
        _ = engine.with_name("7z.dll").write_bytes(b"isolated library placeholder")
        return engine

    def test_native_engine_failure_cannot_publish_or_fall_back(self) -> None:
        _ = self._bundled_engine()
        directory, manifest = self.root / "failed.cargo", self.root / "failed.cargo.json"
        with (
            patch.object(bazel_action.subprocess, "run", return_value=subprocess.CompletedProcess([], 2, b"failed")),
            self.assertRaises(OSError),
        ):
            bazel_action.capture_cargo_artifacts(self.root, self.log, directory, manifest)
        self.assertFalse(manifest.exists())
        self.assertEqual(list(directory.iterdir()), [])

    def test_incomplete_bundled_engine_is_not_an_absent_engine_fallback(self) -> None:
        self._bundled_engine().with_name("7z.dll").unlink()
        manifest = self.root / "missing-library.cargo.json"
        with self.assertRaises(OSError):
            bazel_action.capture_cargo_artifacts(self.root, self.log, self.root / "missing-library.cargo", manifest)
        self.assertFalse(manifest.exists())

    def test_native_members_restore_stored_and_deflated_bytes_and_mtime(self) -> None:
        _ = self._bundled_engine()
        for compression in (zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED):
            with self.subTest(compression=compression):
                directory = self.root / f"native-{compression}.cargo"
                manifest = directory.with_suffix(".cargo.json")

                def compress(argv: list[str], **_kwargs: object) -> subprocess.CompletedProcess[bytes]:
                    with zipfile.ZipFile(directory / "payload.zip", "w", compression=compression) as archive:
                        for name in Path(argv[-1][1:]).read_text(encoding="utf-8").splitlines():
                            archive.write(self.root / name, name)
                    return subprocess.CompletedProcess(argv, 0, b"")

                expected_times = {path: path.stat().st_mtime_ns for path in self.expected}
                with patch.object(bazel_action.subprocess, "run", side_effect=compress):
                    bazel_action.capture_cargo_artifacts(self.root, self.log, directory, manifest)
                self.assertEqual([path.name for path in directory.iterdir()], ["payload.zip"])
                for path in self.expected:
                    path.unlink()
                bazel_action.restore_cargo_artifacts(self.root, directory, manifest)
                for path, data in self.expected.items():
                    self.assertEqual(path.read_bytes(), data)
                    self.assertEqual(path.stat().st_mtime_ns, expected_times[path])

    def test_native_missing_extra_directory_and_corrupt_members_cannot_publish(self) -> None:
        _ = self._bundled_engine()
        for damage in ("missing", "extra", "directory", "bytes", "method"):
            with self.subTest(damage=damage):
                directory = self.root / f"native-{damage}.cargo"
                manifest = directory.with_suffix(".cargo.json")

                def compress(argv: list[str], **_kwargs: object) -> subprocess.CompletedProcess[bytes]:
                    names = Path(argv[-1][1:]).read_text(encoding="utf-8").splitlines()
                    method = zipfile.ZIP_BZIP2 if damage == "method" else zipfile.ZIP_DEFLATED
                    with zipfile.ZipFile(directory / "payload.zip", "w", compression=method) as archive:
                        for name in names[1:] if damage == "missing" else names:
                            data = (self.root / name).read_bytes()
                            archive.writestr(name, b"corrupt" if damage == "bytes" else data)
                        if damage in {"extra", "directory"}:
                            archive.writestr("unexpected/" if damage == "directory" else "unexpected.dll", b"")
                    return subprocess.CompletedProcess(argv, 0, b"")

                with (
                    patch.object(bazel_action.subprocess, "run", side_effect=compress),
                    self.assertRaises(ValueError),
                ):
                    bazel_action.capture_cargo_artifacts(self.root, self.log, directory, manifest)
                self.assertFalse(manifest.exists())
                self.assertFalse((directory / "cargo-files.txt").exists())
                for path, data in self.expected.items():
                    self.assertEqual(path.read_bytes(), data)

    def test_equal_consumers_do_not_inflate_runtime_members(self) -> None:
        original = zipfile.ZipExtFile.read

        def read_metadata(member: zipfile.ZipExtFile, *args: object, **kwargs: object) -> object:
            self.assertEqual(member.name, "metadata.json", "匹配消费者必须只解压小型内嵌文件表")
            return original(member, *args, **kwargs)

        # 完整校验路径下同字节消费者也不解压运行文件；仅改 mtime 触发该路径。
        os.utime(self.program)
        with patch.object(zipfile.ZipExtFile, "read", autospec=True, side_effect=read_metadata):
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
        for path, expected in self.expected.items():
            self.assertEqual(path.read_bytes(), expected)

    def test_untouched_consumers_skip_byte_reverification(self) -> None:
        # 热轮（文件表声明的长度与 mtime 均未变化）不重读产物字节，也不打开压缩包。
        (self.directory / "payload.zip").write_bytes(b"broken archive must not be read")
        with (
            patch.object(bazel_action, "_cargo_digest", side_effect=AssertionError("热轮不得重读产物字节")),
            patch.object(bazel_action, "_cargo_snapshot_paths", side_effect=AssertionError("热轮不得打开压缩包")),
        ):
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
        for path, expected in self.expected.items():
            self.assertEqual(path.read_bytes(), expected)

    def test_changed_consumers_still_require_full_payload_hash(self) -> None:
        payload = self.directory / "payload.zip"
        original = payload.read_bytes()
        payload.write_bytes(original[:-1] + bytes([original[-1] ^ 1]))
        # 仅改 mtime 触发完整校验路径；字节仍与快照一致，失败只能来自整包摘要。
        os.utime(self.program)
        with self.assertRaises(ValueError):
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
        for path, expected in self.expected.items():
            self.assertEqual(path.read_bytes(), expected)

    def test_outer_file_table_cannot_claim_different_consumer_bytes(self) -> None:
        snapshot = json.loads(self.manifest.read_text(encoding="utf-8"))
        record = next(
            record for record in snapshot["files"] if record["path"] == self.program.relative_to(self.root).as_posix()
        )
        current = b"different consumer with altered outer metadata"
        self.program.write_bytes(current)
        record["sha256"] = hashlib.sha256(current).hexdigest()
        record["size"] = len(current)
        self.manifest.write_text(json.dumps(snapshot), encoding="utf-8")
        with self.assertRaises(ValueError):
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
        self.assertEqual(self.program.read_bytes(), current)

    def test_archive_member_missing_duplicate_and_size_mismatch_fail_closed(self) -> None:
        entries = self._snapshot_entries()
        files = json.loads(self.manifest.read_text(encoding="utf-8"))["files"]
        variants = (
            entries[:-1],
            [*entries, entries[0]],
            [(name, data + b"extra") if index == 0 else (name, data) for index, (name, data) in enumerate(entries)],
        )
        for index, changed in enumerate(variants):
            with self.subTest(variant=index):
                if index == 1:
                    with self.assertWarns(UserWarning):
                        self._rewrite_snapshot(changed, files)
                else:
                    self._rewrite_snapshot(changed, files)
                self.program.write_bytes(b"current consumer")
                self.depfile.write_bytes(b"current dep-info")
                with self.assertRaises(ValueError):
                    bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
                self.assertEqual(self.program.read_bytes(), b"current consumer")
                self.assertEqual(self.depfile.read_bytes(), b"current dep-info")

    def test_bound_but_incorrect_file_digest_rejects_before_consumer_writes(self) -> None:
        entries = self._snapshot_entries()
        files = json.loads(self.manifest.read_text(encoding="utf-8"))["files"]
        # 重签整个 payload 不得掩盖匹配消费者对应的坏成员；内嵌成员绑定仍是原值。
        name, data = entries[-1]
        entries[-1] = (name, bytes(byte ^ 1 for byte in data))
        self._rewrite_snapshot(entries, files)
        self.program.write_bytes(b"current consumer")
        self.depfile.write_bytes(b"current dep-info")
        with self.assertRaises(ValueError):
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
        self.assertEqual(self.program.read_bytes(), b"current consumer")
        self.assertEqual(self.depfile.read_bytes(), b"current dep-info")

    def test_refreshed_member_binding_still_checks_restored_uncompressed_digest(self) -> None:
        entries = self._snapshot_entries()
        files = json.loads(self.manifest.read_text(encoding="utf-8"))["files"]
        name, data = entries[-1]
        entries[-1] = (name, bytes(byte ^ 1 for byte in data))
        self._rewrite_snapshot(entries, files, refresh_member_digests=True)
        # 已重新绑定的压缩流仍不能恢复与文件 SHA 不符的字节。
        for path in self.expected:
            path.write_bytes(b"current consumer")
        with self.assertRaises(ValueError):
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
        for path in self.expected:
            self.assertEqual(path.read_bytes(), b"current consumer")

    def test_changed_consumers_reject_rebound_bad_member_without_inflating(self) -> None:
        entries = self._snapshot_entries()
        files = json.loads(self.manifest.read_text(encoding="utf-8"))["files"]
        name, data = entries[-1]
        entries[-1] = (name, bytes(byte ^ 1 for byte in data))
        self._rewrite_snapshot(entries, files)
        # 仅改 mtime 触发完整校验路径；压缩成员绑定篡改仍须在解压运行文件前拒绝。
        os.utime(self.program)
        original = zipfile.ZipExtFile.read

        def read_metadata(member: zipfile.ZipExtFile, *args: object, **kwargs: object) -> object:
            self.assertEqual(member.name, "metadata.json")
            return original(member, *args, **kwargs)

        with (
            patch.object(zipfile.ZipExtFile, "read", autospec=True, side_effect=read_metadata),
            self.assertRaises(ValueError),
        ):
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
        for path, expected in self.expected.items():
            self.assertEqual(path.read_bytes(), expected)

    def test_bound_metadata_rejects_invalid_schema_paths_digests_and_size_bounds(self) -> None:
        entries = self._snapshot_entries()
        original_files = json.loads(self.manifest.read_text(encoding="utf-8"))["files"]
        for key, invalid in (
            ("path", "../outside.exe"),
            ("path", "target/debug/../outside.exe"),
            ("path", "target\\debug\\outside.exe"),
            ("path", "src/main.rs"),
            ("sha256", "g" * 64),
            ("sha256", "0" * 63),
            ("member_sha256", "g" * 64),
            ("member_sha256", "0" * 63),
            ("size", -1),
            ("size", 1024**4 + 1),
            ("size", True),
            ("mtime_ns", -1),
        ):
            with self.subTest(key=key, invalid=invalid):
                files = [dict(record) for record in original_files]
                files[0][key] = invalid
                changed = [
                    (str(invalid), data) if key == "path" and index == 0 else (name, data)
                    for index, (name, data) in enumerate(entries)
                ]
                self._rewrite_snapshot(changed, files)
                self.program.write_bytes(b"current consumer")
                with self.assertRaises((TypeError, ValueError)):
                    bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
                self.assertEqual(self.program.read_bytes(), b"current consumer")

    def test_original_v1_v2_and_noninteger_versions_are_rejected(self) -> None:
        original = self.manifest.read_text(encoding="utf-8")
        for version in (1, 2, True, 3.0):
            with self.subTest(version=version):
                snapshot = json.loads(original)
                snapshot["version"] = version
                self.manifest.write_text(json.dumps(snapshot), encoding="utf-8")
                self.program.write_bytes(b"current consumer")
                with self.assertRaises(ValueError):
                    bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
                self.assertEqual(self.program.read_bytes(), b"current consumer")

    def test_duplicate_json_fields_are_rejected(self) -> None:
        original = self.manifest.read_text(encoding="utf-8")
        duplicate = '{"version": 3,' + original.lstrip()[1:]
        self.manifest.write_text(duplicate, encoding="utf-8")
        self.program.write_bytes(b"current consumer")
        with self.assertRaises(ValueError):
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
        self.assertEqual(self.program.read_bytes(), b"current consumer")

    def test_restoration_checks_archive_crc_before_consumer_writes(self) -> None:
        payload = self.directory / "payload.zip"
        data = bytearray(payload.read_bytes())
        with zipfile.ZipFile(payload) as archive:
            # 只破坏首个 central directory CRC，保留合法压缩流/文件 SHA/内嵌文件表。
            central = archive.start_dir
        data[central + 16] ^= 1
        payload.write_bytes(data)
        snapshot = json.loads(self.manifest.read_text(encoding="utf-8"))
        snapshot["payload_sha256"] = hashlib.sha256(data).hexdigest()
        self.manifest.write_text(json.dumps(snapshot), encoding="utf-8")
        self.program.write_bytes(b"current consumer")
        self.depfile.write_bytes(b"current dep-info")
        with self.assertRaises(ValueError):
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
        self.assertEqual(self.program.read_bytes(), b"current consumer")
        self.assertEqual(self.depfile.read_bytes(), b"current dep-info")
        self.assertEqual(list((self.root / ".tmp/bazel-gate").glob("cargo-restore-*")), [])

    def test_central_flags_remain_bound_after_writer_flag_normalization(self) -> None:
        payload = self.directory / "payload.zip"
        data = bytearray(payload.read_bytes())
        with zipfile.ZipFile(payload) as archive:
            central = archive.start_dir
        # ASCII 名称的 central UTF 位可以独立改变且仍能解析，不能忽略其篡改。
        data[central + 9] ^= 0x08
        payload.write_bytes(data)
        snapshot = json.loads(self.manifest.read_text(encoding="utf-8"))
        snapshot["payload_sha256"] = hashlib.sha256(data).hexdigest()
        self.manifest.write_text(json.dumps(snapshot), encoding="utf-8")
        # 重签整包不能绕过 central 标志绑定；仅改 mtime 触发完整校验路径。
        os.utime(self.program)
        with self.assertRaises(ValueError):
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
        for path, expected in self.expected.items():
            self.assertEqual(path.read_bytes(), expected)

    def test_existing_read_only_consumer_is_replaced_with_writable_bytes(self) -> None:
        self.program.write_bytes(b"readonly previous variant")
        self.program.chmod(self.program.stat().st_mode & ~(stat.S_IWUSR | stat.S_IWGRP | stat.S_IWOTH))
        try:
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
            self.assertEqual(self.program.read_bytes(), self.expected[self.program])
            self.assertTrue(self.program.stat().st_mode & stat.S_IWUSR)
            self.program.write_bytes(b"next Cargo build")
        finally:
            self.program.chmod(self.program.stat().st_mode | stat.S_IWUSR)

    def test_missing_or_malformed_manifest_never_restores_consumers(self) -> None:
        original = self.manifest.read_bytes()
        self.manifest.unlink()
        _ = self.program.write_bytes(b"current consumer")
        with self.assertRaises(OSError):
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
        self.assertEqual(self.program.read_bytes(), b"current consumer")
        _ = self.manifest.write_bytes(b'{"version": 3, "files":')
        with self.assertRaises(ValueError):
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
        self.assertEqual(self.program.read_bytes(), b"current consumer")
        _ = self.manifest.write_bytes(original)

    def test_changed_and_missing_consumers_restore_exact_snapshot_bytes(self) -> None:
        _ = self.program.write_bytes(b"overwritten by another build variant")
        self.depfile.unlink()
        _ = self.dll.write_bytes(b"changed runtime DLL")
        self.pdb.unlink()
        bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
        for path, expected in self.expected.items():
            with self.subTest(path=path):
                self.assertEqual(path.read_bytes(), expected)

    def test_equal_bytes_preserve_existing_consumer_timestamp(self) -> None:
        os.utime(self.program, ns=(1_000_000_000, 1_000_000_000))
        before = self.program.stat().st_mtime_ns
        bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
        self.assertEqual(self.program.read_bytes(), self.expected[self.program])
        self.assertEqual(self.program.stat().st_mtime_ns, before)

    def test_read_only_snapshot_restores_writable_consumer_without_changing_source(self) -> None:
        cached_program = self.directory / "payload.zip"
        original_mode = cached_program.stat().st_mode
        cached_program.chmod(original_mode & ~(stat.S_IWUSR | stat.S_IWGRP | stat.S_IWOTH))
        read_only_mode = cached_program.stat().st_mode
        source_mtime = self.program.stat().st_mtime_ns
        original_payload = cached_program.read_bytes()
        self.program.unlink()
        try:
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
            self.assertEqual(self.program.read_bytes(), self.expected[self.program])
            self.assertTrue(self.program.stat().st_mode & stat.S_IWUSR)
            self.assertEqual(self.program.stat().st_mtime_ns, source_mtime)
            _ = self.program.write_bytes(b"next Cargo build can overwrite restored output")
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
            self.assertEqual(self.program.read_bytes(), self.expected[self.program])
            _ = self.program.write_bytes(b"another build can still overwrite restored output")
            self.assertEqual(cached_program.stat().st_mode, read_only_mode)
            self.assertEqual(cached_program.read_bytes(), original_payload)
        finally:
            cached_program.chmod(original_mode)

    def test_missing_snapshot_file_rejects_without_partial_restoration(self) -> None:
        cached_program = self.directory / "payload.zip"
        cached_program.unlink()
        _ = self.program.write_bytes(b"current consumer remains untouched")
        _ = self.depfile.write_bytes(b"current dep-info remains untouched")
        with self.assertRaises((OSError, ValueError)):
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
        self.assertEqual(self.program.read_bytes(), b"current consumer remains untouched")
        self.assertEqual(self.depfile.read_bytes(), b"current dep-info remains untouched")

    def test_corrupted_snapshot_rejects_length_and_digest_mismatches(self) -> None:
        cached_program = self.directory / "payload.zip"
        expected = cached_program.read_bytes()
        for corrupted in (b"truncated", bytes(byte ^ 1 for byte in expected)):
            with self.subTest(length=len(corrupted)):
                _ = cached_program.write_bytes(corrupted)
                _ = self.program.write_bytes(b"current consumer")
                _ = self.depfile.write_bytes(b"current dep-info")
                with self.assertRaises(ValueError):
                    bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
                self.assertEqual(self.program.read_bytes(), b"current consumer")
                self.assertEqual(self.depfile.read_bytes(), b"current dep-info")
        _ = cached_program.write_bytes(expected)

    def test_manifest_path_escape_cannot_overwrite_real_outside_file(self) -> None:
        outside = self.root.parent / (self.root.name + "-outside.exe")
        _ = outside.write_bytes(b"outside sentinel")
        self.addCleanup(outside.unlink, missing_ok=True)
        original = self.manifest.read_text(encoding="utf-8")
        for relative in ("../" + outside.name, str(outside), "target/debug/../../" + outside.name):
            with self.subTest(relative=relative):
                snapshot = json.loads(original)
                snapshot["files"][0]["path"] = relative
                _ = self.manifest.write_text(json.dumps(snapshot), encoding="utf-8")
                with self.assertRaises((OSError, ValueError)):
                    bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
                self.assertEqual(outside.read_bytes(), b"outside sentinel")
        _ = self.manifest.write_text(original, encoding="utf-8")

    def test_missing_executable_dep_info_fails_capture(self) -> None:
        self.depfile.unlink()
        with self.assertRaises(OSError):
            bazel_action.capture_cargo_artifacts(
                self.root,
                self.log,
                self.root / "incomplete.cargo",
                self.root / "incomplete.cargo.json",
            )
        self.assertFalse((self.root / "incomplete.cargo.json").exists())

    def test_absent_or_malformed_cargo_artifact_evidence_cannot_publish_snapshot(self) -> None:
        for index, text in enumerate(
            (
                "",
                "Finished successfully, but no Cargo JSON evidence\n",
                '{"reason":"compiler-artifact"}\n',
                json.dumps(
                    {
                        "reason": "compiler-artifact",
                        "target": {"kind": ["lib"], "name": "fixture"},
                        "profile": {"test": False},
                        "filenames": "not a filename array",
                        "executable": None,
                    }
                )
                + "\n",
            )
        ):
            with self.subTest(index=index):
                _ = self.log.write_text(text, encoding="utf-8")
                directory = self.root / f"rejected-{index}.cargo"
                manifest = self.root / f"rejected-{index}.cargo.json"
                with self.assertRaises(ValueError):
                    bazel_action.capture_cargo_artifacts(self.root, self.log, directory, manifest)
                self.assertFalse(manifest.exists())
                self.assertFalse(directory.exists())

    def test_check_without_runtime_products_materializes_empty_snapshot(self) -> None:
        metadata = self.target / "fixture.rmeta"
        _ = metadata.write_bytes(b"compiler-only metadata")
        _ = self.log.write_text(
            json.dumps(
                {
                    "reason": "compiler-artifact",
                    "target": {"kind": ["lib"], "name": "fixture"},
                    "profile": {"test": False},
                    "executable": None,
                    "filenames": [str(metadata)],
                }
            )
            + "\n",
            encoding="utf-8",
        )
        directory = self.root / "check.cargo"
        manifest = self.root / "check.cargo.json"
        bazel_action.capture_cargo_artifacts(self.root, self.log, directory, manifest)
        self.assertTrue(directory.is_dir())
        self.assertTrue(manifest.is_file())
        self.assertEqual([path.name for path in directory.iterdir()], ["payload.zip"])
        self.assertEqual(json.loads(manifest.read_text(encoding="utf-8"))["files"], [])
        before = self.program.read_bytes()
        bazel_action.restore_cargo_artifacts(self.root, directory, manifest)
        self.assertEqual(self.program.read_bytes(), before)


class CargoArtifactBatchRestoreTests(unittest.TestCase):
    """批量复用仍验证每个生产者，消费者只接收最后变体的完整字节。"""

    def setUp(self) -> None:
        self.snapshot = CargoArtifactSnapshotTests()
        self.addCleanup(self.snapshot.doCleanups)
        self.snapshot.setUp()
        fixture = self.snapshot
        self.final_bytes = b"final variant consumer"
        _ = fixture.program.write_bytes(self.final_bytes)
        self.final_directory = fixture.root / "final.cargo"
        self.final_manifest = fixture.root / "final.cargo.json"
        bazel_action.capture_cargo_artifacts(
            fixture.root,
            fixture.log,
            self.final_directory,
            self.final_manifest,
        )
        self.snapshots = [
            (fixture.directory, fixture.manifest),
            (self.final_directory, self.final_manifest),
        ]

    def test_final_variant_restores_missing_consumer_and_preserves_equal_timestamp(self) -> None:
        fixture = self.snapshot
        fixture.program.unlink()
        bazel_action.restore_cargo_artifact_snapshots(fixture.root, self.snapshots)
        self.assertEqual(fixture.program.read_bytes(), self.final_bytes)
        os.utime(fixture.program, ns=(1_000_000_000, 1_000_000_000))
        before = fixture.program.stat().st_mtime_ns
        bazel_action.restore_cargo_artifact_snapshots(fixture.root, self.snapshots)
        self.assertEqual(fixture.program.read_bytes(), self.final_bytes)
        self.assertEqual(fixture.program.stat().st_mtime_ns, before)

    def test_corrupted_superseded_payload_rejects_before_any_consumer_write(self) -> None:
        fixture = self.snapshot
        payload = fixture.directory / "payload.zip"
        original = payload.read_bytes()
        _ = payload.write_bytes(bytes(byte ^ 1 for byte in original))
        _ = fixture.program.write_bytes(b"current executable")
        _ = fixture.depfile.write_bytes(b"current dependency data")
        with self.assertRaises(ValueError):
            bazel_action.restore_cargo_artifact_snapshots(fixture.root, self.snapshots)
        self.assertEqual(fixture.program.read_bytes(), b"current executable")
        self.assertEqual(fixture.depfile.read_bytes(), b"current dependency data")

    def test_missing_later_payload_rejects_before_any_consumer_write(self) -> None:
        fixture = self.snapshot
        payload = self.final_directory / "payload.zip"
        payload.unlink()
        _ = fixture.program.write_bytes(b"current executable")
        _ = fixture.depfile.write_bytes(b"current dependency data")
        with self.assertRaises((OSError, ValueError)):
            bazel_action.restore_cargo_artifact_snapshots(fixture.root, self.snapshots)
        self.assertEqual(fixture.program.read_bytes(), b"current executable")
        self.assertEqual(fixture.depfile.read_bytes(), b"current dependency data")

    def test_corrupted_later_payload_rejects_before_any_consumer_write(self) -> None:
        fixture = self.snapshot
        payload = self.final_directory / "payload.zip"
        original = payload.read_bytes()
        payload.write_bytes(original[:-1] + bytes([original[-1] ^ 1]))
        fixture.program.write_bytes(b"current executable")
        fixture.depfile.write_bytes(b"current dependency data")
        with self.assertRaises(ValueError):
            bazel_action.restore_cargo_artifact_snapshots(fixture.root, self.snapshots)
        self.assertEqual(fixture.program.read_bytes(), b"current executable")
        self.assertEqual(fixture.depfile.read_bytes(), b"current dependency data")

    def test_rebound_bad_member_in_either_batch_rejects_before_any_consumer_write(self) -> None:
        fixture = self.snapshot
        for index, (directory, manifest) in enumerate(self.snapshots):
            with self.subTest(snapshot=index):
                payload = directory / "payload.zip"
                original_payload = payload.read_bytes()
                original_manifest = manifest.read_bytes()
                data = bytearray(original_payload)
                with zipfile.ZipFile(payload) as archive:
                    central = archive.start_dir
                # 整包 SHA 已更新，但成员结构与被绑定的原始表不一致。
                data[central + 16] ^= 1
                payload.write_bytes(data)
                snapshot = json.loads(original_manifest)
                snapshot["payload_sha256"] = hashlib.sha256(data).hexdigest()
                manifest.write_text(json.dumps(snapshot), encoding="utf-8")
                fixture.program.write_bytes(b"current executable")
                fixture.depfile.write_bytes(b"current dependency data")
                try:
                    with self.assertRaises(ValueError):
                        bazel_action.restore_cargo_artifact_snapshots(fixture.root, self.snapshots)
                    self.assertEqual(fixture.program.read_bytes(), b"current executable")
                    self.assertEqual(fixture.depfile.read_bytes(), b"current dependency data")
                finally:
                    payload.write_bytes(original_payload)
                    manifest.write_bytes(original_manifest)
