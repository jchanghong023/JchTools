"""覆盖 E-03/E-05：打包读取本次 Cargo 产物及其内嵌状态，不猜测缓存时间。"""

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
from scripts.test_gate import ps51_module_path

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

from scripts.package_windows_tests.cases_delivery import _PackageWindowsDeliveryCases
from scripts.package_windows_tests.cases_script import _PackageWindowsScriptCases


class PackageWindowsArtifactTests(_PackageWindowsScriptCases, _PackageWindowsDeliveryCases, unittest.TestCase):
    """执行部署脚本的真实 artifact 选择和状态文件读取边界。"""

    @classmethod
    def setUpClass(cls) -> None:
        if os.name != "nt":
            raise RuntimeError("打包回归只在 Windows 上执行")
        cls.powershell = shutil.which("powershell.exe") or shutil.which("pwsh.exe")
        if cls.powershell is None:
            raise RuntimeError("缺少 PowerShell（pwsh.exe 或 powershell.exe）")
        cls.workspace = ROOT / ".tmp" / "r24-packaging-tests"
        created_workspace = not cls.workspace.exists()
        if created_workspace:
            cls.addClassCleanup(cls.workspace.rmdir)
        subprocess.run(
            ["python", str(ROOT / "scripts" / "make_tmp.py"), "workspace", "--destination", str(cls.workspace)],
            cwd=ROOT,
            check=True,
            capture_output=True,
            timeout=30,
        )


    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(dir=self.workspace, prefix="case-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.manifest = self.root / "Cargo.toml"
        self.executable = self.root / "构建目录 with spaces" / "x86_64-pc-windows-msvc" / "release" / "JchTools.exe"
        self.out_dir = (
            self.root / "构建目录 with spaces" / "x86_64-pc-windows-msvc" / "release" / "build" / "active" / "out"
        )
        self.out_dir.mkdir(parents=True)
        self.engine_manifest = self.root / "crates" / "file-core" / "Cargo.toml"
        self.records = [
            {
                "reason": "compiler-artifact",
                "package_id": "path+file:///r24#jchtools@0.1.0",
                "manifest_path": str(self.manifest),
                "target": {"name": "JchTools", "kind": ["bin"]},
                "executable": str(self.executable),
            },
            {
                "reason": "compiler-artifact",
                "package_id": "path+file:///r24/crates/file-core#jchtools-file-core@0.1.0",
                "manifest_path": str(self.engine_manifest),
                "target": {"name": "jchtools_file_core", "kind": ["lib"]},
                "executable": None,
            },
            {
                "reason": "build-script-executed",
                "package_id": "path+file:///r24/crates/file-core#jchtools-file-core@0.1.0",
                "out_dir": str(self.out_dir),
            },
        ]


    def powershell_environment(self) -> dict[str, str]:
        environment = os.environ.copy()
        if Path(self.powershell).name.lower() == "powershell.exe":
            # 复用原生 5.1 的模块路径策略，不继承宿主 PowerShell 7 的不兼容模块。
            module_path = ps51_module_path()
            if module_path is not None:
                environment["PSModulePath"] = module_path
        return environment


    def run_selection(self, *, engine: bool = True) -> subprocess.CompletedProcess[str]:
        records_file = self.root / "cargo-records.json"
        records_file.write_text(json.dumps(self.records), encoding="utf-8")
        driver = self.root / "selection.ps1"
        driver.write_text(DRIVER, encoding="utf-8-sig")
        environment = self.powershell_environment()
        environment.update(
            R24_SCRIPT=str(SCRIPT),
            R24_FUNCTIONS="Get-CargoArtifact,Get-CargoLibraryArtifact,Assert-EmbeddedEngine",
            R24_RECORDS=str(records_file),
            R24_BINARY="JchTools",
            R24_MANIFEST=str(self.manifest),
            R24_ENGINE_MANIFEST=str(self.engine_manifest),
            R24_CHECK_ENGINE="1" if engine else "0",
        )
        return subprocess.run(
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


    def installer_compiler(self) -> Path:
        candidates = [
            shutil.which("ISCC.exe"),
            str(Path(os.environ.get("LOCALAPPDATA", "")) / "Programs" / "Inno Setup 6" / "ISCC.exe"),
            str(Path(os.environ.get("ProgramFiles(x86)", "")) / "Inno Setup 6" / "ISCC.exe"),
            str(Path(os.environ.get("ProgramFiles", "")) / "Inno Setup 6" / "ISCC.exe"),
        ]
        for candidate in candidates:
            if candidate and Path(candidate).is_file():
                return Path(candidate)
        self.fail("深路径回归需要真实 Inno Setup 6；不得跳过或伪造编译器")


    @staticmethod
    def extended_path(path: Path) -> Path:
        """Create/read/delete the fixture without depending on Windows long-path policy."""
        absolute = str(path.absolute())
        if absolute.startswith("\\\\"):
            return Path("\\\\?\\UNC\\" + absolute[2:])
        return Path("\\\\?\\" + absolute)


    def installer_source_snapshot(self, source: Path) -> dict:
        extended = self.extended_path(source)
        snapshot = {}
        for directory, directories, files in os.walk(extended):
            for name in directories + files:
                member = Path(directory) / name
                metadata = member.stat()
                snapshot[str(member.relative_to(extended))] = (
                    member.is_dir(),
                    metadata.st_ino,
                    metadata.st_size,
                    metadata.st_mtime_ns,
                    None if member.is_dir() else hashlib.sha256(member.read_bytes()).hexdigest(),
                )
        return snapshot


    def run_installer_compiler(
        self, source: Path, destination: Path, installer_script: Path
    ) -> subprocess.CompletedProcess[str]:
        driver = self.root / "installer-compiler.ps1"
        driver.write_text(INSTALLER_COMPILER_DRIVER, encoding="utf-8-sig")
        environment = self.powershell_environment()
        environment.update(
            R24_SCRIPT=str(SCRIPT),
            R24_FUNCTIONS="Invoke-InstallerCompiler",
            R24_REPOSITORY=str(ROOT),
            R24_ISCC=str(self.installer_compiler()),
            R24_SOURCE_DIRECTORY=str(source),
            R24_DESTINATION=str(destination),
            R24_INSTALLER_SCRIPT=str(installer_script),
        )
        return subprocess.run(
            [self.powershell, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", str(driver)],
            cwd=ROOT,
            env=environment,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=90,
            check=False,
        )


    def run_package_tree(
        self, source: Path, *, destination: Path | None = None
    ) -> subprocess.CompletedProcess[str]:
        driver = self.root / "package-tree.ps1"
        driver.write_text(
            FUNCTION_LOADER
            + r"""
try {
    if ($env:R24_ACTION -eq 'zip') {
        New-PackageZip -Directory $env:R24_SOURCE_DIRECTORY -Destination $env:R24_DESTINATION
    } else {
        Assert-PackageAudioExclusion -Directory $env:R24_SOURCE_DIRECTORY
    }
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 1
}
""",
            encoding="utf-8-sig",
        )
        environment = self.powershell_environment()
        environment.update(
            R24_SCRIPT=str(SCRIPT),
            R24_FUNCTIONS="Assert-PackageAudioExclusion,New-PackageZip",
            R24_SOURCE_DIRECTORY=str(source),
            R24_DESTINATION=str(destination) if destination is not None else "",
            R24_ACTION="zip" if destination is not None else "validate",
        )
        return subprocess.run(
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


    def write_status(self, status: str, *, foreign: bool = False) -> None:
        directory = self.out_dir if not foreign else self.out_dir.parent.parent / "unrelated" / "out"
        directory.mkdir(parents=True, exist_ok=True)
        path = directory / "engine_embed_status.txt"
        path.write_text(status + "\n", encoding="utf-8")
        # 故意让无关缓存更“新”，覆盖旧选择器的两个相反错误。
        stamp = 1790899200 if not foreign else 1790985600
        os.utime(path, (stamp, stamp))


    def run_license_copy(
        self, package_name: str, *, missing: bool = False, destination: Path | None = None
    ) -> tuple[subprocess.CompletedProcess[str], dict, Path]:
        manifest = json.loads((ROOT / "vendor" / "SOURCES.json").read_text(encoding="utf-8"))
        entry = next(package for package in manifest["packages"] if package["name"] == package_name)
        package = {
            "manifest_path": str(ROOT / entry["directory"] / "Cargo.toml"),
            "license_file": entry["license_file"],
        }
        if missing:
            entry["license_and_notice_materials"] = ["LICENSES/nonexistent-license.txt"]
        package_file = self.root / "package.json"
        package_file.write_text(json.dumps(package), encoding="utf-8")
        source_file = self.root / "source.json"
        source_file.write_text(json.dumps(entry), encoding="utf-8")
        if destination is None:
            destination = self.root / "notices"
        driver = self.root / "licenses.ps1"
        driver.write_text(LICENSE_DRIVER, encoding="utf-8-sig")
        environment = self.powershell_environment()
        environment.update(
            R24_SCRIPT=str(SCRIPT),
            R24_FUNCTIONS="Copy-PackageLicenseMaterials",
            R24_PACKAGE=str(package_file),
            R24_SOURCE_PACKAGE=str(source_file),
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
        return result, entry, destination


    def prepare_snap_manifest_fixture(self) -> Path:
        resources = self.root / "resources"
        resources.mkdir()
        shutil.copyfile(ROOT / "resources" / "snap-ocr-assets.json", resources / "snap-ocr-assets.json")
        worker = (
            self.root / "worker 构建目录 with spaces" / "x86_64-pc-windows-msvc" / "release" / "snap-ocr-worker.exe"
        )
        worker.parent.mkdir(parents=True)
        worker.write_bytes(b"MZ\x00synthetic worker executable fixture\x00version-one")
        return worker


    def run_snap_manifest_stage(self, worker: Path, destination: Path, *, fail_main: bool = False) -> dict:
        records_file = self.root / "worker-cargo-records.json"
        records_file.write_text(
            json.dumps(
                [
                    {
                        "reason": "compiler-artifact",
                        "package_id": "path+file:///r24#snap-ocr-worker@0.1.2",
                        "manifest_path": str(self.root / "optional" / "snap-ocr-worker" / "Cargo.toml"),
                        "target": {"name": "snap-ocr-worker", "kind": ["bin"]},
                        "executable": str(worker),
                    }
                ]
            ),
            encoding="utf-8",
        )
        driver = self.root / "snap-manifest-stage.ps1"
        driver.write_text(SNAP_MANIFEST_DRIVER, encoding="utf-8-sig")
        observation = self.root / "snap-manifest-observation.json"
        observation.unlink(missing_ok=True)
        source = self.root / "resources" / "snap-ocr-assets.json"
        source_bytes = source.read_bytes()
        environment = self.powershell_environment()
        timing_pointer = self.root / "timing-pointer.txt"
        restored_env = self.root / "restored-env.txt"
        environment.update(
            R24_SCRIPT=str(SCRIPT),
            R24_ROOT=str(self.root),
            R24_DESTINATION=str(destination),
            R24_RECORDS=str(records_file),
            R24_OBSERVATION=str(observation),
            R24_PREVIOUS_MANIFEST=str(self.root / "caller-manifest-not-used.json"),
            R24_REPOSITORY=str(ROOT),
            R24_TIMING_POINTER=str(timing_pointer),
            R24_RESTORED_ENV=str(restored_env),
            R24_FAIL_MAIN="1" if fail_main else "0",
        )
        result = subprocess.run(
            [self.powershell, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", str(driver)],
            cwd=self.root,
            env=environment,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=30,
            check=False,
        )
        timing_path = Path(timing_pointer.read_text(encoding="utf-8"))
        self.addCleanup(shutil.rmtree, timing_path.parent)
        timing_snapshots = read_timing_snapshots(timing_path)
        timing = timing_snapshots[-1]
        self.assertEqual(restored_env.read_text(encoding="utf-8"), environment["R24_PREVIOUS_MANIFEST"])
        if fail_main:
            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertIn("controlled main build failure", result.stderr)
            return timing
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        self.assertEqual(source.read_bytes(), source_bytes, "生产阶段不得修改 pending-build 源清单")
        snapshot = json.loads(observation.read_text(encoding="utf-8"))
        manifest_bytes = base64.b64decode(snapshot["bytes_base64"], validate=True)
        self.assertEqual(Path(snapshot["path"]).read_bytes(), manifest_bytes)
        stage = destination / "optional-components-v0.1.2"
        self.assertEqual((stage / "snap-ocr-assets.json").read_bytes(), manifest_bytes)
        self.assertEqual((stage / "snap-ocr-worker.exe").read_bytes(), worker.read_bytes())
        manifest = json.loads(manifest_bytes)
        source_manifest = json.loads(source_bytes)
        self.assertEqual(
            {key: value for key, value in manifest.items() if key != "workers"},
            {key: value for key, value in source_manifest.items() if key != "workers"},
        )
        self.assertEqual(len(manifest["workers"]), 1)
        built_worker = manifest["workers"][0]
        self.assertEqual(built_worker["status"], "ok")
        self.assertEqual(built_worker["size_bytes"], worker.stat().st_size)
        self.assertEqual(built_worker["sha256"], hashlib.sha256(worker.read_bytes()).hexdigest())
        self.assertEqual(
            {key: value for key, value in built_worker.items() if key not in {"status", "size_bytes", "sha256"}},
            {
                key: value
                for key, value in source_manifest["workers"][0].items()
                if key not in {"status", "size_bytes", "sha256"}
            },
        )
        snapshot["timing"] = timing
        snapshot["timing_snapshots"] = timing_snapshots
        return snapshot


    def xberg_fixture(self, snapshot_id: str = "fixed") -> dict:
        """Synthetic bytes exercise production archive/policy decisions, not OCR inference."""
        common = {
            name: name.encode()
            for name in (
                "xberg.exe",
                "LICENSE",
                "THIRD_PARTY_LICENSES.md",
                "onnxruntime.dll",
                "onnxruntime_providers_shared.dll",
                "MSVCP140.dll",
                "MSVCP140_1.dll",
                "VCRUNTIME140.dll",
                "VCRUNTIME140_1.dll",
                "models/paddleocr-onnx-models-LICENSE.txt",
                "sherpa-onnx/LICENSE",
                "sherpa-onnx/sherpa-onnx-c-api.dll",
                "sherpa-onnx/sherpa-onnx-cxx-api.dll",
            )
        }
        for name in ("onnxruntime.dll", "onnxruntime_providers_shared.dll"):
            common[f"sherpa-onnx/{name}"] = common[name]
        prefix = f"models/models--xberg-io--paddleocr-onnx-models/snapshots/{snapshot_id}/"
        document = {
            prefix + name: name.encode()
            for name in (
                "v6/det/tiny/model.onnx",
                "v6/rec/tiny/model.onnx",
                "v6/rec/tiny/dict.txt",
                "v2/classifiers/PP-LCNet_x1_0_textline_ori.onnx",
            )
        }
        snapshot = {
            f"models/snapshot-ocr/{name}": ("small-" + name).encode()
            for name in ("det.onnx", "rec.onnx", "dict/dict.txt")
        }
        identity_root = self.root / "identity"
        (identity_root / "resources").mkdir(parents=True)
        (identity_root / "resources" / "markdown-assets.json").write_text(
            json.dumps({"xberg": {"members": [self.xberg_member(p, b) for p, b in (document | snapshot).items()]}}),
            encoding="utf-8",
        )
        components = []
        for name, files in (("document", common | document), ("snapshot", common | snapshot)):
            archive = self.root / f"xberg-{name}-x86_64-pc-windows-msvc.zip"
            with zipfile.ZipFile(archive, "w") as output:
                for path, payload in files.items():
                    output.writestr(f"xberg-cli-x86_64-pc-windows-msvc/{path}", payload)
            components.append(
                {
                    "id": name,
                    "archive": archive.name,
                    "requires": [],
                    "size_bytes": archive.stat().st_size,
                    "sha256": hashlib.sha256(archive.read_bytes()).hexdigest(),
                    "members": [self.xberg_member(p, b) for p, b in files.items()],
                }
            )
        release_url = "https://github.com/jchanghong023/xberg/releases/download/v-test/"
        audio = {
            "id": "audio-assets",
            "archive": "xberg-audio-assets-x86_64-pc-windows-msvc.zip",
            "requires": [],
            "size_bytes": 123,
            "sha256": "a" * 64,
            "members": [self.xberg_member("ffmpeg/LICENSE.txt", b"license")],
        }
        receipt = {
            "owner": "JchTools-bundled-xberg-v1",
            "tag": "v-test",
            "source": "https://api.github.com/repos/jchanghong023/xberg/releases/latest",
            "manifest": {
                "archive": "xberg-components-x86_64-pc-windows-msvc.json",
                "url": release_url + "xberg-components-x86_64-pc-windows-msvc.json",
                "size_bytes": 123,
                "sha256": "b" * 64,
            },
            "components": components,
            "audio_assets": audio | {"url": release_url + audio["archive"]},
        }
        return {"receipt": receipt, "root": identity_root}


    @staticmethod
    def xberg_member(path: str, payload: bytes) -> dict:
        return {"path": path, "size_bytes": len(payload), "sha256": hashlib.sha256(payload).hexdigest()}


    def run_xberg_policy(
        self, fixture: dict, action: str = "expand", *, destination: Path | None = None
    ) -> subprocess.CompletedProcess:
        record = self.root / "xberg-receipt.json"
        record.write_text(json.dumps(fixture["receipt"]), encoding="utf-8")
        driver = self.root / "xberg-policy.ps1"
        driver.write_text(
            FUNCTION_LOADER
            + r"""
Add-Type -AssemblyName System.IO.Compression.FileSystem
$receipt = Get-Content -LiteralPath $env:R24_RECORDS -Raw -Encoding UTF8 | ConvertFrom-Json
$stage = Join-Path $env:R24_DESTINATION 'staged'
switch ($env:R24_ACTION) {
    'api' {
        $release = $receipt
        Get-XbergReleaseAsset $release 'xberg-document-x86_64-pc-windows-msvc.zip' | Out-Null
    }
    'cache' {Get-XbergCachedAsset $receipt $env:R24_DESTINATION -OfflineMode | Out-Null}
    'validate' {Assert-XbergPackage $stage $env:R24_ROOT}
    'install' {Install-XbergBaseFiles $stage $env:R24_INSTALL_DESTINATION $env:R24_ROOT}
    default {
        $union = Assert-XbergBaseMembers @($receipt.components) $env:R24_ROOT
        New-Item -ItemType Directory -Path $stage -Force | Out-Null
        foreach ($component in $receipt.components) {
            Expand-XbergComponent (Join-Path $env:R24_DESTINATION $component.archive) $component $stage
        }
        Copy-Item -LiteralPath $env:R24_RECORDS -Destination (Join-Path $stage 'jchtools-xberg-release.json') -Force
        Assert-XbergPackage $stage $env:R24_ROOT
    }
}
""",
            encoding="utf-8-sig",
        )
        environment = self.powershell_environment()
        environment.update(
            R24_SCRIPT=str(ROOT / "scripts" / "prepare-xberg-runtime.ps1"),
            R24_FUNCTIONS="Assert-XbergRelativePath,Assert-XbergFile,"
            "Get-XbergReleaseAsset,Get-XbergCachedAsset,"
            "Get-XbergMemberMap,Test-XbergAudioPath,Assert-XbergBaseMembers,Expand-XbergComponent,"
            "Assert-XbergPackage,Install-XbergBaseFiles",
            R24_RECORDS=str(record),
            R24_DESTINATION=str(self.root),
            R24_INSTALL_DESTINATION=str(destination if destination is not None else self.root / "installed"),
            R24_ROOT=str(fixture["root"]),
            R24_ACTION=action,
        )
        return subprocess.run(
            [self.powershell, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", str(driver)],
            cwd=ROOT,
            env=environment,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
        )


    def rewrite_xberg_zip(self, fixture: dict, members: list[tuple[str, bytes | zipfile.ZipInfo]]) -> None:
        component = fixture["receipt"]["components"][0]
        archive = self.root / component["archive"]
        with zipfile.ZipFile(archive, "w") as output:
            for path, payload in members:
                if isinstance(payload, zipfile.ZipInfo):
                    output.writestr(payload, b"link-target")
                else:
                    output.writestr(path, payload)
        component["size_bytes"] = archive.stat().st_size
        component["sha256"] = hashlib.sha256(archive.read_bytes()).hexdigest()

if __name__ == "__main__":
    unittest.main()
