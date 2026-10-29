"""打包验收测试用例（自 scripts/test_package_windows.py 原样搬移）."""

from __future__ import annotations
import base64
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
import uuid
from pathlib import Path
import zipfile
from scripts.gate_summary import ps51_module_path

from scripts.package_windows_tests.helpers import (
    ROOT,
    SCRIPT,
    FUNCTION_LOADER,
    DRIVER,
    LICENSE_DRIVER,
    INSTALLER_COMPILER_DRIVER,
    NOTICE_LOOP_DRIVER,
    SNAP_MANIFEST_DRIVER,
    TIMING_STORAGE_FAILURE_DRIVER,
    TIMING_LIVE_READER_DRIVER,
    PACKAGE_TEST_STAGE_DRIVER,
    ACCEPTANCE_TEST_STAGE_DRIVER,
    read_timing_snapshots,
)
class _PackageWindowsScriptCases:
    """脚本/许可/快照清单/计时用例（mixin；测试 ID 保持 PackageWindowsArtifactTests.*）。"""

    def test_package_deep_model_audio_exclusion_and_zip_preserve_source(self) -> None:
        model_relative = Path(
            "xberg/models/models--xberg-io--paddleocr-onnx-models/snapshots/"
            "bc5ec866cf0e798e667808dfa51b0ba8ad0dafc8/v2/classifiers/PP-LCNet_x1_0_textline_ori.onnx"
        )
        padding = 280 - len(str((self.root / "package" / model_relative).parent))
        self.assertGreaterEqual(padding, 0)
        source = self.root / ("package" + "p" * padding)
        self.assertEqual(len(str((source / model_relative).parent)), 280)
        self.assertGreater(len(str(source / model_relative)), 260)
        extended_source = self.extended_path(source)
        extended_source.mkdir(parents=True)
        self.addCleanup(shutil.rmtree, extended_source)
        members = {
            Path("JchTools.exe"): b"MZ synthetic non-executed package fixture",
            Path("xberg/xberg.exe"): b"MZ synthetic non-executed Xberg fixture",
            Path("licenses/许可证.txt"): "真实归档字节，不执行模型".encode(),
            model_relative: b"\x00\xffsynthetic model bytes; no inference claim\r\n",
        }
        for relative, payload in members.items():
            member = extended_source / relative
            member.parent.mkdir(parents=True, exist_ok=True)
            member.write_bytes(payload)
        (extended_source / "empty-assets").mkdir()
        before = self.installer_source_snapshot(source)
        result = self.run_package_tree(source)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.installer_source_snapshot(source), before)

        archive_root = self.root / "zip-output"
        destination = archive_root / ("a" * 120) / ("b" * 120) / "package.zip"
        self.assertGreater(len(str(destination.parent)), 260)
        self.extended_path(destination.parent).mkdir(parents=True)
        self.addCleanup(shutil.rmtree, self.extended_path(archive_root))
        result = self.run_package_tree(source, destination=destination)
        diagnostics = result.stdout + result.stderr
        self.assertEqual(result.returncode, 0, diagnostics)
        extended_archive = self.extended_path(destination)
        archive_bytes = extended_archive.read_bytes()
        with zipfile.ZipFile(extended_archive) as archive:
            entries = archive.infolist()
            self.assertEqual(len(entries), len({entry.filename for entry in entries}), diagnostics)
            self.assertEqual(
                {entry.filename for entry in entries if not entry.is_dir()},
                {f"{source.name}/{relative.as_posix()}" for relative in members},
                diagnostics,
            )
            self.assertTrue(archive.getinfo(f"{source.name}/empty-assets/").is_dir(), diagnostics)
            for entry in entries:
                self.assertTrue(entry.filename.startswith(f"{source.name}/"), entry.filename)
                self.assertNotIn("\\", entry.filename)
            for relative, payload in members.items():
                self.assertEqual(archive.read(f"{source.name}/{relative.as_posix()}"), payload, diagnostics)
        self.assertEqual(self.installer_source_snapshot(source), before, diagnostics)
        result = self.run_package_tree(source, destination=destination)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(extended_archive.read_bytes(), archive_bytes)
        self.assertEqual(self.installer_source_snapshot(source), before)

        # Exercise each existing exclusion branch without rebuilding the deep package.
        for relative in (
            model_relative.parent / "ffmpeg" / "payload.dat",
            model_relative.parent / "avcodec-63.dll",
            model_relative.parent / "model.int8.onnx",
        ):
            with self.subTest(audio=relative.as_posix()):
                member = extended_source / relative
                member.parent.mkdir(parents=True, exist_ok=True)
                member.write_bytes(b"forbidden audio-only fixture")
                try:
                    result = self.run_package_tree(source)
                    diagnostics = result.stdout + result.stderr
                    self.assertNotEqual(result.returncode, 0, diagnostics)
                    self.assertIn("默认安装包/ZIP 不得携带音频专属资产", diagnostics)
                    rejected = relative.parent if relative.parent.name == "ffmpeg" else relative
                    self.assertIn(rejected.as_posix(), diagnostics)
                finally:
                    member.unlink()
                    if relative.parent.name == "ffmpeg":
                        member.parent.rmdir()


    def test_inno6_compiles_deep_hf_source_without_mutation_or_alias_leak(self) -> None:
        # Synthetic storage-only payload: neither executable nor model is run or claimed to support OCR.
        import winreg

        model_relative = Path(
            "xberg/models/models--xberg-io--paddleocr-onnx-models/snapshots/"
            "bc5ec866cf0e798e667808dfa51b0ba8ad0dafc8/v2/classifiers/PP-LCNet_x1_0_textline_ori.onnx"
        )
        # Same failure geometry as the real release: the file crosses MAX_PATH, its parent does not.
        source_parent = ROOT / ".tmp"
        source_name_length = 262 - len(str(source_parent)) - len(str(model_relative)) - 2
        unique_name = "p-" + uuid.uuid4().hex
        self.assertGreaterEqual(source_name_length, len(unique_name))
        source = source_parent / unique_name.ljust(source_name_length, "s")
        self.assertEqual(len(str(source / model_relative)), 262)
        self.assertLess(len(str((source / model_relative).parent)), 260)
        extended = self.extended_path(source)
        extended.mkdir(parents=True)
        self.addCleanup(shutil.rmtree, extended)
        members = {
            Path("JchTools.exe"): b"MZ synthetic non-executed GUI fixture",
            Path("jchocr.exe"): b"MZ synthetic non-executed CLI fixture",
            Path("xberg/xberg.exe"): b"MZ synthetic non-executed Xberg fixture",
            Path("xberg/jchtools-xberg-release.json"): b'{"fixture":"synthetic non-OCR storage-only payload"}',
            model_relative: b"synthetic model bytes; no inference claim",
        }
        for relative, payload in members.items():
            member = extended / relative
            member.parent.mkdir(parents=True, exist_ok=True)
            member.write_bytes(payload)
        before = self.installer_source_snapshot(source)
        aliases_before = set((ROOT / ".tmp").glob("i-*"))
        destination = self.root / "installer-output"
        destination.mkdir()
        # Include the real installer with its supported isolation defines. Silent installation
        # suppresses [Run]; no fake payload is executed, no uninstall entry or real shortcut is made.
        isolation = ROOT / ".tmp" / ("s-" + uuid.uuid4().hex[:12])
        isolation.mkdir()
        self.addCleanup(shutil.rmtree, self.extended_path(isolation))
        self.assertLess(len(str(isolation / "installed" / model_relative)), 260)
        app_id = uuid.uuid4().hex
        registry_key = rf"Software\JchTools-Review-{app_id}"
        with self.assertRaises(FileNotFoundError):
            winreg.OpenKey(winreg.HKEY_CURRENT_USER, registry_key)

        def remove_isolated_registry_key() -> None:
            try:
                winreg.DeleteKey(winreg.HKEY_CURRENT_USER, registry_key)
            except FileNotFoundError:
                pass

        self.addCleanup(remove_isolated_registry_key)
        installer_script = self.root / "isolated-installer.iss"
        installer_script.write_text(
            f'#define ReviewRemediationRoot "{isolation}"\n'
            f'#define ReviewRemediationAppId "{app_id}"\n'
            f'#include "{ROOT / "installer" / "JchTools.iss"}"\n'
            "[Setup]\n"
            f"SourceDir={ROOT / 'installer'}\n"
            "Uninstallable=no\n",
            encoding="utf-8-sig",
        )
        result = self.run_installer_compiler(source, destination, installer_script)
        diagnostics = f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        self.assertEqual(self.installer_source_snapshot(source), before, diagnostics)
        self.assertEqual(set((ROOT / ".tmp").glob("i-*")), aliases_before, diagnostics)
        self.assertEqual(result.returncode, 0, diagnostics)
        setup = destination / "JchTools-Setup-x64.exe"
        self.assertTrue(setup.is_file(), diagnostics)
        self.assertGreater(setup.stat().st_size, 0, diagnostics)
        install_log = self.root / "isolated-install.log"
        installed = subprocess.run(
            [
                str(setup),
                "/VERYSILENT",
                "/SUPPRESSMSGBOXES",
                "/NORESTART",
                "/SP-",
                "/NOICONS",
                "/TASKS=!desktopicon,!quicklaunchicon",
                f"/LOG={install_log}",
            ],
            cwd=self.root,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=90,
            check=False,
        )
        install_diagnostics = (
            diagnostics + f"\ninstaller exit={installed.returncode}\nstdout:\n{installed.stdout}"
            f"\nstderr:\n{installed.stderr}\nlog:\n"
            + (install_log.read_text(encoding="utf-8-sig", errors="replace") if install_log.exists() else "(missing)")
        )
        self.assertEqual(installed.returncode, 0, install_diagnostics)
        for relative, payload in members.items():
            installed_path = (
                isolation / "WindowsApps" / relative.name
                if relative == Path("jchocr.exe")
                else isolation / "installed" / relative
            )
            extended_installed = self.extended_path(installed_path)
            self.assertTrue(extended_installed.is_file(), f"{relative}\n{install_diagnostics}")
            self.assertEqual(extended_installed.read_bytes(), payload, f"{relative}\n{install_diagnostics}")
        self.assertEqual(self.installer_source_snapshot(source), before, install_diagnostics)
        self.assertEqual(set((ROOT / ".tmp").glob("i-*")), aliases_before, install_diagnostics)


    def test_inno6_native_compile_error_removes_alias_without_mutating_source(self) -> None:
        source = self.root / "native-error-source"
        source.mkdir()
        for relative, payload in {
            "JchTools.exe": b"MZ synthetic non-executed GUI",
            "jchocr.exe": b"MZ synthetic non-executed CLI",
            "xberg/xberg.exe": b"MZ synthetic non-executed Xberg",
            "xberg/jchtools-xberg-release.json": b'{"fixture":"synthetic non-OCR"}',
            "xberg/models/retained.onnx": b"retained model bytes; no inference claim",
        }.items():
            member = source / relative
            member.parent.mkdir(parents=True, exist_ok=True)
            member.write_bytes(payload)
        installer_script = self.root / "native-error.iss"
        installer_script.write_text(
            f'#include "{ROOT / "installer" / "JchTools.iss"}"\n#error "JCHTOOLS_EXPECTED_NATIVE_COMPILER_ERROR"\n',
            encoding="utf-8-sig",
        )
        destination = self.root / "native-error-output"
        destination.mkdir()
        before = self.installer_source_snapshot(source)
        aliases_before = set((ROOT / ".tmp").glob("i-*"))
        result = self.run_installer_compiler(source, destination, installer_script)
        diagnostics = f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        self.assertNotEqual(result.returncode, 0, diagnostics)
        self.assertIn("JCHTOOLS_EXPECTED_NATIVE_COMPILER_ERROR", result.stdout + result.stderr, diagnostics)
        self.assertIn("Inno Setup compiler failed: 2", result.stdout + result.stderr, diagnostics)
        self.assertEqual(self.installer_source_snapshot(source), before, diagnostics)
        self.assertEqual(set((ROOT / ".tmp").glob("i-*")), aliases_before, diagnostics)
        self.assertFalse((destination / "JchTools-Setup-x64.exe").exists(), diagnostics)


    def test_newer_foreign_incomplete_does_not_reject_active_ok(self) -> None:
        self.write_status("ok")
        self.write_status("incomplete", foreign=True)
        result = self.run_selection()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), str(self.executable))


    def test_newer_foreign_ok_cannot_hide_active_incomplete(self) -> None:
        self.write_status("incomplete")
        self.write_status("ok", foreign=True)
        result = self.run_selection()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("incomplete", result.stderr)


    def test_missing_active_status_cannot_use_foreign_ok(self) -> None:
        self.write_status("ok", foreign=True)
        self.assertNotEqual(self.run_selection().returncode, 0)


    def test_missing_active_build_record_fails_closed(self) -> None:
        self.write_status("ok")
        self.records[2]["package_id"] = "registry+https://example.invalid#foreign@1.0.0"
        self.assertNotEqual(self.run_selection().returncode, 0)


    def test_ambiguous_active_build_records_fail_closed(self) -> None:
        self.write_status("ok")
        self.records.append(dict(self.records[2]))
        self.assertNotEqual(self.run_selection().returncode, 0)


    def test_configured_artifact_path_not_default_release_is_returned(self) -> None:
        result = self.run_selection(engine=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), str(self.executable))


    def test_same_binary_name_from_another_manifest_is_not_selected(self) -> None:
        foreign = dict(self.records[0])
        foreign["manifest_path"] = str(self.root / "dependency" / "Cargo.toml")
        foreign["executable"] = str(self.root / "wrong.exe")
        self.records.insert(0, foreign)
        result = self.run_selection(engine=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), str(self.executable))


    def test_ambiguous_binary_artifacts_fail_closed(self) -> None:
        self.records.append(dict(self.records[0]))
        self.assertNotEqual(self.run_selection(engine=False).returncode, 0)


    def test_recorded_nested_slint_licenses_preserve_paths_and_bytes(self) -> None:
        result, entry, destination = self.run_license_copy("slint")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertGreater(len(entry["license_and_notice_materials"]), 0)
        for relative in entry["license_and_notice_materials"]:
            self.assertEqual(
                (destination / relative).read_bytes(),
                (ROOT / entry["directory"] / relative).read_bytes(),
            )


    def test_recorded_nested_renderer_licenses_copy_to_deep_destination_without_source_mutation(self) -> None:
        package_name = "i-slint-renderer-software"
        manifest = json.loads((ROOT / "vendor" / "SOURCES.json").read_text(encoding="utf-8"))
        entry = next(package for package in manifest["packages"] if package["name"] == package_name)
        relative = Path("LICENSES/LicenseRef-Slint-Royalty-free-2.0.md")
        self.assertIn(relative.as_posix(), entry["license_and_notice_materials"])
        source = ROOT / entry["directory"]
        before = self.installer_source_snapshot(source)
        output_root = self.root / "deep-license-output"
        package_directory = f"{package_name}-{entry['version']}"
        padding = 280 - len(str(output_root / "third-party-rust" / package_directory / relative)) - 1
        self.assertGreater(padding, 0)
        self.assertLessEqual(padding, 255)
        destination = output_root / ("p" * padding) / "third-party-rust" / package_directory
        self.assertEqual(len(str(destination / relative)), 280)
        self.assertGreater(len(str(destination / relative)), 260)
        output_root.mkdir()
        self.addCleanup(shutil.rmtree, self.extended_path(output_root))

        result, copied_entry, copied_destination = self.run_license_copy(package_name, destination=destination)
        diagnostics = result.stdout + result.stderr
        self.assertEqual(result.returncode, 0, diagnostics)
        self.assertEqual(copied_destination, destination)
        extended_destination = self.extended_path(destination)
        expected = set(copied_entry["license_and_notice_materials"])
        self.assertEqual(
            {
                member.relative_to(extended_destination).as_posix()
                for member in extended_destination.rglob("*")
                if member.is_file()
            },
            expected,
            diagnostics,
        )
        for material in expected:
            self.assertEqual(
                (extended_destination / material).read_bytes(),
                (source / material).read_bytes(),
                f"{material}\n{diagnostics}",
            )
        self.assertEqual(self.installer_source_snapshot(source), before, diagnostics)


    def test_recorded_cfg_aliases_notice_is_shipped(self) -> None:
        result, entry, destination = self.run_license_copy("cfg_aliases")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            (destination / "NOTICES.md").read_bytes(),
            (ROOT / entry["directory"] / "NOTICES.md").read_bytes(),
        )


    def test_missing_recorded_license_fails_closed(self) -> None:
        result, _, _ = self.run_license_copy("slint", missing=True)
        self.assertNotEqual(result.returncode, 0)


    def test_actual_notice_loop_preserves_all_recorded_materials(self) -> None:
        destination = self.root / "actual-notice-loop"
        destination.mkdir()
        driver = self.root / "actual-notice-loop.ps1"
        driver.write_text(NOTICE_LOOP_DRIVER, encoding="utf-8-sig")
        environment = self.powershell_environment()
        environment.update(
            R24_SCRIPT=str(SCRIPT),
            R24_ROOT=str(ROOT),
            R24_DESTINATION=str(destination),
        )
        result = subprocess.run(
            [self.powershell, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", str(driver)],
            cwd=ROOT,
            env=environment,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=30,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        sources = json.loads((ROOT / "vendor" / "SOURCES.json").read_text(encoding="utf-8"))
        for entry in sources["packages"]:
            if entry["name"] not in {"slint", "cfg_aliases"}:
                continue
            for relative in entry["license_and_notice_materials"]:
                self.assertEqual(
                    (destination / f"{entry['name']}-{entry['version']}" / relative).read_bytes(),
                    (ROOT / entry["directory"] / relative).read_bytes(),
                )
        self.assertFalse((destination / "jchtools-assets-0.1.0").exists(), "workspace 自有 crate 不属于第三方")


    def test_snap_manifest_build_input_is_independent_of_destination_root(self) -> None:
        worker = self.prepare_snap_manifest_fixture()
        first = self.run_snap_manifest_stage(worker, self.root / "交付目录 A")
        second = self.run_snap_manifest_stage(worker, self.root / "交付目录 B")
        for field in ("path", "bytes_base64", "last_write_time_utc_ticks"):
            with self.subTest(build_input=field):
                self.assertEqual(first[field], second[field], f"仅变更 DestinationRoot 不得改变主构建输入 {field}")


    def test_snap_manifest_unchanged_worker_preserves_bytes_and_last_write_time(self) -> None:
        worker = self.prepare_snap_manifest_fixture()
        destination = self.root / "重复交付目录"
        first = self.run_snap_manifest_stage(worker, destination)
        manifest_path = Path(first["path"])
        # 使用确定的旧时间，避免依赖 sleep 或文件系统的写入时间分辨率。
        stamp = 946684800
        os.utime(manifest_path, (stamp, stamp))
        expected_ticks = str(621355968000000000 + manifest_path.stat().st_mtime_ns // 100)
        second = self.run_snap_manifest_stage(worker, destination)
        self.assertEqual(first["path"], second["path"])
        self.assertEqual(first["bytes_base64"], second["bytes_base64"])
        self.assertEqual(
            second["last_write_time_utc_ticks"],
            expected_ticks,
            "worker 与源清单均未变化时不得重写 Cargo rerun-if-changed 输入",
        )


    def test_snap_manifest_changed_worker_refreshes_sha256_and_size(self) -> None:
        worker = self.prepare_snap_manifest_fixture()
        destination = self.root / "worker 更新交付目录"
        first = self.run_snap_manifest_stage(worker, destination)
        original_size = worker.stat().st_size
        worker.write_bytes(b"MZ\x00real changed worker bytes\x00version-two-with-a-different-size")
        self.assertNotEqual(worker.stat().st_size, original_size)
        second = self.run_snap_manifest_stage(worker, destination)
        self.assertNotEqual(first["bytes_base64"], second["bytes_base64"], "真实 worker 变更不得静默复用旧清单")
        before = json.loads(base64.b64decode(first["bytes_base64"]))["workers"][0]
        after = json.loads(base64.b64decode(second["bytes_base64"]))["workers"][0]
        self.assertNotEqual(before["sha256"], after["sha256"])
        self.assertNotEqual(before["size_bytes"], after["size_bytes"])


    def test_snap_manifest_corrupt_content_addressed_input_is_repaired(self) -> None:
        worker = self.prepare_snap_manifest_fixture()
        first = self.run_snap_manifest_stage(worker, self.root / "清单篡改交付 A")
        Path(first["path"]).write_bytes(b"corrupt input")
        second = self.run_snap_manifest_stage(worker, self.root / "清单篡改交付 B")
        self.assertEqual(first["path"], second["path"])
        self.assertEqual(first["bytes_base64"], second["bytes_base64"])


    def test_snap_manifest_source_content_change_selects_new_build_input(self) -> None:
        worker = self.prepare_snap_manifest_fixture()
        first = self.run_snap_manifest_stage(worker, self.root / "源清单变更交付 A")
        source = self.root / "resources" / "snap-ocr-assets.json"
        manifest = json.loads(source.read_text(encoding="utf-8"))
        manifest["version"] = "regression-real-source-change"
        source.write_text(json.dumps(manifest, ensure_ascii=False), encoding="utf-8")
        second = self.run_snap_manifest_stage(worker, self.root / "源清单变更交付 B")
        self.assertNotEqual(first["path"], second["path"])
        self.assertNotEqual(first["bytes_base64"], second["bytes_base64"])
        self.assertEqual(json.loads(base64.b64decode(second["bytes_base64"]))["version"], manifest["version"])


    def test_main_build_failure_preserves_completed_times_and_unrun_steps(self) -> None:
        worker = self.prepare_snap_manifest_fixture()
        timing = self.run_snap_manifest_stage(worker, self.root / "失败交付目录", fail_main=True)
        steps = {entry["step"]: entry for entry in timing["steps"]}
        for name in ("worker-build", "manifest"):
            self.assertEqual(steps[name]["status"], "PASS")
            self.assertGreater(steps[name]["elapsed_seconds"], 0)
            self.assertIsNotNone(steps[name]["finished_utc"])
        self.assertEqual(steps["main-build"]["status"], "FAIL")
        self.assertIn("controlled main build failure", steps["main-build"]["detail"])
        for name in ("staging", "licenses", "package-validation", "installer", "zip"):
            self.assertEqual(steps[name]["status"], "NOT_RUN")
            self.assertEqual(steps[name]["elapsed_seconds"], 0)
            self.assertIsNone(steps[name]["started_utc"])
        self.assertEqual(timing["total"]["status"], "FAIL")
        self.assertGreaterEqual(
            timing["total"]["elapsed_seconds"], sum(entry["elapsed_seconds"] for entry in timing["steps"])
        )


    def test_successful_stage_records_transitions_without_claiming_delivery(self) -> None:
        worker = self.prepare_snap_manifest_fixture()
        snapshot = self.run_snap_manifest_stage(worker, self.root / "阶段计时交付目录")
        timing = snapshot["timing"]
        steps = {entry["step"]: entry for entry in timing["steps"]}
        for name in ("worker-build", "manifest", "main-build"):
            self.assertEqual(steps[name]["status"], "PASS")
            self.assertGreater(steps[name]["elapsed_seconds"], 0)
            self.assertIsNotNone(steps[name]["started_utc"])
            self.assertIsNotNone(steps[name]["finished_utc"])
        self.assertEqual(steps["installer"]["status"], "NOT_RUN")
        self.assertEqual(steps["zip"]["status"], "NOT_RUN")
        self.assertEqual(timing["total"]["status"], "PASS")
        history = snapshot["timing_snapshots"]
        self.assertGreater(len(history), 1)
        for name in ("worker-build", "manifest", "main-build"):
            statuses = {entry["status"] for record in history for entry in record["steps"] if entry["step"] == name}
            self.assertIn("RUNNING", statuses)
            self.assertIn("PASS", statuses)


    def test_live_reader_observes_appended_transitions_and_ignores_incomplete_tail(self) -> None:
        driver = self.root / "timing-live-reader.ps1"
        driver.write_text(TIMING_LIVE_READER_DRIVER, encoding="utf-8-sig")
        pointer = self.root / "timing-reader-pointer.txt"
        saved = self.root / "timing-reader-observed.jsonl"
        environment = self.powershell_environment()
        environment.update(
            R24_SCRIPT=str(SCRIPT),
            R24_FUNCTIONS="Initialize-PackageTiming,Save-PackageTiming,Stop-PackageStep,Start-PackageStep,Complete-PackageTiming",
            R24_REPOSITORY=str(ROOT),
            R24_TIMING_POINTER=str(pointer),
            R24_SAVED_TIMING=str(saved),
        )
        result = subprocess.run(
            [self.powershell, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", str(driver)],
            cwd=ROOT,
            env=environment,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=30,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        timing_path = Path(pointer.read_text(encoding="utf-8"))
        self.addCleanup(shutil.rmtree, timing_path.parent)
        self.assertEqual(saved.read_bytes(), timing_path.read_bytes())
        snapshots = read_timing_snapshots(saved)
        self.assertGreater(len(snapshots), 1)
        final = snapshots[-1]
        self.assertEqual(final["total"]["status"], "PASS")
        steps = {entry["step"]: entry for entry in final["steps"]}
        for name in ("setup", "worker-build", "manifest"):
            self.assertEqual(steps[name]["status"], "PASS")
            self.assertGreater(steps[name]["elapsed_seconds"], 0)
        self.assertEqual(steps["installer"]["status"], "NOT_RUN")
        with saved.open("ab") as stream:
            stream.write(b'{"total":')
        self.assertEqual(read_timing_snapshots(saved), snapshots)


    def test_timing_storage_failure_preserves_previous_record_and_original_error(self) -> None:
        driver = self.root / "timing-storage-failure.ps1"
        driver.write_text(TIMING_STORAGE_FAILURE_DRIVER, encoding="utf-8-sig")
        pointer = self.root / "timing-storage-pointer.txt"
        saved = self.root / "timing-storage-before.json"
        environment = self.powershell_environment()
        environment.update(
            R24_SCRIPT=str(SCRIPT),
            R24_FUNCTIONS="Initialize-PackageTiming,Save-PackageTiming,Stop-PackageStep,Start-PackageStep,Complete-PackageTiming",
            R24_REPOSITORY=str(ROOT),
            R24_TIMING_POINTER=str(pointer),
            R24_SAVED_TIMING=str(saved),
        )
        result = subprocess.run(
            [self.powershell, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", str(driver)],
            cwd=ROOT,
            env=environment,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=30,
            check=False,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("controlled original packaging failure", result.stderr)
        self.assertIn("Unable to finalize package timing", result.stdout + result.stderr)
        timing_path = Path(pointer.read_text(encoding="utf-8"))
        self.addCleanup(shutil.rmtree, timing_path.parent)
        self.assertEqual(timing_path.read_bytes(), saved.read_bytes())
        steps = {entry["step"]: entry for entry in read_timing_snapshots(saved)[-1]["steps"]}
        self.assertEqual(steps["setup"]["status"], "PASS")
        self.assertGreater(steps["setup"]["elapsed_seconds"], 0)
        self.assertEqual(steps["worker-build"]["status"], "RUNNING")
        self.assertEqual(steps["zip"]["status"], "NOT_RUN")
