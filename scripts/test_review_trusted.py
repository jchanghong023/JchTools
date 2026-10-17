"""R21 可信基回归：仅合成目录和真实解析行为，不启动桌面、引擎或 Cargo。"""

from __future__ import annotations

import ast
import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

from scripts import make_tmp, requirement_coverage

ROOT = Path(__file__).resolve().parent.parent


class TrustedWorkspaceTests(unittest.TestCase):
    def setUp(self) -> None:
        workspace = ROOT / ".tmp" / "review-trusted"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(make_tmp.main(), 0)
        temporary = tempfile.TemporaryDirectory(dir=workspace)
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)


class ModalGatingBehaviorTests(unittest.TestCase):
    def test_permanently_disabled_controls_are_safe_without_weakening_live_controls(self) -> None:
        # 只加载真实检查函数，避免脚本模块末尾的全仓检查和 sys.exit 导入副作用。
        checker_path = ROOT / "scripts" / "static_check.py"
        tree = ast.parse(checker_path.read_text(encoding="utf-8"))
        function = next(
            node for node in tree.body
            if isinstance(node, ast.FunctionDef) and node.name == "slint_modal_gating"
        )
        namespace = {"ROOT": ROOT, "re": re, "read_text": lambda path: source}
        exec(compile(ast.Module(body=[function], type_ignores=[]), str(checker_path), "exec"), namespace)
        expressions = {
            "false": True,
            "true": False,
            "root.busy": False,
            "false || root.busy": False,
            "!root.busy && root.confirm-kind == 0": True,
        }
        observed = {}
        for expression in expressions:
            source = (
                "export component AppWindow inherits Window {\n"
                f"    Button {{ enabled: {expression}; }}\n"
                "    if root.confirm-kind != 0: Rectangle {}\n"
                "}\n"
            )
            try:
                namespace["slint_modal_gating"]()
            except AssertionError:
                observed[expression] = False
            else:
                observed[expression] = True
        self.assertEqual(observed, expressions)


class DestinationBoundaryTests(TrustedWorkspaceTests):
    def assert_rejected(self, destination: Path) -> None:
        repository = self.directory / "fake-repository"
        (repository / "scripts").mkdir(parents=True, exist_ok=True)
        with (
            patch.object(make_tmp, "__file__", str(repository / "scripts" / "make_tmp.py")),
            patch.object(sys, "argv", ["make_tmp.py", "testdata", "--destination", str(destination), "--force"]),
            patch.object(make_tmp, "seven_zip", return_value=Path("not-executed.exe")) as engine,
            patch.object(make_tmp, "_clear_existing_target", side_effect=AssertionError("不应到达破坏性清空")),
        ):
            with self.assertRaisesRegex(SystemExit, r"\.tmp"):
                make_tmp.main()
            engine.assert_not_called()

    def test_external_destination_rejected_before_clear(self) -> None:
        external = self.directory / "synthetic-external"
        external.mkdir()
        sentinel = external / "sentinel.txt"
        sentinel.write_bytes(b"must survive")
        self.assert_rejected(external)
        self.assertEqual(sentinel.read_bytes(), b"must survive")
        self.assertEqual(sorted(path.name for path in external.iterdir()), ["sentinel.txt"])

    def test_junction_external_destination_rejected_before_clear(self) -> None:
        external = self.directory / "synthetic-external"
        external.mkdir()
        sentinel = external / "sentinel.txt"
        sentinel.write_bytes(b"must survive junction")
        temp = self.directory / "fake-repository" / ".tmp"
        temp.mkdir(parents=True)
        junction = temp / "linked-data"
        result = subprocess.run(
            ["cmd", "/d", "/c", "mklink", "/J", str(junction), str(external)],
            capture_output=True,
            check=False,
            timeout=20,
        )
        self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))
        self.addCleanup(lambda: junction.rmdir() if junction.exists() else None)
        self.assert_rejected(junction)
        self.assertEqual(sentinel.read_bytes(), b"must survive junction")

    def test_junction_inside_tmp_and_ancestor_are_rejected_before_clear(self) -> None:
        temp = self.directory / "fake-repository" / ".tmp"
        target = temp / "protected-target"
        child = target / "child"
        child.mkdir(parents=True)
        sentinel = child / "sentinel.txt"
        sentinel.write_bytes(b"must survive internal junction")
        junction = temp / "linked-data"
        result = subprocess.run(
            ["cmd", "/d", "/c", "mklink", "/J", str(junction), str(target)],
            capture_output=True,
            check=False,
            timeout=20,
        )
        self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))
        self.addCleanup(lambda: junction.rmdir() if junction.exists() else None)
        for destination in (junction, junction / "child", junction / "not-created"):
            with self.subTest(destination=destination):
                self.assert_rejected(destination)
                self.assertEqual(sentinel.read_bytes(), b"must survive internal junction")
                self.assertEqual(sorted(path.name for path in child.iterdir()), ["sentinel.txt"])

    def test_valid_destination_reaches_existing_clear_boundary(self) -> None:
        repository = self.directory / "fake-repository"
        scripts = repository / "scripts"
        scripts.mkdir(parents=True)
        destination = repository / ".tmp" / "data"
        with (
            patch.object(make_tmp, "__file__", str(scripts / "make_tmp.py")),
            patch.object(sys, "argv", ["make_tmp.py", "testdata", "--destination", str(destination), "--force"]),
            patch.object(make_tmp, "seven_zip", return_value=Path("not-executed.exe")),
            patch.object(
                make_tmp, "_clear_existing_target", side_effect=AssertionError("合法目的地到达清空边界")
            ) as clear,
        ):
            with self.assertRaisesRegex(AssertionError, "合法目的地"):
                make_tmp.main()
            clear.assert_called_once_with(destination.resolve(), force=True)


class OcrCompareAssetTests(TrustedWorkspaceTests):
    def test_missing_or_mutated_rec_dictionary_preserves_existing_bundle(self) -> None:
        repository = self.directory / "fake-repository"
        models = repository / "synthetic-models"
        det = models / "PP-OCRv6_small_det" / "inference.onnx"
        rec = models / "PP-OCRv6_small_rec" / "inference.onnx"
        dictionary = rec.with_name("inference.yml")
        for path, data in ((det, b"det"), (rec, b"rec")):
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
        font = repository / "font.otf"
        font.write_bytes(b"font")
        bundle = repository / ".tmp" / "ocr-compare-bundle"
        bundle.mkdir(parents=True)
        sentinel = bundle / "sentinel.txt"
        sentinel.write_bytes(b"preserve prior bundle")
        digests = {
            b"det": "3914f972d833af87d23bb2338bd09238f978a48f3c4dbb8e1a4ee26a93869940",
            b"rec": "3e3def686ac9a1676b59bc9749ad896263d8f68b53f352060774de359a2e23ed",
            b"font": "ec04cc376b34887cedbdf84074e2e226ed2761eeabdcb9173fc1dd7bfd153ef7",
        }
        for state in ("missing", "mutated"):
            with self.subTest(state=state):
                if state == "mutated":
                    dictionary.write_bytes(b"x" * 150579)
                with (
                    patch.object(
                        make_tmp.hashlib,
                        "sha256",
                        side_effect=lambda data: Mock(hexdigest=lambda: digests.get(data, "0" * 64)),
                    ),
                    patch.object(
                        make_tmp,
                        "_clear_existing_target",
                        side_effect=AssertionError("未校验字典不得清空既有 bundle"),
                    ) as clear,
                    patch.object(make_tmp.shutil, "copytree") as copytree,
                ):
                    with self.assertRaisesRegex(SystemExit, r"inference\.yml"):
                        make_tmp.build_ocr_compare_bundle(repository, str(models), str(font), force=True)
                    clear.assert_not_called()
                    copytree.assert_not_called()
                self.assertEqual(sentinel.read_bytes(), b"preserve prior bundle")
                self.assertEqual(sorted(path.name for path in bundle.iterdir()), ["sentinel.txt"])


class AttributeBaselineTests(TrustedWorkspaceTests):
    # static_check 自 10071d43 起按自身 ROOT 硬性要求登记清单全部在场；合成目录
    # 必须满足同一合同（含各 build producer），检查本身不为此放宽。
    _PRODUCTION_CRATES = (
        "foundation",
        "file-core",
        "recursive-extract",
        "directory-organizer",
        "md-tools",
        "git-tools",
        "xberg",
        "assets",
        "markdown",
        "acp-api",
        "codeagent",
        "jchocr",
        "ui",
        "gui",
    )
    _OPTIONAL_CRATES = ("snap-ocr-core", "snap-ocr-worker")
    _BUILD_PRODUCERS = (
        "",  # 根 build.rs
        "crates/file-core",
        "crates/assets",
        "crates/ui",
        "optional/snap-ocr-worker",
    )

    def _materialize_manifest_contract(self) -> None:
        (self.directory / "Cargo.toml").write_bytes(b"")
        (self.directory / "src").mkdir(exist_ok=True)
        for parent, names in (
            (self.directory / "crates", self._PRODUCTION_CRATES),
            (self.directory / "optional", self._OPTIONAL_CRATES),
        ):
            for crate in names:
                package = parent / crate
                (package / "src").mkdir(parents=True, exist_ok=True)
                (package / "Cargo.toml").write_bytes(b"")
                (package / "src" / "lib.rs").write_bytes(b"")
        for producer in self._BUILD_PRODUCERS:
            (self.directory / producer / "build.rs").write_bytes(b"")

    def collect(self, source: str) -> dict[str, object]:
        scripts = self.directory / "scripts"
        scripts.mkdir(exist_ok=True)
        shutil.copyfile(ROOT / "scripts" / "static_check.py", scripts / "static_check.py")
        tests = self.directory / "tests"
        tests.mkdir(exist_ok=True)
        (tests / "sample.rs").write_text(source, encoding="utf-8")
        self._materialize_manifest_contract()
        result = subprocess.run(
            [sys.executable, str(scripts / "static_check.py"), "--update-test-baseline"],
            capture_output=True,
            check=False,
            timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))
        rows = json.loads((scripts / "test-baseline.json").read_text(encoding="utf-8"))["tests"]
        self.assertEqual(len(rows), 1)
        return rows[0]

    def test_ignore_across_comment_changes_baseline(self) -> None:
        baseline = self.collect("#[test]\nfn sample() { assert!(false); }\n")
        changed = self.collect("#[ignore]\n// 忽略原因\n#[test]\nfn sample() { assert!(false); }\n")
        self.assertFalse(baseline["ignored"])
        self.assertTrue(changed["ignored"])
        self.assertEqual(changed["body"], baseline["body"])

    def test_cfg_across_nested_comment_changes_baseline(self) -> None:
        baseline = self.collect("#[test]\nfn sample() { assert!(false); }\n")
        changed = self.collect("#[cfg(any())]\n/* 外层 /* 内层 */ 继续 */\n#[test]\nfn sample() { assert!(false); }\n")
        self.assertIsNone(baseline["cfg"])
        self.assertEqual(changed["cfg"], "any()")
        self.assertEqual(changed["body"], baseline["body"])

    def test_comment_alone_does_not_change_test_gates(self) -> None:
        changed = self.collect("// #[ignore] 只是注释\n#[test]\nfn sample() { assert!(false); }\n")
        self.assertFalse(changed["ignored"])
        self.assertIsNone(changed["cfg"])

    def test_test_attribute_in_raw_string_is_not_discovered(self) -> None:
        changed = self.collect(
            'const EXAMPLE: &str = r#"#[test]\nfn fake() { assert!(false); }"#;\n'
            "#[test]\nfn sample() { assert!(true); }\n"
        )
        self.assertEqual(changed["name"], "sample")
        self.assertIsNone(changed["cfg"])

    def test_module_cfg_survives_literal_braces_and_stacked_attributes(self) -> None:
        changed = self.collect(
            '#[cfg(all(windows, any(test, feature = "test-hooks")))]\n'
            '#[allow(dead_code)]\nmod gated {\nconst BRACE: &str = "}";\n'
            "#[test]\nfn sample() { assert!(true); }\n}\n"
        )
        self.assertEqual(changed["cfg"], 'all(windows, any(test, feature = "test-hooks"))')

    def test_nested_comment_braces_do_not_hide_assertion_changes(self) -> None:
        source = "#[test]\nfn sample() { /* outer /* inner */ } */ assert!(%s); }\n"
        baseline = self.collect(source % "true")
        changed = self.collect(source % "false")
        self.assertNotEqual(changed["body"], baseline["body"])

    def test_literal_file_cfg_is_not_a_gate(self) -> None:
        changed = self.collect('const EXAMPLE: &str = r#"#![cfg(any())]"#;\n#[test]\nfn sample() { assert!(true); }\n')
        self.assertIsNone(changed["cfg"])

    def _assert_review_cfg_identity(self, before: str, after: str, cfg: str | None) -> None:
        # 覆盖 P-12/P-13：加载真实声明而非导入脚本的全量门执行段，不改写解析器。
        # 合成基线是显式预期数据；不调用生成器、不接触仓库基线。
        checker = ROOT / "scripts" / "static_check.py"
        tree = ast.parse(checker.read_text(encoding="utf-8"), filename=str(checker))
        declarations = []
        for node in tree.body:
            if isinstance(node, ast.If) and isinstance(node.test, ast.Name) and node.test.id == "UPDATE_BASELINE":
                break
            if isinstance(
                node, (ast.Import, ast.ImportFrom, ast.FunctionDef, ast.ClassDef, ast.Assign, ast.AnnAssign, ast.Try)
            ):
                declarations.append(node)
        namespace = {"__file__": str(checker), "__name__": "review_cfg_identity"}
        exec(compile(ast.Module(body=declarations, type_ignores=[]), str(checker), "exec"), namespace)
        namespace["ROOT"] = self.directory
        self._materialize_manifest_contract()
        tests = self.directory / "tests"
        tests.mkdir(exist_ok=True)
        source = tests / "sample.rs"
        scripts = self.directory / "scripts"
        scripts.mkdir(exist_ok=True)
        expected = [
            {
                "file": "tests/sample.rs",
                "name": "sample",
                "cfg": cfg,
                "ignored": False,
                "body": hashlib.sha256(b"{ assert!(true); }").hexdigest()[:16],
            }
        ]
        baseline = scripts / "test-baseline.json"
        baseline.write_text(json.dumps({"tests": expected}), encoding="utf-8")
        baseline_bytes = baseline.read_bytes()
        collect_tests = namespace["collect_tests"]
        test_baseline = namespace["test_baseline"]
        for version, text in (("before", before), ("after", after)):
            with self.subTest(version=version):
                source.write_text(text, encoding="utf-8")
                collect_tests.cache_clear()
                rows = collect_tests()
                self.assertEqual(
                    [(row["file"], row["name"], row["cfg"]) for row in rows],
                    [("tests/sample.rs", "sample", cfg)],
                )
                self.assertEqual(rows, expected)
                self.assertEqual(
                    test_baseline(),
                    "1 个测试与 scripts/test-baseline.json 完全一致（含 ignore、平台门禁与测试体哈希）。",
                )
                self.assertEqual(baseline.read_bytes(), baseline_bytes)

    def test_review_fix_module_cfg_attr_keeps_ungated_baseline_identity(self) -> None:
        sample = "#[test]\nfn sample() { assert!(true); }\n"
        before = "mod tests {\n" + sample + "}\n"
        for predicate in ("test", "not(test)"):
            with self.subTest(predicate=predicate):
                after = f"#[cfg_attr({predicate}, rustfmt::skip)]\n" + before
                self._assert_review_cfg_identity(before, after, None)

    def test_review_fix_nested_cfg_attr_retains_real_gate_identity(self) -> None:
        sample = "#[test]\nfn sample() { assert!(true); }\n"
        before = '#[cfg(windows)]\nmod outer {\n#[cfg(feature = "test-hooks")]\nmod inner {\n' + sample + "}\n}\n"
        after = (
            "#[cfg(windows)]\n#[cfg_attr(test, rustfmt::skip)]\nmod outer {\n"
            '#[cfg_attr(not(test), allow(dead_code))]\n#[cfg(feature = "test-hooks")]\n'
            "mod inner {\n" + sample + "}\n}\n"
        )
        self._assert_review_cfg_identity(before, after, 'windows && feature = "test-hooks"')

    def test_review_fix_stacked_module_cfg_attr_normalizes_test_gate_identity(self) -> None:
        sample = "#[test]\nfn sample() { assert!(true); }\n"
        before = (
            '#[cfg(all(windows, feature = "test-hooks"))]\n'
            "#[cfg(test)]\n#[allow(dead_code)]\nmod tests {\n" + sample + "}\n"
        )
        after = (
            "#[cfg_attr(test, rustfmt::skip)]\n"
            '#[cfg(all(windows, feature = "test-hooks"))]\n'
            "#[allow(dead_code)]\n#[cfg_attr(test, allow(unused_imports))]\n"
            "#[cfg(test)]\nmod tests {\n" + sample + "}\n"
        )
        self._assert_review_cfg_identity(before, after, 'all(windows, feature = "test-hooks")')

    def test_review_fix_file_cfg_attr_retains_real_gate_identity(self) -> None:
        sample = "#[test]\nfn sample() { assert!(true); }\n"
        for cfg, prefix in ((None, "#![cfg(test)]\n"), ("windows", "#![cfg(windows)]\n#![cfg(test)]\n")):
            with self.subTest(cfg=cfg):
                before = prefix + sample
                after = (
                    "#![cfg_attr(test, rustfmt::skip)]\n"
                    + prefix
                    + "#![cfg_attr(not(test), allow(dead_code))]\n"
                    + sample
                )
                self._assert_review_cfg_identity(before, after, cfg)

    def test_review_fix_stacked_test_cfg_attr_keeps_baseline_identity(self) -> None:
        before = "#[cfg(windows)]\n#[cfg(test)]\n#[test]\nfn sample() { assert!(true); }\n"
        after = (
            "#[cfg_attr(test, rustfmt::skip)]\n#[cfg(windows)]\n#[cfg(test)]\n"
            "#[test]\n#[cfg_attr(not(test), allow(dead_code))]\n"
            "fn sample() { assert!(true); }\n"
        )
        self._assert_review_cfg_identity(before, after, "windows")


class RequirementInventoryTests(TrustedWorkspaceTests):
    def test_ah_ca_authority_and_references_remain_unverified(self) -> None:
        requirements = self.directory / "docs" / "requirements"
        requirements.mkdir(parents=True)
        expected = {f"AH-{number:02}" for number in range(1, 17)} | {f"CA-{number:02}" for number in range(1, 8)}
        (requirements / "authority.md").write_text(
            "\n".join(f"- **{identifier}** 合成行为" for identifier in sorted(expected)), encoding="utf-8"
        )
        tests = self.directory / "tests"
        tests.mkdir()
        (tests / "sample.rs").write_text("// 覆盖 AH-01～AH-16 / CA-01～CA-07\n", encoding="utf-8")
        report = requirement_coverage.build_report(self.directory)
        self.assertEqual({item["id"] for item in report["items"]}, expected)
        self.assertEqual(report["summary"], {"total": 23, "without_reference": 0, "verified": 0, "unverified": 23})
        self.assertTrue(all(item["status"] == "UNVERIFIED" for item in report["items"]))

    def test_all_package_test_locations_supply_candidate_references(self) -> None:
        requirements = self.directory / "docs" / "requirements"
        requirements.mkdir(parents=True)
        paths = (
            "build.rs",
            "examples/sample.rs",
            "benches/sample.rs",
            "crates/sample/build.rs",
            "crates/sample/examples/sample.rs",
            "crates/sample/benches/sample.rs",
            "optional/sample/build.rs",
            "optional/sample/tests/sample.rs",
            "optional/sample/examples/sample.rs",
            "optional/sample/benches/sample.rs",
        )
        expected = {f"P-{number:02}" for number in range(1, len(paths) + 1)}
        (requirements / "authority.md").write_text(
            "\n".join(f"- **{identifier}** 合成行为" for identifier in sorted(expected)), encoding="utf-8"
        )
        for number, relative in enumerate(paths, 1):
            source = self.directory / relative
            source.parent.mkdir(parents=True, exist_ok=True)
            source.write_text(f"// 覆盖 P-{number:02}\n", encoding="utf-8")
        report = requirement_coverage.build_report(self.directory)
        self.assertEqual(report["summary"]["without_reference"], 0)
        self.assertTrue(all(item["status"] == "UNVERIFIED" for item in report["items"]))

    def test_delivery_sources_supply_exact_candidate_references(self) -> None:
        requirements = self.directory / "docs" / "requirements"
        requirements.mkdir(parents=True)
        identifiers = tuple(
            f"{prefix}-{number:02}"
            for prefix, number in (("P", 5), ("E", 4), ("U", 8), ("O", 31), ("P", 1), ("O", 32))
        )
        package, environment, ui, delivery, behavior, measurement = identifiers
        (requirements / "authority.md").write_text(
            "\n".join(f"- **{identifier}** 合成交付要求" for identifier in identifiers),
            encoding="utf-8",
        )
        sources = {
            "scripts/release.ps1": f"# {package}\nWrite-Output 'synthetic'\n# {environment}\n",
            "scripts/release.cmd": f"rem {environment}\n@echo synthetic\nrem {ui}\n",
            ".github/workflows/release.yml": f"# {ui}\nname: synthetic\n# {delivery}\n",
            ".github/workflows/check.yaml": f"# {delivery}\nname: synthetic\n# {behavior}\n",
            "installer/setup.iss": f"; {behavior}\n[Setup]\n; {package}\n",
        }
        for relative, content in sources.items():
            source = self.directory / relative
            source.parent.mkdir(parents=True, exist_ok=True)
            source.write_text(content, encoding="utf-8-sig")
        excluded = (
            "vendor/scripts/release.ps1",
            "vendor/.github/workflows/release.yml",
            "vendor/installer/setup.iss",
            "docs/release.ps1",
            "docs/workflows/release.yaml",
            "docs/setup.iss",
            ".tmp/scripts/release.cmd",
            ".tmp/.github/workflows/release.yml",
            ".tmp/installer/setup.iss",
            "scripts/generated.tmp",
            ".github/workflows/generated.tmp",
            "installer/generated.tmp",
        )
        for relative in excluded:
            source = self.directory / relative
            source.parent.mkdir(parents=True, exist_ok=True)
            source.write_text(f"# {measurement} / {package}\n", encoding="utf-8-sig")
        expected = {
            package: [("installer/setup.iss", 3), ("scripts/release.ps1", 1)],
            environment: [("scripts/release.cmd", 1), ("scripts/release.ps1", 3)],
            ui: [(".github/workflows/release.yml", 1), ("scripts/release.cmd", 3)],
            delivery: [(".github/workflows/check.yaml", 1), (".github/workflows/release.yml", 3)],
            behavior: [(".github/workflows/check.yaml", 3), ("installer/setup.iss", 1)],
            measurement: [],
        }
        report = requirement_coverage.build_report(self.directory)
        observed = {
            item["id"]: sorted(
                (Path(reference["file"]).as_posix(), reference["line"])
                for reference in item["candidate_references"]
            )
            for item in report["items"]
        }
        self.assertEqual(observed, expected)
        self.assertEqual(report["summary"], {"total": 6, "without_reference": 1, "verified": 0, "unverified": 6})
        self.assertTrue(all(item["status"] == "UNVERIFIED" for item in report["items"]))

    def test_utf8_bom_authority_preserves_first_requirement_and_line(self) -> None:
        requirements = self.directory / "docs" / "requirements"
        requirements.mkdir(parents=True)
        package, measurement = (f"{prefix}-{number:02}" for prefix, number in (("P", 5), ("O", 32)))
        (requirements / "authority.md").write_text(
            f"- **{package}** 合成交付要求\n\n- **{measurement}** 合成测量要求\n",
            encoding="utf-8-sig",
        )
        report = requirement_coverage.build_report(self.directory)
        self.assertEqual(
            [(item["id"], item["document"], item["line"]) for item in report["items"]],
            [(package, "authority.md", 1), (measurement, "authority.md", 3)],
        )
        self.assertEqual(report["summary"], {"total": 2, "without_reference": 2, "verified": 0, "unverified": 2})

    def test_verification_kinds_preserve_unverified_status_and_reference_summary(self) -> None:
        expected = {
            f"{prefix}-{number:02}": kind
            for (prefix, number), kind in {
                ("H", 8): "NON_GOAL",
                ("U", 4): "NON_GOAL",
                ("P", 14): "ARCHITECTURE",
                ("O", 2): "ARCHITECTURE",
                ("P", 5): "DELIVERY",
                ("E", 4): "DELIVERY",
                ("U", 8): "DELIVERY",
                ("O", 31): "DELIVERY",
                ("O", 4): "MEASUREMENT",
                ("O", 32): "MEASUREMENT",
                ("XB", 13): "COMPARISON",
                ("T", 30): "BEHAVIOR",
                ("P", 1): "BEHAVIOR",
                ("P", 15): "BEHAVIOR",
                ("O", 8): "BEHAVIOR",
                ("XB", 4): "BEHAVIOR",
                ("AH", 1): "BEHAVIOR",
                ("CA", 1): "BEHAVIOR",
                ("P", 99): "BEHAVIOR",
            }.items()
        }
        requirements = self.directory / "docs" / "requirements"
        requirements.mkdir(parents=True)
        (requirements / "authority.md").write_text(
            "\n".join(f"- **{identifier}** 合成要求" for identifier in expected),
            encoding="utf-8",
        )
        scripts = self.directory / "scripts"
        scripts.mkdir()
        delivery_ids = {
            f"{prefix}-{number:02}" for prefix, number in (("P", 5), ("E", 4), ("U", 8), ("O", 31))
        }
        (scripts / "deployment.py").write_text(
            "# " + " / ".join(sorted(delivery_ids)) + "\n",
            encoding="utf-8",
        )
        report = requirement_coverage.build_report(self.directory)
        items = {item["id"]: item for item in report["items"]}
        self.assertEqual(set(items), set(expected))
        self.assertEqual(
            report["summary"],
            {"total": 19, "without_reference": 15, "verified": 0, "unverified": 19},
        )
        for identifier, kind in expected.items():
            with self.subTest(identifier=identifier):
                item = items[identifier]
                self.assertEqual(item["verification_kind"], kind)
                self.assertEqual(item["status"], "UNVERIFIED")
                self.assertEqual(
                    item["candidate_references"],
                    [{"file": str(Path("scripts") / "deployment.py"), "line": 1}]
                    if identifier in delivery_ids else [],
                )


class TaskPollingTests(unittest.TestCase):
    def test_same_second_new_task_does_not_require_increasing_random_uuid(self) -> None:
        # 覆盖 P-11/P-13：任务名的秒后缀是随机 UUID，不是递增任务代次。
        # 加载真实轮询声明，既不导入 UIA，也不启动 GUI、引擎或 Cargo。
        driver = ROOT / "scripts" / "gui_smoke.py"
        tree = ast.parse(driver.read_text(encoding="utf-8"), filename=str(driver))
        definition = next(
            node for node in tree.body if isinstance(node, ast.FunctionDef) and node.name == "wait_task_status"
        )
        clock = Mock()
        clock.time.side_effect = [0.0, 0.0, 2.0]
        previous = "20261015T010203-ffffffff-ffff-4fff-bfff-ffffffffffff"
        current = "20261015T010203-00000000-0000-4000-8000-000000000000"
        records = Mock(return_value=[(current, "ready"), (previous, "finished")])
        namespace = {"time": clock, "_task_records": records, "COMPLETION_TIMEOUT": 240}
        exec(compile(ast.Module(body=[definition], type_ignores=[]), str(driver), "exec"), namespace)
        self.assertIsNone(namespace["wait_task_status"]("synthetic-data", "ready", frozenset({previous}), timeout=1))
        records.assert_called_once_with("synthetic-data")
        clock.sleep.assert_not_called()

    def test_all_prelaunch_tasks_are_excluded_not_only_the_largest_id(self) -> None:
        driver = ROOT / "scripts" / "gui_smoke.py"
        tree = ast.parse(driver.read_text(encoding="utf-8"), filename=str(driver))
        definitions = [
            node for node in tree.body
            if isinstance(node, ast.FunctionDef) and node.name in ("task_ids", "wait_task_status")
        ]
        older = "20261015T010203-11111111"
        previous = "20261015T010203-ffffffff"
        records = Mock(return_value=[(older, "ready"), (previous, "finished")])
        clock = Mock()
        clock.time.side_effect = [0.0, 0.0, 2.0]
        namespace = {"time": clock, "_task_records": records, "COMPLETION_TIMEOUT": 240}
        exec(compile(ast.Module(body=definitions, type_ignores=[]), str(driver), "exec"), namespace)
        snapshot = namespace["task_ids"]("synthetic-data")
        self.assertEqual(snapshot, frozenset({older, previous}))
        with self.assertRaisesRegex(RuntimeError, "超时"):
            namespace["wait_task_status"]("synthetic-data", "ready", snapshot, timeout=1)


class AcceptanceWorkspaceTests(TrustedWorkspaceTests):
    def test_repeat_initialization_uses_fresh_assets_and_preserves_prior_run(self) -> None:
        # 执行真实入口的初始化 AST，到第一个验收阶段为止；不启动测试门或桌面。
        scripts = self.directory / "scripts"
        scripts.mkdir()
        (scripts / "make_tmp.py").write_bytes((ROOT / "scripts" / "make_tmp.py").read_bytes())
        harness = r"""
[Console]::InputEncoding = [Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
$payload = [Console]::In.ReadToEnd() | ConvertFrom-Json
$tokens = $null; $errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($payload.path, [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw '验收语法错误' }
$PSScriptRoot = $payload.scripts
$WithGuiSmoke = $false; $WithMarkdownAcceptance = $false; $WithDesktopTests = $false
$initialization = [Collections.Generic.List[string]]::new()
$initialization.Add('$PSScriptRoot = $payload.scripts')
foreach ($statement in $ast.EndBlock.Statements) {
    if ($statement.Extent.Text -match "Invoke-Logged -Name 'static-check'") { break }
    $initialization.Add($statement.Extent.Text)
}
$initialize = [ScriptBlock]::Create(($initialization -join "`n"))
. $initialize | Out-Null
$first = $script:LogDir
$assets = Join-Path $first 'gui-markdown-assets'
New-Item -ItemType Directory -Path $assets -Force | Out-Null
$notice = Join-Path $assets 'notice'
[IO.File]::WriteAllText($notice, 'prior run')
. $initialize | Out-Null
if ($first -eq $script:LogDir) { throw 'repeat invocation reused the asset root' }
if (Test-Path -LiteralPath (Join-Path $script:LogDir 'gui-markdown-assets\notice')) {
    throw 'new invocation inherited an old notice'
}
if ([IO.File]::ReadAllText($notice) -ne 'prior run') { throw 'prior run was modified' }
[Console]::WriteLine('FRESH_PRESERVED')
"""
        powershell = Path(os.environ["SYSTEMROOT"]) / "System32/WindowsPowerShell/v1.0/powershell.exe"
        result = subprocess.run(
            [str(powershell), "-NoProfile", "-Command", harness],
            input=json.dumps({"path": str(ROOT / "scripts" / "acceptance.ps1"), "scripts": str(scripts)}),
            text=True, encoding="utf-8", capture_output=True, check=False, timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "FRESH_PRESERVED")


class DesktopAuthorizationTests(unittest.TestCase):
    def test_desktop_driver_entry_requires_explicit_authorization(self) -> None:
        # 只运行真实入口root赋值之前的if决策；不执行目录/工具发现、Cargo或桌面动作。
        harness = r"""
[Console]::InputEncoding = [Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
$payload = [Console]::In.ReadToEnd() | ConvertFrom-Json
$tokens = $null; $errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($payload.path, [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw '验收语法错误' }
$WithGuiSmoke=[bool]$payload.smoke
$WithMarkdownAcceptance=[bool]$payload.markdown
$WithDesktopTests=[bool]$payload.desktop
try {
    foreach ($statement in $ast.EndBlock.Statements) {
        if ($statement -is [Management.Automation.Language.AssignmentStatementAst] -and $statement.Left.Extent.Text -eq '$root') { break }
        if ($statement -is [Management.Automation.Language.IfStatementAst]) {
            . ([ScriptBlock]::Create($statement.Extent.Text))
        }
    }
    [Console]::WriteLine('ALLOWED')
} catch {
    [Console]::WriteLine('REFUSED:' + $_.Exception.Message)
}
"""
        powershell = Path(os.environ["SYSTEMROOT"]) / "System32/WindowsPowerShell/v1.0/powershell.exe"
        for smoke, markdown, desktop in (
            (True, False, False),
            (False, True, False),
            (True, True, False),
            (True, True, True),
            (False, False, False),
        ):
            with self.subTest(smoke=smoke, markdown=markdown, desktop=desktop):
                payload = {
                    "path": str(ROOT / "scripts" / "acceptance.ps1"),
                    "smoke": smoke,
                    "markdown": markdown,
                    "desktop": desktop,
                }
                result = subprocess.run(
                    [str(powershell), "-NoProfile", "-Command", harness],
                    input=json.dumps(payload),
                    text=True,
                    encoding="utf-8",
                    capture_output=True,
                    check=False,
                    timeout=30,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                if (smoke or markdown) and not desktop:
                    self.assertTrue(result.stdout.startswith("REFUSED:"), result.stdout)
                    self.assertIn("WithDesktopTests", result.stdout)
                else:
                    self.assertEqual(result.stdout.strip(), "ALLOWED")


class ReviewRequiredCheckExitTests(TrustedWorkspaceTests):
    def test_review_f60_missing_pygments_cannot_exit_success(self) -> None:
        # P-13：执行真实 rust_lexical→check→报告/退出；仅隔离其他检查，避免跑全仓门。
        checker = ROOT / "scripts" / "static_check.py"
        tree = ast.parse(checker.read_text(encoding="utf-8"), filename=str(checker))
        declarations: list[ast.stmt] = []
        for index, node in enumerate(tree.body):
            if isinstance(node, ast.If) and isinstance(node.test, ast.Name) and node.test.id == "UPDATE_BASELINE":
                runtime = tree.body[index + 1:]
                break
            if isinstance(
                node, (ast.Import, ast.ImportFrom, ast.FunctionDef, ast.ClassDef, ast.Assign, ast.AnnAssign, ast.Try)
            ):
                declarations.append(node)
        else:
            self.fail("static_check 真实执行入口未找到")
        namespace: dict[str, object] = {"__file__": str(checker), "__name__": "review_required_check"}
        exec(compile(ast.Module(body=declarations, type_ignores=[]), str(checker), "exec"), namespace)
        namespace["ROOT"] = self.directory
        namespace["UPDATE_BASELINE"] = False
        namespace["lex"] = None
        namespace["Lexer"] = None
        namespace["Comment"] = None
        namespace["Literal"] = None
        namespace["pygments"] = None
        # 注册表仍由原脚本执行；其他检查的环境依赖不属于此反例。
        registry = next(
            node for node in runtime
            if isinstance(node, ast.For) and isinstance(node.target, ast.Name) and node.target.id == "fn"
        )
        if not isinstance(registry.iter, ast.List):
            self.fail("static_check 检查注册表不是可隔离的列表")
        for entry in registry.iter.elts:
            if not isinstance(entry, ast.Name):
                self.fail("static_check 检查注册表含非函数引用")
            if entry.id != "rust_lexical":
                namespace[entry.id] = lambda: "isolated unrelated check"
        with (
            contextlib.redirect_stdout(io.StringIO()),
            self.assertRaises(SystemExit) as stopped,
        ):
            exec(compile(ast.Module(body=runtime, type_ignores=[]), str(checker), "exec"), namespace)
        report = json.loads((self.directory / ".tmp" / "static-check.json").read_text(encoding="utf-8"))
        skipped = [row for row in report["checks"] if row["status"] == "SKIP"]
        self.assertEqual([row["name"] for row in skipped], ["rust_lexical"])
        self.assertFalse(any(row["status"] == "FAIL" for row in report["checks"]), report)
        self.assertNotEqual(stopped.exception.code, 0, "必需检查未运行时不能向 acceptance.ps1 报成功")


if __name__ == "__main__":
    unittest.main()
