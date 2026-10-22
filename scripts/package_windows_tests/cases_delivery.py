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
from scripts.test_gate import _ps51_module_path

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
class _PackageWindowsDeliveryCases:
    """打包验收/引擎库/Xberg 装配用例（mixin；测试 ID 保持 PackageWindowsArtifactTests.*）。"""

    def test_default_package_tests_use_existing_non_desktop_classifier(self) -> None:
        # 覆盖 P-12 / AGENTS.md 3.4：-WithPackage 的默认调用也不能激活用户桌面。
        observations = self.root / "test-processes.jsonl"
        records = []
        for name in ("ordinary", "gui_flow", "acp_service_process"):
            executable = self.root / f"{name}.ps1"
            executable.write_text(
                "$ErrorActionPreference = 'Stop'\n"
                f"$record = @{{name='{name}';argv=@($args)}} | ConvertTo-Json -Compress\n"
                '[IO.File]::AppendAllText($env:R25_OBSERVATIONS, $record + "`n", [Text.UTF8Encoding]::new($false))\n'
                "exit 0\n",
                encoding="utf-8-sig",
            )
            records.append(
                {
                    "reason": "compiler-artifact",
                    "profile": {"test": True},
                    "target": {"name": name, "kind": ["test"]},
                    "executable": str(executable),
                }
            )
        records_path = self.root / "test-artifacts.json"
        records_path.write_text(json.dumps(records), encoding="utf-8")
        driver = self.root / "package-tests-stage.ps1"
        driver.write_text(PACKAGE_TEST_STAGE_DRIVER, encoding="utf-8-sig")
        environment = self.powershell_environment()
        environment.update(
            R24_SCRIPT=str(SCRIPT),
            R24_FUNCTIONS="Invoke-Cargo",
            R25_REPOSITORY=str(ROOT),
            R25_WORKSPACE=str(self.root),
            R25_RECORDS=str(records_path),
            R25_OBSERVATIONS=str(observations),
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
        processes = [json.loads(line) for line in observations.read_text(encoding="utf-8").splitlines()]
        self.assertEqual([process["name"] for process in processes], ["ordinary", "acp_service_process"])
        self.assertEqual(processes[0]["argv"], ["--nocapture", "--test-threads=2"])
        self.assertEqual(
            processes[1]["argv"],
            [
                "--nocapture",
                "--test-threads=2",
                "--skip",
                "real_gui_save_close_kill_reopen_and_confirmed_service_exit",
                "--skip",
                "real_gui_pending_configuration_applies_only_after_inflight_completion",
            ],
        )
        self.assertEqual((self.root / "real-engine-step.txt").read_text(encoding="utf-8"), "executed")
        self.assertIn("NOT RUN", result.stdout)
        self.assertIn("desktop", result.stdout)


    def test_acceptance_package_keeps_desktop_authorization_explicit(self) -> None:
        # 覆盖 P-12/P-13：默认分类不漏普通测试，显式授权仍保留完整桌面入口。
        for desktop in (False, True):
            with self.subTest(desktop=desktop):
                workspace = self.root / ("desktop" if desktop else "default")
                workspace.mkdir()
                observations = workspace / "test-processes.jsonl"
                records = []
                for name in ("ordinary", "gui_flow", "acp_service_process"):
                    executable = workspace / f"{name}.ps1"
                    executable.write_text(
                        f"$record = @{{name='{name}';argv=@($args)}} | ConvertTo-Json -Compress\n"
                        '[IO.File]::AppendAllText($env:R25_OBSERVATIONS, $record + "`n", [Text.UTF8Encoding]::new($false))\n'
                        "exit 0\n",
                        encoding="utf-8-sig",
                    )
                    records.append(
                        {
                            "reason": "compiler-artifact",
                            "profile": {"test": True},
                            "target": {"name": name, "kind": ["test"]},
                            "executable": str(executable),
                        }
                    )
                records_path = workspace / "test-artifacts.json"
                records_path.write_text(json.dumps(records), encoding="utf-8")
                driver = workspace / "acceptance-tests-stage.ps1"
                driver.write_text(ACCEPTANCE_TEST_STAGE_DRIVER, encoding="utf-8-sig")
                environment = self.powershell_environment()
                environment.update(
                    R24_SCRIPT=str(ROOT / "scripts" / "acceptance.ps1"),
                    R24_FUNCTIONS="Invoke-Logged",
                    R25_REPOSITORY=str(ROOT),
                    R25_WORKSPACE=str(workspace),
                    R25_RECORDS=str(records_path),
                    R25_OBSERVATIONS=str(observations),
                    R25_PYTHON=sys.executable,
                    R25_DESKTOP="1" if desktop else "0",
                )
                result = subprocess.run(
                    [
                        self.powershell,
                        "-NoProfile",
                        "-NonInteractive",
                        "-ExecutionPolicy",
                        "Bypass",
                        "-File",
                        str(driver),
                    ],
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
                observation = json.loads((workspace / "acceptance-observation.json").read_text(encoding="utf-8"))
                self.assertEqual([call["file"] for call in observation["calls"]], ["cargo", "powershell"])
                expected_cargo = ["test", "--all-targets", "--features", "test-hooks"]
                if not desktop:
                    expected_cargo += ["--no-run", "--message-format=json"]
                self.assertEqual(observation["calls"][0]["argv"], expected_cargo)
                self.assertEqual(
                    observation["calls"][1]["argv"],
                    ["-NoProfile", "-File", str(SCRIPT)],
                )
                if desktop:
                    self.assertFalse(observations.exists())
                    self.assertFalse(any("NOT RUN" in entry for entry in observation["results"]))
                else:
                    processes = [json.loads(line) for line in observations.read_text(encoding="utf-8").splitlines()]
                    self.assertEqual([process["name"] for process in processes], ["ordinary", "acp_service_process"])
                    self.assertEqual(processes[0]["argv"], ["--nocapture", "--test-threads=2"])
                    self.assertEqual(
                        processes[1]["argv"],
                        [
                            "--nocapture",
                            "--test-threads=2",
                            "--skip",
                            "real_gui_save_close_kill_reopen_and_confirmed_service_exit",
                            "--skip",
                            "real_gui_pending_configuration_applies_only_after_inflight_completion",
                        ],
                    )
                    self.assertTrue(any(entry.startswith("NOT RUN  desktop-tests") for entry in observation["results"]))


    def test_missing_engine_library_artifact_cannot_use_main_out_dir(self) -> None:
        self.write_status("ok")
        del self.records[1]
        self.records[1]["package_id"] = self.records[0]["package_id"]
        self.assertNotEqual(self.run_selection().returncode, 0)


    def test_ambiguous_engine_library_artifacts_fail_closed(self) -> None:
        self.write_status("ok")
        self.records.append(dict(self.records[1]))
        self.assertNotEqual(self.run_selection().returncode, 0)


    def test_xberg_complete_base_receipt_and_same_root_refresh_preserve_audio_and_config(self) -> None:
        fixture = self.xberg_fixture()
        result = self.run_xberg_policy(fixture)
        self.assertEqual(result.returncode, 0, result.stderr)
        staged = self.root / "staged"
        self.assertEqual(json.loads((staged / "jchtools-xberg-release.json").read_text()), fixture["receipt"])
        self.assertFalse((staged / "ffmpeg").exists())
        installed = self.root / "installed"
        (installed / "ffmpeg").mkdir(parents=True)
        (installed / "ffmpeg" / "avcodec-63.dll").write_bytes(b"downloaded-audio")
        (installed / "config.json").write_bytes(b"user-config")
        (installed / "xberg.exe").write_bytes(b"old-engine")
        result = self.run_xberg_policy(fixture, "install")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((installed / "ffmpeg" / "avcodec-63.dll").read_bytes(), b"downloaded-audio")
        self.assertEqual((installed / "config.json").read_bytes(), b"user-config")
        self.assertEqual((installed / "xberg.exe").read_bytes(), (staged / "xberg.exe").read_bytes())


    def test_xberg_install_deep_model_path_preserves_audio_config_and_member_bytes(self) -> None:
        snapshot_id = "bc5ec866cf0e798e667808dfa51b0ba8ad0dafc8"
        fixture = self.xberg_fixture(snapshot_id=snapshot_id)
        model_relative = Path(
            f"models/models--xberg-io--paddleocr-onnx-models/snapshots/{snapshot_id}/"
            "v2/classifiers/PP-LCNet_x1_0_textline_ori.onnx"
        )
        result = self.run_xberg_policy(fixture)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        staged = self.root / "staged"
        self.assertLess(len(str(staged / model_relative)), 260)
        package_root = self.root / "package"
        padding = 280 - len(str(package_root / "xberg-base" / "xberg" / model_relative))
        self.assertGreaterEqual(padding, 0)
        installed = package_root / ("xberg-base" + "d" * padding) / "xberg"
        self.assertEqual(len(str(installed / model_relative)), 280)
        self.assertGreater(len(str(installed / model_relative)), 260)
        self.assertLess(len(str((installed / model_relative).parent)), 260)
        extended_installed = self.extended_path(installed)
        extended_installed.mkdir(parents=True)
        self.addCleanup(shutil.rmtree, self.extended_path(package_root))
        preserved = {
            Path("ffmpeg/avcodec-63.dll"): b"previously-downloaded-audio",
            Path("models/sense_voice_small/model.int8.onnx"): b"previously-downloaded-speech-model",
            Path("config.json"): b'{"user_config":"keep exactly"}',
        }
        for relative, payload in preserved.items():
            member = extended_installed / relative
            member.parent.mkdir(parents=True, exist_ok=True)
            member.write_bytes(payload)
        (extended_installed / "xberg.exe").write_bytes(b"old-engine")
        result = self.run_xberg_policy(fixture, "install", destination=installed)
        diagnostics = result.stdout + result.stderr
        self.assertEqual(result.returncode, 0, diagnostics)
        expected = {
            member["path"]: member
            for component in fixture["receipt"]["components"]
            for member in component["members"]
        }
        self.assertIn(model_relative.as_posix(), expected)
        for relative, member in expected.items():
            with self.subTest(member=relative):
                deployed = extended_installed / relative
                self.assertTrue(deployed.is_file(), diagnostics)
                payload = deployed.read_bytes()
                self.assertEqual(payload, (staged / relative).read_bytes(), diagnostics)
                self.assertEqual(len(payload), member["size_bytes"], diagnostics)
                self.assertEqual(hashlib.sha256(payload).hexdigest(), member["sha256"], diagnostics)
        self.assertEqual(
            (extended_installed / "jchtools-xberg-release.json").read_bytes(),
            (staged / "jchtools-xberg-release.json").read_bytes(),
            diagnostics,
        )
        for relative, payload in preserved.items():
            with self.subTest(preserved=relative):
                self.assertEqual((extended_installed / relative).read_bytes(), payload, diagnostics)


    def test_xberg_missing_common_model_identity_and_shared_conflict_fail_closed(self) -> None:
        original = self.xberg_fixture()
        for mutation in ("missing-common", "wrong-model", "shared-conflict", "audio-in-base", "audio-overwrites-base"):
            with self.subTest(mutation=mutation):
                fixture = {"receipt": json.loads(json.dumps(original["receipt"])), "root": original["root"]}
                if mutation == "missing-common":
                    fixture["receipt"]["components"][0]["members"] = [
                        m for m in fixture["receipt"]["components"][0]["members"] if m["path"] != "sherpa-onnx/LICENSE"
                    ]
                elif mutation == "wrong-model":
                    next(m for m in fixture["receipt"]["components"][0]["members"] if "/tiny/" in m["path"])[
                        "sha256"
                    ] = "f" * 64
                elif mutation == "shared-conflict":
                    next(m for m in fixture["receipt"]["components"][1]["members"] if m["path"] == "xberg.exe")[
                        "sha256"
                    ] = "f" * 64
                elif mutation == "audio-in-base":
                    fixture["receipt"]["components"][0]["members"].append(
                        self.xberg_member("ffmpeg/avcodec-63.dll", b"x")
                    )
                else:
                    fixture["receipt"]["audio_assets"]["members"].append(self.xberg_member("xberg.exe", b"x"))
                self.assertNotEqual(self.run_xberg_policy(fixture).returncode, 0)


    def test_xberg_archive_digest_mismatch_is_rejected(self) -> None:
        fixture = self.xberg_fixture()
        fixture["receipt"]["components"][0]["sha256"] = "f" * 64
        self.assertNotEqual(self.run_xberg_policy(fixture).returncode, 0)


    def test_xberg_zip_missing_unlisted_duplicate_unsafe_symlink_and_corrupt_members_rejected(self) -> None:
        fixture = self.xberg_fixture()
        archive = self.root / fixture["receipt"]["components"][0]["archive"]
        with zipfile.ZipFile(archive) as source:
            members = [(entry.filename, source.read(entry)) for entry in source.infolist()]
        for mutation in ("missing", "unlisted", "duplicate", "traversal", "ads", "symlink", "corrupt"):
            with self.subTest(mutation=mutation):
                changed = list(members)
                if mutation == "missing":
                    changed.pop()
                elif mutation == "unlisted":
                    changed.append(("xberg-cli-x86_64-pc-windows-msvc/extra.dll", b"x"))
                elif mutation == "duplicate":
                    prefix, path = changed[0][0].split("/", 1)
                    changed.append((prefix + "/" + path.upper(), changed[0][1]))
                elif mutation in ("traversal", "ads"):
                    path = "../outside" if mutation == "traversal" else "xberg.exe:stream"
                    changed.append(("xberg-cli-x86_64-pc-windows-msvc/" + path, b"x"))
                elif mutation == "symlink":
                    info = zipfile.ZipInfo(changed[0][0])
                    info.create_system = 3
                    info.external_attr = 0o120777 << 16
                    changed[0] = (info.filename, info)
                else:
                    changed[0] = (changed[0][0], b"z" * len(changed[0][1]))
                self.rewrite_xberg_zip(fixture, changed)
                self.assertNotEqual(self.run_xberg_policy(fixture).returncode, 0)


    def test_xberg_final_tree_detects_audio_leak_and_missing_base(self) -> None:
        fixture = self.xberg_fixture()
        result = self.run_xberg_policy(fixture)
        self.assertEqual(result.returncode, 0, result.stderr)
        (self.root / "staged" / "ffmpeg").mkdir()
        self.assertNotEqual(self.run_xberg_policy(fixture, "validate").returncode, 0)
        (self.root / "staged" / "ffmpeg").rmdir()
        (self.root / "staged" / "xberg.exe").unlink()
        self.assertNotEqual(self.run_xberg_policy(fixture, "validate").returncode, 0)


    def test_xberg_fixed_api_provenance_and_digest_are_required(self) -> None:
        name = "xberg-document-x86_64-pc-windows-msvc.zip"
        release = {
            "tag_name": "v-test",
            "draft": False,
            "prerelease": False,
            "html_url": "https://github.com/jchanghong023/xberg/releases/tag/v-test",
            "assets": [
                {
                    "name": name,
                    "state": "uploaded",
                    "size": 123,
                    "digest": "sha256:" + "a" * 64,
                    "browser_download_url": "https://github.com/jchanghong023/xberg/releases/download/v-test/" + name,
                }
            ],
        }
        fixture = {"receipt": release, "root": self.root}
        result = self.run_xberg_policy(fixture, "api")
        self.assertEqual(result.returncode, 0, result.stderr)
        for key, value in (("digest", None), ("browser_download_url", "https://example.invalid/base.zip"), ("size", 0)):
            changed = json.loads(json.dumps(release))
            changed["assets"][0][key] = value
            self.assertNotEqual(self.run_xberg_policy({"receipt": changed, "root": self.root}, "api").returncode, 0)


    def test_xberg_offline_cache_is_rehashed_not_trusted_by_filename(self) -> None:
        payload = b"cached-archive"
        record = self.xberg_member("unused", payload) | {
            "archive": "document.zip",
            "url": "https://example.invalid/never",
        }
        cache = self.root / f"{record['sha256']}-{record['archive']}"
        cache.write_bytes(payload)
        fixture = {"receipt": record, "root": self.root}
        result = self.run_xberg_policy(fixture, "cache")
        self.assertEqual(result.returncode, 0, result.stderr)
        cache.write_bytes(b"corrupt")
        self.assertNotEqual(self.run_xberg_policy(fixture, "cache").returncode, 0)
        cache.unlink()
        self.assertNotEqual(self.run_xberg_policy(fixture, "cache").returncode, 0)


    def test_resolved_patched_packages_use_local_path_sources(self) -> None:
        # 真实 Cargo resolver 查询：仅读取元数据，不编译、不执行任何 crate。
        sources = json.loads((ROOT / "vendor" / "SOURCES.json").read_text(encoding="utf-8"))
        patched = [entry for entry in sources["packages"] if entry["effective_source"]["type"] == "path"]
        self.assertTrue(patched, "来源清单必须登记实际本地补丁，不能空跑")
        expected_identities = {(entry["name"], entry["version"]) for entry in patched}
        self.assertEqual(len(patched), len(expected_identities), "来源清单不能重复登记同一包")
        for entry in sources["packages"]:
            if entry["project_source_modifications"]:
                self.assertEqual(entry["effective_source"]["type"], "path", entry["name"])
        result = subprocess.run(
            ["cargo", "metadata", "--locked", "--offline", "--format-version", "1"],
            cwd=ROOT,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=30,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        metadata = json.loads(result.stdout)
        packages = {package["id"]: package for package in metadata["packages"]}
        self.assertEqual(
            {packages[member]["name"] for member in metadata["workspace_members"]},
            {
                "jchtools",
                "snap-ocr-core",
                "snap-ocr-worker",
                "jchocr",
                "jchtools-foundation",
                "jchtools-file-core",
                "jchtools-recursive-extract",
                "jchtools-directory-organizer",
                "jchtools-md-tools",
                "jchtools-git-tools",
                "jchtools-xberg",
                "jchtools-assets",
                "jchtools-markdown",
                "jchtools-acp-api",
                "jchtools-codeagent",
                "jchtools-ui",
                "jchtools-gui",
            },
        )
        workspace_members = set(metadata["workspace_members"])
        resolved_local_identities = {
            (package["name"], package["version"])
            for package in packages.values()
            if package["source"] is None and package["id"] not in workspace_members
        }
        self.assertEqual(resolved_local_identities, expected_identities)
        nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
        pending = list(metadata["workspace_members"])
        reachable = set()
        while pending:
            identity = pending.pop()
            if identity in reachable:
                continue
            reachable.add(identity)
            pending.extend(dependency["pkg"] for dependency in nodes[identity]["deps"])
        for entry in patched:
            name, version = entry["name"], entry["version"]
            matches = [
                package for package in packages.values() if package["name"] == name and package["version"] == version
            ]
            self.assertEqual(len(matches), 1, (name, version, matches))
            package = matches[0]
            self.assertIsNone(package["source"], (name, package["source"]))
            self.assertEqual(
                Path(package["manifest_path"]).resolve(),
                (ROOT / entry["effective_source"]["path"] / "Cargo.toml").resolve(),
            )
            self.assertIn(package["id"], reachable, name)
