"""执行 fetch-7zip.ps1 的真实落位阶段；不下载、不调用 MSI、不运行合成引擎。"""

from __future__ import annotations

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

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "scripts" / "fetch-7zip.ps1"

# 与 test_package_windows.py 一样，从实际入口的 AST 执行生产语句。
# 输入代表上游验证、行政解包成功后的边界；不替代或测试这些上游校验。
# 原生 RAR 能力探测独立于文件落位，合成的 7z.exe 不应被执行。
PLACEMENT_DRIVER = r"""
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$tokens = $null
$errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($env:FETCH_SCRIPT, [ref]$tokens, [ref]$errors)
if ($errors.Count) {throw '7-Zip 下载脚本语法无效'}
foreach ($statement in $ast.EndBlock.Statements) {
    if ($statement -is [Management.Automation.Language.FunctionDefinitionAst]) {
        . ([scriptblock]::Create($statement.Extent.Text))
    }
}
$metadata = Get-Content -LiteralPath $env:FETCH_METADATA -Raw -Encoding UTF8 | ConvertFrom-Json
$Version = $metadata.version
$Destination = $env:FETCH_DESTINATION
$staging = $env:FETCH_STAGING
$msi = [pscustomobject]@{
    Path=$env:FETCH_INSTALLER; Name=$metadata.installer.name
    Sha256=$metadata.installer.sha256; Url=$metadata.installer.url
}
$source = [pscustomobject]@{
    Path=$env:FETCH_SOURCE; Name=$metadata.source.name
    Sha256=$metadata.source.sha256; Url=$metadata.source.url
}
function Find-AssignmentIndex($statements, [string]$name) {
    $matches = @()
    for ($index = 0; $index -lt $statements.Count; $index++) {
        $statement = $statements[$index]
        if ($statement -is [Management.Automation.Language.AssignmentStatementAst] -and
            $statement.Left -is [Management.Automation.Language.VariableExpressionAst] -and
            $statement.Left.VariablePath.UserPath -ceq $name) {
            $matches += $index
        }
    }
    if ($matches.Count -ne 1) {throw "无法唯一定位实际落位阶段变量 $name"}
    return $matches[0]
}
$blocks = @($ast.EndBlock.Statements | Where-Object {
    $_ -is [Management.Automation.Language.TryStatementAst]
})
if ($blocks.Count -ne 1) {throw '无法唯一定位实际 MSI 解包与落位块'}
$statements = @($blocks[0].Body.Statements)
$engineStart = Find-AssignmentIndex $statements 'manifestFiles'
$probeStart = Find-AssignmentIndex $statements 'engine'
$materialsStart = Find-AssignmentIndex $statements 'licenseFiles'
if (-not ($engineStart -lt $probeStart -and $probeStart -lt $materialsStart)) {
    throw '实际落位阶段与原生引擎探测边界发生变化'
}
# 保留生产文件选择、摘要计算、复制和清单写入；仅绕过上游命令及原生探测。
for ($index = $engineStart; $index -lt $probeStart; $index++) {
    . ([scriptblock]::Create($statements[$index].Extent.Text))
}
for ($index = $materialsStart; $index -lt $statements.Count; $index++) {
    . ([scriptblock]::Create($statements[$index].Extent.Text))
}
"""


class Fetch7zipPlacementTests(unittest.TestCase):
    """同一已验证输入不能刷新 Cargo 监听资产；不同有效字节必须落位。"""

    @classmethod
    def setUpClass(cls) -> None:
        if os.name != "nt":
            raise RuntimeError("7-Zip 落位回归只在 Windows 上执行")
        cls.powershell = shutil.which("pwsh.exe") or shutil.which("powershell.exe")
        if cls.powershell is None:
            raise RuntimeError("缺少 PowerShell（pwsh.exe 或 powershell.exe）")
        # 独占本次执行目录，所有驱动、元数据、源文件和目标均留在 .tmp 内。
        cls.workspace = ROOT / ".tmp" / f"fetch-7zip-tests-{uuid.uuid4().hex}"
        subprocess.run(
            [sys.executable, str(ROOT / "scripts" / "make_tmp.py"), "workspace", "--destination", str(cls.workspace)],
            cwd=ROOT,
            check=True,
            capture_output=True,
            timeout=30,
        )
        cls.addClassCleanup(cls.workspace.rmdir)
        # 落位边界使用合成的已验证元数据；干净检出还没有被 git 忽略的
        # resources/7zip/manifest.json，不能把先下载真实引擎作为此单测的前置。
        cls.official_metadata = {
            "version": "99.01",
            "upstream": "https://www.7-zip.org/",
            "installer": {
                "name": "7z9901-x64.msi",
                "sha256": hashlib.sha256(b"synthetic already-verified MSI; must never be invoked\n").hexdigest(),
                "url": "https://github.com/ip7z/7zip/releases/download/99.01/7z9901-x64.msi",
            },
            "source": {
                "name": "7z9901-src.tar.xz",
                "sha256": hashlib.sha256(b"synthetic already-verified source archive\n").hexdigest(),
                "url": "https://github.com/ip7z/7zip/releases/download/99.01/7z9901-src.tar.xz",
            },
        }

    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(dir=self.workspace, prefix="case-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.staging = self.root / "administrative image with spaces"
        self.extracted = self.staging / "Files" / "7-Zip"
        self.extracted.mkdir(parents=True)
        self.destination = self.root / "resources" / "7zip"
        self.destination.mkdir(parents=True)
        self.source = self.root / self.official_metadata["source"]["name"]
        self.source.write_bytes(b"synthetic already-verified source archive\n")
        self.installer = self.root / self.official_metadata["installer"]["name"]
        self.installer.write_bytes(b"synthetic already-verified MSI; must never be invoked\n")
        self.metadata_path = self.root / "official-metadata.json"
        self.metadata_path.write_text(json.dumps(self.official_metadata), encoding="utf-8")
        self.driver = self.root / "placement.ps1"
        self.driver.write_text(PLACEMENT_DRIVER, encoding="utf-8-sig")
        self.inputs = {
            "7z.exe": b"synthetic extracted full console engine A\n",
            "7z.dll": b"synthetic extracted full engine library A\n",
            "License.txt": b"synthetic extracted official license material\n",
        }
        # 源和目标使用明确不同的历史时间，不依赖 sleep 或文件系统当前时钟。
        self.source_stamp_ns = 1_010_000_000_000_000_000
        self.target_stamp_ns = 946_684_800_000_000_000
        for name, content in self.inputs.items():
            self.write_extracted(name, content)
        os.utime(self.source, ns=(self.source_stamp_ns, self.source_stamp_ns))

    def write_extracted(self, name: str, content: bytes) -> None:
        path = self.extracted / name
        path.write_bytes(content)
        os.utime(path, ns=(self.source_stamp_ns, self.source_stamp_ns))

    def run_placement(self) -> None:
        environment = os.environ.copy()
        environment.update(
            FETCH_SCRIPT=str(SCRIPT),
            FETCH_METADATA=str(self.metadata_path),
            FETCH_DESTINATION=str(self.destination),
            FETCH_STAGING=str(self.staging),
            FETCH_INSTALLER=str(self.installer),
            FETCH_SOURCE=str(self.source),
        )
        result = subprocess.run(
            [self.powershell, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", str(self.driver)],
            cwd=self.root,
            env=environment,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=30,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stdout + "\n" + result.stderr)

    def manifest(self) -> dict:
        return json.loads((self.destination / "manifest.json").read_text(encoding="utf-8-sig"))

    def assert_manifest_matches_extracted(self) -> None:
        manifest = self.manifest()
        self.assertEqual(manifest["version"], self.official_metadata["version"])
        self.assertEqual(manifest["upstream"], self.official_metadata["upstream"])
        self.assertEqual(manifest["installer"], self.official_metadata["installer"])
        self.assertEqual(manifest["source"], self.official_metadata["source"])
        hashes = {entry["name"]: entry["sha256"] for entry in manifest["files"]}
        self.assertEqual(set(hashes), {"7z.exe", "7z.dll"})
        for name in ("7z.exe", "7z.dll"):
            content = (self.extracted / name).read_bytes()
            self.assertEqual((self.destination / name).read_bytes(), content)
            self.assertEqual(hashes[name], hashlib.sha256(content).hexdigest())

    def freeze_targets(self, paths: list[Path]) -> dict[Path, int]:
        for path in paths:
            os.utime(path, ns=(self.target_stamp_ns, self.target_stamp_ns))
        return {path: path.stat().st_mtime_ns for path in paths}

    def test_identical_verified_inputs_preserve_engine_manifest_and_material_mtimes(self) -> None:
        self.run_placement()
        self.assert_manifest_matches_extracted()
        paths = [
            self.destination / "7z.exe",
            self.destination / "7z.dll",
            self.destination / "manifest.json",
            self.destination / "NOTICE.txt",
            self.destination / self.source.name,
            self.destination / "licenses" / "License.txt",
        ]
        stamps = self.freeze_targets(paths)
        contents = {path: path.read_bytes() for path in paths}
        self.assertNotEqual(stamps[paths[0]], (self.extracted / "7z.exe").stat().st_mtime_ns)
        self.run_placement()
        self.assert_manifest_matches_extracted()
        for path in paths:
            with self.subTest(asset=str(path.relative_to(self.destination))):
                self.assertEqual(path.stat().st_mtime_ns, stamps[path], "相同有效输入不应重写落位资产")
                self.assertEqual(path.read_bytes(), contents[path])

    def test_changed_verified_engine_bytes_update_targets_and_manifest(self) -> None:
        self.run_placement()
        paths = [self.destination / name for name in ("7z.exe", "7z.dll", "manifest.json")]
        stamps = self.freeze_targets(paths)
        previous_manifest = self.manifest()
        # 官方版本、MSI 元数据及目标自写清单均不变；真实解包结果的不同字节不能被缓存标记掩盖。
        for name in ("7z.exe", "7z.dll"):
            self.write_extracted(name, self.inputs[name] + b"different verified extraction bytes B\n")
        self.run_placement()
        self.assert_manifest_matches_extracted()
        self.assertNotEqual(self.manifest()["files"], previous_manifest["files"])
        for path in paths:
            with self.subTest(asset=path.name):
                self.assertNotEqual(path.stat().st_mtime_ns, stamps[path], "不同有效字节必须更新目标与清单")

    def test_stale_manifest_cannot_hide_corrupted_existing_engine_bytes(self) -> None:
        self.run_placement()
        previous_manifest = self.manifest()
        for name in ("7z.exe", "7z.dll"):
            (self.destination / name).write_bytes(b"corrupt existing destination; manifest still claims valid bytes\n")
        paths = [self.destination / name for name in ("7z.exe", "7z.dll", "manifest.json", "NOTICE.txt")]
        stamps = self.freeze_targets(paths)
        self.run_placement()
        self.assert_manifest_matches_extracted()
        self.assertEqual(self.manifest(), previous_manifest)
        for path in paths:
            with self.subTest(asset=path.name):
                if path.name in ("7z.exe", "7z.dll"):
                    self.assertNotEqual(path.stat().st_mtime_ns, stamps[path], "被篡改引擎必须恢复真实有效字节")
                else:
                    self.assertEqual(path.stat().st_mtime_ns, stamps[path], "恢复引擎不应重写未变化的清单或声明")

    def test_changed_verified_license_and_source_bytes_update_materials(self) -> None:
        self.run_placement()
        material_paths = [
            self.destination / "licenses" / "License.txt",
            self.destination / self.source.name,
        ]
        unchanged_paths = [self.destination / name for name in ("7z.exe", "7z.dll", "NOTICE.txt")]
        manifest_path = self.destination / "manifest.json"
        stamps = self.freeze_targets(material_paths + unchanged_paths + [manifest_path])
        license_bytes = self.inputs["License.txt"] + b"different verified official license material\n"
        source_bytes = self.source.read_bytes() + b"different verified upstream source bytes\n"
        self.write_extracted("License.txt", license_bytes)
        self.source.write_bytes(source_bytes)
        os.utime(self.source, ns=(self.source_stamp_ns, self.source_stamp_ns))
        metadata = json.loads(self.metadata_path.read_text(encoding="utf-8"))
        metadata["source"]["sha256"] = hashlib.sha256(source_bytes).hexdigest()
        self.metadata_path.write_text(json.dumps(metadata), encoding="utf-8")
        self.run_placement()
        self.assertEqual(material_paths[0].read_bytes(), license_bytes)
        self.assertEqual(material_paths[1].read_bytes(), source_bytes)
        self.assertEqual(self.manifest()["source"], metadata["source"])
        for path in material_paths + [manifest_path]:
            with self.subTest(asset=str(path.relative_to(self.destination))):
                self.assertNotEqual(path.stat().st_mtime_ns, stamps[path], "不同有效材料或元数据必须更新")
        for path in unchanged_paths:
            with self.subTest(asset=path.name):
                self.assertEqual(path.stat().st_mtime_ns, stamps[path], "材料变更不应重写未变化的引擎或声明")

    def test_corrupted_license_source_manifest_and_notice_are_restored(self) -> None:
        self.run_placement()
        material_paths = [
            self.destination / "licenses" / "License.txt",
            self.destination / self.source.name,
            self.destination / "manifest.json",
            self.destination / "NOTICE.txt",
        ]
        engine_paths = [self.destination / name for name in ("7z.exe", "7z.dll")]
        original_contents = {path: path.read_bytes() for path in material_paths + engine_paths}
        for path in material_paths:
            path.write_bytes(b"corrupt existing destination material\n")
        stamps = self.freeze_targets(material_paths + engine_paths)
        self.run_placement()
        self.assert_manifest_matches_extracted()
        for path in material_paths:
            with self.subTest(asset=str(path.relative_to(self.destination))):
                self.assertEqual(path.read_bytes(), original_contents[path], "被篡改材料必须恢复")
                self.assertNotEqual(path.stat().st_mtime_ns, stamps[path], "被篡改材料必须更新目标")
        for path in engine_paths:
            with self.subTest(asset=path.name):
                self.assertEqual(path.read_bytes(), original_contents[path])
                self.assertEqual(path.stat().st_mtime_ns, stamps[path], "材料恢复不应重写未变化的引擎")


if __name__ == "__main__":
    unittest.main()
