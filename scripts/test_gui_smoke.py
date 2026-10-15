"""gui_smoke 判据及内存 UIA 阶段回归单测.

不启动真实 GUI、不操作硬件鼠标、不写仓库目录（tempfile 例外）；
受 Bandit B101 管控的 assert 逐条附 nosec 说明。
"""

from __future__ import annotations

import contextlib
import json
import os
import shutil
import sqlite3
import subprocess
import tempfile
import unittest
import zipfile
from collections import Counter
from dataclasses import dataclass
from functools import partial
from pathlib import Path
from typing import TYPE_CHECKING, cast
from unittest.mock import Mock, patch

if TYPE_CHECKING:
    from collections.abc import Callable

    from pywinauto.application import WindowSpecification

from scripts import gui_smoke, requirement_coverage, xberg_test_engine

# 与 src/gui.rs 收尾统计行同构的样本：两轮计数与四舍五入后的总耗时可以逐字相同。
DONE_LINE = "成功 2 · 部分提取 0 · 失败 0 · 已有结果跳过 0 · 重复结果跳过 0 · 总耗时 0.4s"
DIFFERENT_LINE = "成功 0 · 部分提取 0 · 失败 0 · 已有结果跳过 2 · 重复结果跳过 0 · 总耗时 0.4s"
ISOLATED_ENV = {
    "JCHTOOLS_TEST_STATE_DIR": r"C:\isolated\state",
    "JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT": r"C:\isolated\snap-assets",
}
REQUIRED_IDLE_CHECKS = 2


def snapshot_names(snapshot: frozenset[tuple[str, int, int]]) -> set[str]:
    return {entry[0] for entry in snapshot}


class ProducedSnapshotTests(unittest.TestCase):
    def test_snapshot_tracks_new_updated_and_unchanged_state(self) -> None:
        with tempfile.TemporaryDirectory(prefix="jchtools-smoke-unit-") as temporary:
            root = Path(temporary)
            empty = gui_smoke.produced_snapshot(root)
            assert empty == frozenset()  # nosec B101: 回归测试断言，不用于产品权限或输入校验。
            (root / "sub").mkdir()
            _ = (root / "sub" / "one_txt.md").write_bytes(b"one\n")
            produced = gui_smoke.produced_snapshot(root)
            assert produced != empty  # nosec B101: 新产物必须改变快照（S11-06 代次锚）。
            assert snapshot_names(produced) == {f"sub{os.sep}one_txt.md"}  # nosec B101: 快照记录相对路径。
            assert gui_smoke.produced_snapshot(root) == produced  # nosec B101: 未再写入时快照稳定。
            _ = (root / "sub" / "one_txt.md").write_bytes(b"one rewritten with different length\n")
            updated = gui_smoke.produced_snapshot(root)
            assert updated != produced  # nosec B101: 内容更新（大小变化）必须改变快照。

    def test_snapshot_of_missing_directory_is_empty(self) -> None:
        with tempfile.TemporaryDirectory(prefix="jchtools-smoke-missing-") as temporary:
            missing = Path(temporary) / "missing"
            assert not missing.exists()  # nosec B101: 本次隔离的子目录必须确实缺失。
            assert gui_smoke.produced_snapshot(missing) == frozenset()  # nosec B101: 目录不可读返回空快照。


class CancelledOutputTests(unittest.TestCase):
    """覆盖 T-23/P-11/P-12：对真实 GUI 用例的业务判据做反例回归。."""

    def test_completed_output_survives_and_in_flight_output_is_allowed_before_stop_lands(self) -> None:
        # 停止请求存在 UIA/调度时延：当前文件若在停止落地前自然完成，其成品按
        # T-23「已完成产物保留」合法提交；后续文件与临时产物仍然必须被拒绝。
        with tempfile.TemporaryDirectory(prefix="jchtools-stop-unit-") as temporary:
            root = Path(temporary)
            completed = root / "done.md"
            _ = completed.write_bytes(b"completed")
            before = gui_smoke.snapshot_output_bytes(root)
            gui_smoke.verify_cancelled_outputs(root, before, "current.md")
            _ = (root / "current.md").write_bytes(b"in-flight finished just before stop landed")
            gui_smoke.verify_cancelled_outputs(root, before, "current.md")
            for name in ("next.md", ".jch-markdown-partial"):
                artifact = root / name
                _ = artifact.write_bytes(b"partial")
                try:
                    gui_smoke.verify_cancelled_outputs(root, before, "current.md")
                except RuntimeError:
                    pass
                else:
                    message = f"取消判据必须拒绝当前文件之外的产物：{name}"
                    raise AssertionError(message)
                artifact.unlink()
            _ = completed.write_bytes(b"modified")
            try:
                gui_smoke.verify_cancelled_outputs(root, before, "current.md")
            except RuntimeError:
                pass
            else:
                message = "取消判据必须拒绝此前成品被改写"
                raise AssertionError(message)
            _ = completed.write_bytes(b"completed")
            _ = (root / "current.md").unlink()
            lost = root / "done.md"
            _ = lost.unlink()
            try:
                gui_smoke.verify_cancelled_outputs(root, before, "current.md")
            except RuntimeError:
                pass
            else:
                message = "取消判据必须拒绝此前成品丢失"
                raise AssertionError(message)


class LayoutAndCoverageTests(unittest.TestCase):
    """覆盖 P-11/P-12/P-13：布局反例及需求审计不得产生假绿。."""

    def test_layout_rejects_clipped_zero_area_and_overlapping_buttons(self) -> None:
        gui_smoke.verify_control_rectangles((0, 0, 100, 100), [(1, 1, 10, 10), (20, 1, 30, 10)])
        invalid = [[(-1, 1, 10, 10)], [(1, 1, 1, 10)], [(1, 1, 10, 10), (5, 5, 15, 15)]]
        for rectangles in invalid:
            try:
                gui_smoke.verify_control_rectangles((0, 0, 100, 100), rectangles)
            except RuntimeError:
                pass
            else:
                message = "布局自动判据没有拒绝裁切、零面积或重叠"
                raise AssertionError(message)

    def test_requirement_ranges_and_empty_authority_are_handled(self) -> None:
        identifiers = requirement_coverage.referenced_ids("覆盖 XB-10\uff5eXB-12 / T-23")
        assert identifiers == {"XB-10", "XB-11", "XB-12", "T-23"}  # nosec B101: 需求映射回归断言。
        with tempfile.TemporaryDirectory() as temporary:
            try:
                _ = requirement_coverage.requirement_rows(Path(temporary))
            except ValueError:
                pass
            else:
                message = "没有权威需求时不能生成空白通过清单"
                raise AssertionError(message)

    def test_latest_engine_metadata_requires_the_official_archive_digest(self) -> None:
        tag = "v2099.1.1"
        asset: dict[str, object] = {
            "name": xberg_test_engine.ARCHIVE_NAME,
            "size": 123,
            "digest": "sha256:" + "a" * 64,
            "browser_download_url": f"https://github.com/{xberg_test_engine.REPOSITORY}/releases/download/{tag}/{xberg_test_engine.ARCHIVE_NAME}",
        }
        metadata: dict[str, object] = {"tag_name": tag, "assets": [asset]}
        release = xberg_test_engine.release_from_metadata(metadata)
        assert release.tag == tag  # nosec B101: 最新引擎准备入口的元数据回归。
        for key, invalid in (
            ("digest", ""),
            ("browser_download_url", "https://example.invalid/engine.zip"),
            ("size", 0),
        ):
            bad_asset = dict(asset)
            bad_asset[key] = invalid
            bad = dict(metadata)
            bad["assets"] = [bad_asset]
            try:
                _ = xberg_test_engine.release_from_metadata(bad)
            except ValueError:
                pass
            else:
                message = f"最新测试引擎元数据未拒绝非法 {key}"
                raise AssertionError(message)

    def test_empty_unfinished_media_directory_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory(prefix="jchtools-stop-media-unit-") as temporary:
            root = Path(temporary)
            (root / "current_media").mkdir()
            try:
                gui_smoke.verify_cancelled_outputs(root, {}, "current.md")
            except RuntimeError:
                pass
            else:
                message = "取消判据必须拒绝未完成媒体目录"
                raise AssertionError(message)


class LatestEngineRecoveryTests(unittest.TestCase):
    """覆盖 XB-10/P-13：测试引擎准备失败不能丢失完整旧备份。."""

    def test_identity_rejection_never_deletes_or_restores_a_redirected_target(self) -> None:
        with tempfile.TemporaryDirectory(prefix="latest-identity-unit-") as temporary:
            root = Path(temporary)
            target, work = root / "engine", root / "workspace"
            target.mkdir()
            work.mkdir()
            _ = (target / "xberg.exe").write_bytes(b"old complete engine")
            archive = root / xberg_test_engine.ARCHIVE_NAME
            with zipfile.ZipFile(archive, "w") as package:
                package.writestr("xberg.exe", b"new engine")
            release = xberg_test_engine.Release("v2099.1.1", gui_smoke.file_digest(archive), archive.stat().st_size)
            real_replace = xberg_test_engine.replace_prepared_tree
            real_remove, real_copy = shutil.rmtree, shutil.copytree
            mutations: list[str] = []

            def fake_command(argv: list[str], **_options: object) -> subprocess.CompletedProcess[str]:
                if len(argv) > 1 and argv[1] == "release":
                    _ = shutil.copyfile(archive, Path(argv[-1]) / archive.name)
                return subprocess.CompletedProcess(argv, 0, "", "")

            def redirect_before_replace(prepared: Path, expected_sha256: str) -> None:
                xberg_test_engine.EXPECTED_TEST_ENGINE = root / "redirected"
                real_replace(prepared, expected_sha256)

            def tracked_remove(path: str | Path, **_options: object) -> None:
                if Path(path).resolve() == target.resolve():
                    mutations.append("delete target")
                real_remove(path)

            def tracked_copy(source: str | Path, destination: str | Path, **_options: object) -> str | Path:
                if Path(destination).resolve() == target.resolve():
                    mutations.append("restore target")
                return real_copy(source, destination)

            failure = ""
            with (
                patch.object(xberg_test_engine, "TEST_ENGINE", target),
                patch.object(xberg_test_engine, "EXPECTED_TEST_ENGINE", target.resolve()),
                patch.object(xberg_test_engine, "WORKSPACE", work),
                patch.object(xberg_test_engine, "ensure_test_engine_is_idle", lambda: None),
                patch.object(xberg_test_engine, "replace_prepared_tree", redirect_before_replace),
                patch("scripts.xberg_test_engine.subprocess.run", fake_command),
                patch("scripts.xberg_test_engine.shutil.rmtree", tracked_remove),
                patch("scripts.xberg_test_engine.shutil.copytree", tracked_copy),
            ):
                try:
                    xberg_test_engine.install_latest("synthetic-gh", release)
                except OSError as error:
                    failure = str(error)
            backups = list(work.rglob("previous/xberg.exe"))
            assert not mutations  # nosec B101: 身份校验拒绝后不得删除或复制到错误目标。
            assert backups  # nosec B101: 身份拒绝后的完整备份必须保留。
            assert str(backups[0].parent) in failure  # nosec B101: 错误须报告保留的备份路径。
            assert backups[0].read_bytes() == b"old complete engine"  # nosec B101: 旧备份字节完整。
            assert (target / "xberg.exe").read_bytes() == b"old complete engine"  # nosec B101: 目标未被误删或改写。

    def test_recovery_failure_preserves_backup_and_rechecks_idle_before_delete(self) -> None:
        with tempfile.TemporaryDirectory(prefix="latest-recovery-unit-") as temporary:
            root = Path(temporary)
            target, work = root / "engine", root / "workspace"
            target.mkdir()
            work.mkdir()
            _ = (target / "xberg.exe").write_bytes(b"old complete engine")
            archive = root / xberg_test_engine.ARCHIVE_NAME
            with zipfile.ZipFile(archive, "w") as package:
                package.writestr("xberg.exe", b"new engine")
            release = xberg_test_engine.Release("v2099.1.1", gui_smoke.file_digest(archive), archive.stat().st_size)
            idle_checks: list[None] = []
            delete_checks: list[int] = []
            real_remove = shutil.rmtree

            def idle() -> None:
                idle_checks.append(None)

            def fake_command(argv: list[str], **_options: object) -> subprocess.CompletedProcess[str]:
                if len(argv) > 1 and argv[1] == "release":
                    _ = shutil.copyfile(archive, Path(argv[-1]) / archive.name)
                return subprocess.CompletedProcess(argv, 0, "", "")

            def fail_target_delete(path: object, **_options: object) -> None:
                if not isinstance(path, (str, Path)):
                    message = "测试删除参数不是路径"
                    raise TypeError(message)
                if Path(path).resolve() == target.resolve():
                    delete_checks.append(len(idle_checks))
                    message = "simulated locked engine during replacement and rollback"
                    raise OSError(message)
                real_remove(path)

            failure = ""
            with (
                patch.object(xberg_test_engine, "TEST_ENGINE", target),
                patch.object(xberg_test_engine, "EXPECTED_TEST_ENGINE", target.resolve()),
                patch.object(xberg_test_engine, "WORKSPACE", work),
                patch.object(xberg_test_engine, "ensure_test_engine_is_idle", idle),
                patch("scripts.xberg_test_engine.subprocess.run", fake_command),
                patch("scripts.xberg_test_engine.shutil.rmtree", fail_target_delete),
            ):
                try:
                    xberg_test_engine.install_latest("synthetic-gh", release)
                except OSError as error:
                    failure = str(error)
            backups = list(work.rglob("previous/xberg.exe"))
            assert backups  # nosec B101: 失败恢复必须保留完整旧备份。
            assert backups[0].read_bytes() == b"old complete engine"  # nosec B101: 备份内容完整。
            assert delete_checks  # nosec B101: 必须实际覆盖替换失败路径。
            assert all(count >= REQUIRED_IDLE_CHECKS for count in delete_checks)  # nosec B101: 删除前重新检查空闲。
            assert str(backups[0].parent) in failure  # nosec B101: 恢复失败须报告保留的备份位置。


class CompletionConfirmsNewRunTests(unittest.TestCase):
    def test_identical_line_with_changed_snapshot_confirms_new_run(self) -> None:
        # S11-06 回归：统计行与上一轮逐字相同时，旧判据（只比文本）识别不出新完成。
        before = frozenset({("one_txt.md", 1, 4)})
        after = frozenset({("one_txt.md", 2, 8)})
        assert gui_smoke.completion_confirms_new_run(DONE_LINE, DONE_LINE, before, after)  # nosec B101: 回归测试断言。

    def test_identical_line_with_unchanged_snapshot_is_not_new(self) -> None:
        same = frozenset({("one_txt.md", 1, 4)})
        assert not gui_smoke.completion_confirms_new_run(DONE_LINE, DONE_LINE, same, same)  # nosec B101: 回归测试断言。

    def test_identical_line_without_snapshot_cannot_confirm(self) -> None:
        assert not gui_smoke.completion_confirms_new_run(DONE_LINE, DONE_LINE, None, None)  # nosec B101: 回归测试断言。
        assert not gui_smoke.completion_confirms_new_run(DONE_LINE, DONE_LINE, None, frozenset())  # nosec B101: 回归测试断言。

    def test_different_line_confirms_without_snapshot(self) -> None:
        assert gui_smoke.completion_confirms_new_run(DIFFERENT_LINE, DONE_LINE, None, None)  # nosec B101: 回归测试断言。

    def test_identical_line_with_unavailable_before_snapshot_cannot_confirm(self) -> None:
        """覆盖 P-11/P-13：前置快照读取失败后恢复可读，不能把旧产物当成新完成。."""
        existing = frozenset({("old_txt.md", 1, 4)})
        self.assertFalse(
            gui_smoke.completion_confirms_new_run(DONE_LINE, DONE_LINE, frozenset(), existing),
            "空前置快照表示无可用代次锚，旧统计与恢复可读的旧产物不得确认新一轮完成",
        )

    def test_line_without_done_marker_never_confirms(self) -> None:
        running = "正在处理 one.txt"
        assert not gui_smoke.completion_confirms_new_run(running, DONE_LINE, None, frozenset({("a", 1, 1)}))  # nosec B101: 回归测试断言。


class IsolationGuardTests(unittest.TestCase):
    def test_missing_isolation_env_lists_absent_keys_in_fixed_order(self) -> None:
        assert gui_smoke.missing_isolation_env({}) == list(gui_smoke.ISOLATION_REQUIRED_ENV_KEYS)  # nosec B101: 回归测试断言。
        partial = {"JCHTOOLS_TEST_STATE_DIR": r"C:\isolated\state"}
        assert gui_smoke.missing_isolation_env(partial) == ["JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT"]  # nosec B101: 回归测试断言。
        assert gui_smoke.missing_isolation_env(ISOLATED_ENV) == []  # nosec B101: 回归测试断言。

    def test_guarded_stage_without_isolation_is_refused_with_entry_hint(self) -> None:
        for stages in (["S5"], ["S10", "S11", "S12", "S13"], ["S14"], ["S17"], ["S1", "S11"]):
            message = ""
            try:
                gui_smoke.require_orchestrated_isolation(stages, {}, allow_isolated_run=False)
            except RuntimeError as error:
                message = str(error)
            assert message, f"{stages} 缺隔离环境时必须拒绝启动"  # nosec B101: 回归测试断言，不用于产品权限或输入校验。
            assert "JCHTOOLS_TEST_STATE_DIR" in message  # nosec B101: 拒绝信息必须指认缺失变量。
            assert "acceptance.ps1" in message  # nosec B101: 拒绝信息必须指引编排入口。
            assert "--allow-isolated-run" in message  # nosec B101: 拒绝信息必须给出显式逃生口。

    def test_guard_passes_with_isolated_env_or_unguarded_stages_or_explicit_flag(self) -> None:
        gui_smoke.require_orchestrated_isolation(["S5", "S17"], ISOLATED_ENV, allow_isolated_run=False)
        with self.assertRaises(RuntimeError):  # noqa: PT027 - 标准库 unittest 入口，不引入额外运行依赖。
            gui_smoke.require_orchestrated_isolation(["S1", "S4", "S15"], {}, allow_isolated_run=False)
        gui_smoke.require_orchestrated_isolation(["S5", "S14"], {}, allow_isolated_run=True)

    def test_every_non_self_isolating_stage_rejects_missing_state(self) -> None:
        """覆盖 P-11/P-13/XB-22：所有会启动 GUI 的非自隔离阶段必须守卫。."""
        for stage in ("S1", "S2", "S3", "S4", "S5", "S10", "S11", "S12", "S13", "S14", "S15", "S16", "S17", "S18"):
            with self.subTest(stage=stage), self.assertRaises(RuntimeError):  # noqa: PT027 - 标准库 unittest 入口，不引入额外运行依赖。
                gui_smoke.require_orchestrated_isolation([stage], {}, allow_isolated_run=False)

    def test_public_stage_setup_rejects_missing_isolation_before_spawning(self) -> None:
        """覆盖 P-11/P-13/XB-22：直接调用公共入口也不得启动生产状态 GUI。."""
        environments: tuple[dict[str, str] | None, ...] = (None, {})
        for supplied in environments:
            with (
                self.subTest(env=supplied),
                patch.dict(os.environ, {}, clear=True),
                patch(
                    "scripts.gui_smoke.subprocess.Popen", side_effect=AssertionError("隔离拒绝前不得启动进程")
                ) as spawn,
                self.assertRaises(RuntimeError),  # noqa: PT027 - 标准库 unittest 入口，不引入额外运行依赖。
            ):
                gui_smoke.run_stage("S1", "unused.exe", lambda _window: None, env=supplied)
            spawn.assert_not_called()

    def test_self_isolated_settings_do_not_inherit_host_runtime_or_audio_barrier(self) -> None:
        with tempfile.TemporaryDirectory(prefix="gui-settings-isolation-") as temporary:
            state = Path(temporary).resolve()
            inherited = {
                "JCHTOOLS_TEST_XBERG_DIR": str(state / "host-engine"),
                "JCHTOOLS_SMOKE_XBERG_DIR": str(state / "host-smoke-engine"),
                "JCHTOOLS_TEST_BUNDLED_XBERG_DIR": str(state / "host-installation"),
                "JCHTOOLS_TEST_AUDIO_DOWNLOAD_BARRIER": str(state / "unrelated-barrier"),
                "JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT": str(state / "host-assets"),
            }
            with patch.dict(os.environ, inherited):
                env = gui_smoke.isolated_state_env(state)
                explicit = gui_smoke.isolated_state_env(state, state / "fixture-bundle")
            self.assertEqual(env["JCHTOOLS_TEST_BUNDLED_XBERG_DIR"], str(state / "absent-bundle"))
            self.assertEqual(explicit["JCHTOOLS_TEST_BUNDLED_XBERG_DIR"], str(state / "fixture-bundle"))
            self.assertEqual(env["JCHTOOLS_TEST_STATE_DIR"], str(state))
            self.assertEqual(env["JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT"], str(state / "markdown-assets"))
            self.assertEqual(env["JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT"], str(state / "snap-assets"))
            for key in ("JCHTOOLS_TEST_XBERG_DIR", "JCHTOOLS_SMOKE_XBERG_DIR", "JCHTOOLS_TEST_AUDIO_DOWNLOAD_BARRIER"):
                self.assertNotIn(key, env)


class BundledRuntimePredicateTests(unittest.TestCase):
    """只验证落盘结果/拒绝条件；合成文件不代表真实引擎或 GUI 端到端通过."""

    @staticmethod
    def write_package(root: Path) -> dict[str, object]:
        files = {"xberg.exe": b"synthetic engine", "sherpa-onnx/runtime.dll": b"synthetic shared runtime"}
        members: list[dict[str, object]] = []
        for relative, content in files.items():
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            _ = path.write_bytes(content)
            members.append({"path": relative, "size_bytes": len(content), "sha256": gui_smoke.file_digest(path)})
        tag = "v2099.1.1"
        archive = "xberg-audio-assets-x86_64-pc-windows-msvc.zip"
        receipt: dict[str, object] = {
            "owner": "JchTools-bundled-xberg-v1",
            "source": "https://api.github.com/repos/jchanghong023/xberg/releases/latest",
            "tag": tag,
            "components": [{"id": "document", "members": members}, {"id": "snapshot", "members": members}],
            "audio_assets": {
                "id": "audio-assets",
                "archive": archive,
                "url": f"https://github.com/jchanghong023/xberg/releases/download/{tag}/{archive}",
            },
        }
        BundledRuntimePredicateTests.write_receipt(root, receipt)
        return receipt

    @staticmethod
    def write_receipt(root: Path, receipt: dict[str, object]) -> None:
        _ = (root / "jchtools-xberg-release.json").write_text(json.dumps(receipt), encoding="utf-8")

    def test_package_predicate_checks_actual_member_bytes_and_exact_tree(self) -> None:
        with tempfile.TemporaryDirectory(prefix="gui-base-predicate-") as temporary:
            root = Path(temporary)
            _ = self.write_package(root)
            before = gui_smoke.verify_bundled_package(root)
            self.assertEqual(before, gui_smoke.snapshot_output_bytes(root))
            engine = root / "xberg.exe"
            original = engine.read_bytes()
            _ = engine.write_bytes(b"x" * len(original))
            with self.assertRaises(RuntimeError):  # noqa: PT027 - 标准库 unittest。
                _ = gui_smoke.verify_bundled_package(root)
            _ = engine.write_bytes(original)
            engine.unlink()
            with self.assertRaises(RuntimeError):  # noqa: PT027 - 标准库 unittest。
                _ = gui_smoke.verify_bundled_package(root)
            _ = engine.write_bytes(original)
            extra = root / "not-in-receipt.dll"
            _ = extra.write_bytes(b"unexpected")
            with self.assertRaises(RuntimeError):  # noqa: PT027 - 标准库 unittest。
                _ = gui_smoke.verify_bundled_package(root)

    def test_default_package_rejects_audio_even_as_empty_directories(self) -> None:
        for relative in ("ffmpeg", "models/vad", "models/sense_voice_small"):
            with self.subTest(relative=relative), tempfile.TemporaryDirectory(prefix="gui-base-audio-") as temporary:
                root = Path(temporary)
                _ = self.write_package(root)
                (root / relative).mkdir(parents=True)
                with self.assertRaises(RuntimeError):  # noqa: PT027 - 标准库 unittest。
                    _ = gui_smoke.verify_bundled_package(root)

    def test_component_identity_and_audio_release_must_match_the_base(self) -> None:
        for change in ("media-component", "wrong-audio-release", "unsafe-member", "conflicting-shared-member"):
            with self.subTest(change=change), tempfile.TemporaryDirectory(prefix="gui-base-record-") as temporary:
                root = Path(temporary)
                receipt = self.write_package(root)
                components = cast("list[dict[str, object]]", receipt["components"])
                audio = cast("dict[str, object]", receipt["audio_assets"])
                members = cast("list[dict[str, object]]", components[0]["members"])
                if change == "media-component":
                    components[1]["id"] = "media"
                elif change == "wrong-audio-release":
                    audio["url"] = str(audio["url"]).replace("v2099.1.1", "v2099.1.2")
                elif change == "unsafe-member":
                    members[0]["path"] = "../outside.exe"
                else:
                    components[1]["members"] = [dict(members[0], sha256="0" * 64)]
                self.write_receipt(root, receipt)
                with self.assertRaises(RuntimeError):  # noqa: PT027 - 标准库 unittest。
                    _ = gui_smoke.verify_bundled_package(root)

    def test_active_source_requires_both_saved_directory_records(self) -> None:
        with tempfile.TemporaryDirectory(prefix="gui-active-source-") as temporary:
            root = Path(temporary)
            runtime = root / "xberg"
            current = {
                "xberg_source": "bundled",
                "xberg_directory": str(runtime),
                "xberg_bundled_directory": str(runtime),
                "xberg_custom_directory": str(root / "legacy"),
            }
            database = root / "config.sqlite3"
            with contextlib.closing(sqlite3.connect(database)) as connection:
                _ = connection.execute("CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT)")
                _ = connection.executemany("INSERT INTO app_settings VALUES(?,?)", current.items())
                connection.commit()
            self.assertEqual(gui_smoke.active_xberg_settings(root), current)
            gui_smoke.verify_bundled_settings(gui_smoke.active_xberg_settings(root), runtime)
            for key, value in (("xberg_source", "custom"), ("xberg_directory", str(root)), ("xberg_bundled_directory", "")):
                with self.subTest(key=key), self.assertRaises(RuntimeError):  # noqa: PT027 - 标准库 unittest。
                    gui_smoke.verify_bundled_settings(dict(current, **{key: value}), runtime)
            self.assertEqual(gui_smoke.active_xberg_settings(root), current, "只读判据不得改写 SQLite")

    def test_download_start_needs_a_real_barrier_marker(self) -> None:
        with tempfile.TemporaryDirectory(prefix="gui-audio-barrier-") as temporary:
            marker = Path(temporary) / "barrier"
            with patch.object(gui_smoke, "TIMEOUT", 0), self.assertRaises(RuntimeError):  # noqa: PT027 - 标准库 unittest。
                gui_smoke.wait_download_barrier(marker)
            marker.touch()
            gui_smoke.wait_download_barrier(marker)


class StageSelectionTests(unittest.TestCase):
    def test_empty_stage_selection_is_rejected(self) -> None:
        """覆盖 P-13：零阶段执行不能报告验收成功。."""
        for selection in ("", " ", ",", " , , "):
            with self.subTest(selection=selection), self.assertRaises(RuntimeError):  # noqa: PT027 - 标准库 unittest 入口，不引入额外运行依赖。
                _ = gui_smoke.parse_stages(selection)


class ExistingResultsPreservationTests(unittest.TestCase):
    def test_second_run_rejects_deleted_existing_results(self) -> None:
        """覆盖 T-12/P-11/P-12：跳过统计不能掩盖一份或全部既有结果丢失。."""
        for remove_all in (False, True):
            with self.subTest(remove_all=remove_all):
                calls = 0

                def convert(
                    _window: WindowSpecification,
                    previous_done: str = "",
                    produced_dir: Path | None = None,
                    *,
                    _remove_all: bool = remove_all,
                ) -> str:
                    nonlocal calls
                    if previous_done != ("" if calls == 0 else DONE_LINE):
                        message = "回归必须从第一轮完成统计启动第二轮"
                        raise AssertionError(message)
                    if produced_dir is None:
                        message = "回归必须使用真实输出目录"
                        raise AssertionError(message)
                    calls += 1
                    if calls == 1:
                        for name in ("one_txt.md", "two_txt.md"):
                            _ = (produced_dir / name).write_bytes(b"completed result")
                        return DONE_LINE
                    (produced_dir / "one_txt.md").unlink()
                    if _remove_all:
                        (produced_dir / "two_txt.md").unlink()
                    return DIFFERENT_LINE

                def stage(_tag: str, _exe: str, body: Callable[[WindowSpecification], None]) -> None:
                    body(cast("WindowSpecification", object()))

                with (
                    patch("scripts.gui_smoke.run_stage", side_effect=stage),
                    patch("scripts.gui_smoke.goto_converter"),
                    patch("scripts.gui_smoke.set_converter_dirs"),
                    patch("scripts.gui_smoke.start_conversion_and_wait_done", side_effect=convert),
                    self.assertRaises(RuntimeError),  # noqa: PT027 - 标准库 unittest 入口，不引入额外运行依赖。
                ):
                    gui_smoke.s11_existing_results_are_skipped_untouched("unused.exe")


class StateDirectoryTests(unittest.TestCase):
    def test_isolated_task_root_overrides_localappdata(self) -> None:
        env = {"JCHTOOLS_TEST_STATE_DIR": r"C:\isolated\state", "LOCALAPPDATA": r"C:\real"}
        with patch.dict(os.environ, env, clear=True):
            assert gui_smoke.state_dir() == Path(r"C:\isolated\state")  # nosec B101: 隔离状态根回归断言。

    def test_relative_isolated_task_root_is_rejected(self) -> None:
        env = {"JCHTOOLS_TEST_STATE_DIR": "relative-state", "LOCALAPPDATA": r"C:\real"}
        failure = ""
        with patch.dict(os.environ, env, clear=True):
            try:
                _ = gui_smoke.state_dir()
            except RuntimeError as error:
                failure = str(error)
            else:
                message = "相对状态根不得回退到真实任务目录"
                raise AssertionError(message)
        assert "绝对路径" in failure  # nosec B101: 非法路径明确拒绝，不回退生产状态。

    def test_default_task_root_is_preserved_without_override(self) -> None:
        with patch.dict(os.environ, {"LOCALAPPDATA": r"C:\real"}, clear=True):
            assert gui_smoke.state_dir() == Path(r"C:\real") / "JchTools" / "data"  # nosec B101: 默认目录回归。

    def test_empty_explicit_root_is_rejected_instead_of_falling_back(self) -> None:
        env = {"JCHTOOLS_TEST_STATE_DIR": "", "LOCALAPPDATA": r"C:\real"}
        with patch.dict(os.environ, env, clear=True):
            try:
                _ = gui_smoke.state_dir()
            except RuntimeError:
                return
        message = "显式空隔离根必须与 Rust 策略一致地拒绝，不能回退到生产状态"
        raise AssertionError(message)


class TaskRecordTests(unittest.TestCase):
    """覆盖 P-11/P-13：任务观察不能把合法隔离路径误判成没有完成任务。."""

    def test_task_records_accept_uri_reserved_characters_in_state_path(self) -> None:
        with tempfile.TemporaryDirectory(prefix="jchtools-task-record-unit-") as temporary:
            root = Path(temporary)
            state = root / "state#percent%25"
            task = state / "tasks" / "20261015T120000-synthetic"
            task.mkdir(parents=True)
            data = root / "input"
            data.mkdir()
            with contextlib.closing(sqlite3.connect(task / "task.sqlite3")) as connection, connection:
                connection.execute("CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT)")
                connection.executemany(
                    "INSERT INTO metadata (key,value) VALUES (?,?)",
                    (("root", json.dumps(str(data.resolve()))), ("status", json.dumps("finished"))),
                )
            with patch.dict(os.environ, {"JCHTOOLS_TEST_STATE_DIR": str(state)}, clear=True):
                self.assertEqual(gui_smoke._task_records(str(data)), [(task.name, "finished")])

    def test_unreadable_metadata_closes_the_observer_connection(self) -> None:
        with tempfile.TemporaryDirectory(prefix="jchtools-task-close-unit-") as temporary:
            root = Path(temporary)
            task = root / "tasks" / "20261015T120000-synthetic"
            task.mkdir(parents=True)
            database = task / "task.sqlite3"
            database.touch()
            connection = sqlite3.connect(database)
            with (
                patch.dict(os.environ, {"JCHTOOLS_TEST_STATE_DIR": str(root)}, clear=True),
                patch("scripts.gui_smoke.sqlite3.connect", return_value=connection),
            ):
                self.assertEqual(gui_smoke._task_records(str(root / "input")), [])
            try:
                with self.assertRaises(sqlite3.ProgrammingError):
                    connection.execute("SELECT 1")
            finally:
                connection.close()


def require_organize_failure(
    root: Path,
    expected: dict[str, tuple[str, int]],
    git_before: tuple[frozenset[str], dict[str, tuple[str, int]]],
) -> None:
    try:
        gui_smoke.verify_organize_witnesses(root, expected, git_before)
    except RuntimeError:
        return
    message = "整理结果判据未拒绝合同反例"
    raise AssertionError(message)


class OrganizeResultTests(unittest.TestCase):
    """覆盖 C-02/C-03/C-04/C-05/C-14/C-21：真实整理的判据须拒绝只有 finished 的假绿。."""

    def test_unchanged_input_tree_is_not_a_completed_organization(self) -> None:
        with tempfile.TemporaryDirectory(prefix="organize-result-unit-") as temporary:
            root = Path(temporary)
            expected = gui_smoke.prepare_organize_witnesses(root)
            git_before = gui_smoke.tree_snapshot(root / "__gui_witness" / "project")
            require_organize_failure(root, expected, git_before)

    def test_result_oracle_rejects_corruption_extra_copy_and_missing_git_directory(self) -> None:
        with tempfile.TemporaryDirectory(prefix="organize-result-unit-") as temporary:
            root = Path(temporary)
            expected = gui_smoke.prepare_organize_witnesses(root)
            source = root / "__gui_witness"
            git_before = gui_smoke.tree_snapshot(source / "project")
            by_digest = {gui_smoke.file_digest(path): path for path in source.rglob("*") if path.is_file()}
            for name, (digest, stamp) in expected.items():
                target = root / name
                target.parent.mkdir(exist_ok=True)
                _ = shutil.copyfile(by_digest[digest], target)
                os.utime(target, ns=(stamp, stamp))
            project = root / "Git项目集合" / "project"
            project.parent.mkdir()
            _ = shutil.copytree(source / "project", project)
            shutil.rmtree(source)
            gui_smoke.verify_organize_witnesses(root, expected, git_before)
            extra = root / "文档" / "extra.txt"
            _ = shutil.copyfile(root / "文档" / "copy_2.txt", extra)
            require_organize_failure(root, expected, git_before)
            extra.unlink()
            damaged = root / "文档" / "same.txt"
            original = damaged.read_bytes()
            original_stamp = damaged.stat().st_mtime_ns
            _ = damaged.write_bytes(b"corruption")
            require_organize_failure(root, expected, git_before)
            _ = damaged.write_bytes(original)
            os.utime(damaged, ns=(original_stamp, original_stamp))
            (project / "empty").rmdir()
            require_organize_failure(root, expected, git_before)


class OriginalDispositionTests(unittest.TestCase):
    """覆盖 X-09/X-10/P-12：数字尾缀不等于白名单分卷，原样保留才是正确结果。."""

    def test_non_archive_numeric_suffixes_are_preserved_in_every_dataset_section(self) -> None:
        names = ("data.001", "windows.iso.001", "report.docx.001", "archive.zip.0001", "archive.zip.01")
        sections = ("07-压缩包-各格式", "17-容器不自动解压")
        with tempfile.TemporaryDirectory(prefix="jchtools-numeric-original-unit-") as temporary:
            root = Path(temporary)
            for section in sections:
                (root / section).mkdir()
                for name in names:
                    with self.subTest(section=section, name=name):
                        relative = Path(section) / name
                        original = root / relative
                        _ = original.write_bytes(b"ordinary numeric-suffix input")
                        originals = {relative: gui_smoke.file_digest(original)}
                        gui_smoke.verify_original_disposition(root, originals, Counter())
                        _ = original.write_bytes(b"changed ordinary input")
                        with self.assertRaises(RuntimeError):
                            gui_smoke.verify_original_disposition(root, originals, Counter())
                        original.unlink()
                        with self.assertRaises(RuntimeError):
                            gui_smoke.verify_original_disposition(root, originals, Counter())


class _ReviewClock:
    def __init__(self) -> None:
        self.now = 0.0

    def read(self) -> float:
        return self.now

    def sleep(self, seconds: float) -> None:
        self.now += seconds


@dataclass(frozen=True)
class _ReviewRectangle:
    left: int
    top: int
    right: int
    bottom: int


class _ReviewControl:
    def __init__(
        self,
        title: str,
        rectangle: _ReviewRectangle = _ReviewRectangle(250, 80, 390, 112),
        *,
        action: Callable[[], None] | None = None,
        before_wait: Callable[[], None] | None = None,
    ) -> None:
        self.title = title
        self.bounds = rectangle
        self.action = action
        self.before_wait = before_wait

    def window_text(self) -> str:
        return self.title

    def rectangle(self) -> _ReviewRectangle:
        return self.bounds

    def is_visible(self) -> bool:
        return True

    def is_enabled(self) -> bool:
        return True

    def wait(self, _state: str, **_options: object) -> _ReviewControl:
        if self.before_wait is not None:
            self.before_wait()
        return self

    def invoke(self) -> None:
        if self.action is not None:
            self.action()

    def click_input(self) -> None:
        message = "内存 UIA 回归禁止鼠标兜底"
        raise AssertionError(message)


class WindowLocalClickTests(unittest.TestCase):
    """局部点击必须命中所属 HWND 的真实控件坐标，拒绝错误归属及不可用控件."""

    @staticmethod
    def control() -> Mock:
        control = Mock()
        control.handle = 0
        control.process_id.return_value = 501
        control.invoke.side_effect = gui_smoke.NoPatternInterfaceError
        control.rectangle.return_value = _ReviewRectangle(260, 180, 340, 220)
        control.is_visible.return_value = True
        control.is_enabled.return_value = True
        return control

    @staticmethod
    def dispatch(control: Mock) -> list[tuple[int, int, int, int]]:
        window = Mock(handle=100)
        events: list[tuple[int, int, int, int]] = []

        def post_message(handle: int, message: int, keys: int, point: int) -> None:
            events.append((handle, message, keys, point))

        with (
            patch("scripts.gui_smoke.time.sleep"),
            patch("scripts.gui_smoke.win32gui.ShowWindow"),
            patch("scripts.gui_smoke.win32process.GetWindowThreadProcessId", return_value=(1, 501)),
            patch("scripts.gui_smoke.win32gui.IsChild", return_value=False),
            patch("scripts.gui_smoke.win32gui.IsWindowVisible", return_value=True),
            patch("scripts.gui_smoke.win32gui.IsWindowEnabled", return_value=True),
            patch("scripts.gui_smoke.win32gui.GetClientRect", return_value=(0, 0, 640, 480)),
            patch(
                "scripts.gui_smoke.win32gui.ScreenToClient",
                side_effect=lambda _handle, point: (point[0] - 200, point[1] - 100),
            ),
            patch("scripts.gui_smoke.win32gui.PostMessage", side_effect=post_message),
        ):
            gui_smoke.click(cast("WindowSpecification", window), cast("WindowSpecification", control))
        return events

    def test_unavailable_invoke_targets_control_center_without_foreground_input(self) -> None:
        events = self.dispatch(self.control())
        point = gui_smoke.win32api.MAKELONG(100, 100)
        self.assertEqual(
            events,
            [
                (100, gui_smoke.win32con.WM_MOUSEMOVE, 0, point),
                (100, gui_smoke.win32con.WM_LBUTTONDOWN, gui_smoke.win32con.MK_LBUTTON, point),
                (100, gui_smoke.win32con.WM_LBUTTONUP, 0, point),
            ],
        )

    def test_invalid_owner_visibility_enablement_and_bounds_reject_local_click(self) -> None:
        for invalid in ("process", "foreign-hwnd", "hidden", "disabled", "outside", "empty"):
            control = self.control()
            if invalid == "process":
                control.process_id.return_value = 999
            elif invalid == "foreign-hwnd":
                control.handle = 200
            elif invalid == "hidden":
                control.is_visible.return_value = False
            elif invalid == "disabled":
                control.is_enabled.return_value = False
            elif invalid == "outside":
                control.rectangle.return_value = _ReviewRectangle(190, 180, 340, 220)
            else:
                control.rectangle.return_value = _ReviewRectangle(260, 180, 260, 220)
            with self.subTest(invalid=invalid), self.assertRaises(RuntimeError):
                _ = self.dispatch(control)


class _ReviewStageDriver:
    """仅替换进程/桌面边界，pre、真实阶段主体及 after 仍执行。."""

    def __init__(self, window: object) -> None:
        self.window = window

    def __call__(
        self,
        _tag: str,
        _exe: str,
        body: Callable[[WindowSpecification], None],
        *,
        pre: Callable[[], None] | None = None,
        after: Callable[[], None] | None = None,
        **_options: object,
    ) -> None:
        try:
            if pre is not None:
                pre()
            body(cast("WindowSpecification", self.window))
        finally:
            if after is not None:
                after()


class _ReviewLayoutWindow:
    handle = 0
    # 独立 UIA 场景，不从被测 LAYOUT_PAGES 生成，否则漏页会同步消失。
    pages: dict[str, tuple[str, ...]] = {
        "递归解压": ("开始解压",),
        "目录整理": ("开始分析",),
        "MD 整理": ("开始合并",),
        "转 Markdown": ("开始转换",),
        "Git 工具": ("开始",),
        "截图 OCR": (),
        "设置": ("下载 / 补齐音频转写资产", "检查已安装组件"),
        "关于": (),
        "模型服务": ("保存配置", "退出模型服务"),
        # 错误提示当前可见；正常无提示时不应无条件等待“关闭提示”。
        "CodeAgent": ("保存间隔", "关闭提示"),
    }

    def __init__(self, damaged_page: str = "", damage: str = "", *, show_hint: bool = True) -> None:
        self.page = "递归解压"
        self.bounds = _ReviewRectangle(0, 0, 960, 620)
        self.damaged_page = damaged_page
        self.damage = damage
        self.show_hint = show_hint
        self.modal_open = False
        self.service_running = True

    def rectangle(self) -> _ReviewRectangle:
        return self.bounds

    def resize(
        self, _handle: int, _after: int, _x: int, _y: int, width: int, height: int, _flags: int
    ) -> None:
        self.bounds = _ReviewRectangle(0, 0, width, height)

    def select_page(self, page: str) -> None:
        self.page = page

    def open_modal(self) -> None:
        self.modal_open = True

    def return_to_page(self) -> None:
        self.modal_open = False

    def stop_service(self) -> None:
        self.service_running = False

    def descendants(self, *, control_type: str) -> list[_ReviewControl]:
        if control_type == "Text":
            if self.modal_open:
                return [_ReviewControl("退出模型服务")]
            return [_ReviewControl("截图快捷键"), _ReviewControl("关于 JchTools")]
        if control_type != "Button":
            return []
        if self.modal_open:
            titles = ("返回检查", "确认退出模型服务")
            actions: tuple[Callable[[], None] | None, ...] = (self.return_to_page, self.stop_service)
            page = "confirm5"
            buttons: list[_ReviewControl] = []
        else:
            titles = self.pages[self.page]
            if self.page == "CodeAgent" and not self.show_hint:
                titles = ("保存间隔",)
            actions = tuple(self.open_modal if title == "退出模型服务" else None for title in titles)
            page = self.page
            buttons = [
                _ReviewControl(name, action=partial(self.select_page, name))
                for name in self.pages
            ]
        for index, (title, action) in enumerate(zip(titles, actions, strict=True)):
            left = 250 + index * 180
            bounds = _ReviewRectangle(left, 200, left + 160, 240)
            if page == self.damaged_page:
                if self.damage == "clipped" and index == 0:
                    bounds = _ReviewRectangle(250, 200, self.bounds.right + 1, 240)
                elif self.damage == "overlap" and index == 1:
                    bounds = _ReviewRectangle(370, 200, 530, 240)
            buttons.append(_ReviewControl(title, bounds, action=action))
        return buttons

    def child_window(self, *, title: str, control_type: str, found_index: int = 0) -> _ReviewControl:
        matching = [control for control in self.descendants(control_type=control_type) if control.title == title]
        if len(matching) <= found_index:
            message = f"内存 UIA 中不存在可见控件：{title}"
            raise gui_smoke.timings.TimeoutError(message)
        return matching[found_index]


class _ReviewLayoutService:
    """仅替换真实服务的进程/Job 边界，不改页面或模态行为。."""

    def __init__(self) -> None:
        self.reaped = False

    def poll(self) -> int | None:
        return 0 if self.reaped else None

    def close(self) -> None:
        self.reaped = True

    def wait(self, *, timeout: int) -> int:
        del timeout
        if not self.reaped:
            raise AssertionError("隔离服务必须先由所属 Job 收尾")
        return 0


class ReviewRemediationLayoutTests(unittest.TestCase):
    def run_layout(self, window: _ReviewLayoutWindow) -> None:
        clock = _ReviewClock()
        service = _ReviewLayoutService()

        def spawn(argv: list[str], *, env: dict[str, str]) -> _ReviewLayoutService:
            self.assertIs(env, stage.call_args.kwargs["env"], "GUI 与服务必须使用同一完整隔离环境")
            self.assertEqual(argv, ["unused.exe", "--acp-http-service"], "不允许启动真实 GUI 或 Agent")
            state = Path(env["JCHTOOLS_TEST_STATE_DIR"])
            self.assertTrue(state.is_absolute())
            self.assertNotEqual(str(state), ISOLATED_ENV["JCHTOOLS_TEST_STATE_DIR"])
            for key in (
                "JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT",
                "JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT",
                "JCHTOOLS_TEST_ASSET_ROOT",
                "JCHTOOLS_TEST_CODEAGENT_CREDENTIALS",
            ):
                self.assertTrue(Path(env[key]).is_relative_to(state), key)
            self.assertFalse(Path(env["JCHTOOLS_TEST_CODEAGENT_CREDENTIALS"]).exists(), "不得读取真实 Token")
            self.assertEqual(
                env["JCHTOOLS_TEST_CODEAGENT_REGISTRY"],
                rf"Software\JchTools\Tests\CodeAgent\{state.name}",
            )
            self.assertEqual(env["JCHTOOLS_TEST_ACP_SERVICE_EXE"], str(Path("unused.exe").resolve()))
            return service
        with (
            patch.object(gui_smoke, "run_stage", side_effect=_ReviewStageDriver(window)) as stage,
            patch.object(gui_smoke, "activate"),
            patch("scripts.gui_smoke.win32gui.SetWindowPos", side_effect=window.resize),
            patch("scripts.gui_smoke.time.time", side_effect=clock.read),
            patch("scripts.gui_smoke.time.sleep", side_effect=clock.sleep),
            patch.dict(
                os.environ,
                {
                    **ISOLATED_ENV,
                    "JCHTOOLS_TEST_CODEAGENT_CREDENTIALS": r"C:\real-profile\.cac\.credentials.json",
                    "JCHTOOLS_TEST_CODEAGENT_REGISTRY": "Environment",
                    "JCHTOOLS_TEST_ACP_SERVICE_EXE": r"C:\real-production\JchTools.exe",
                },
            ),
            patch("scripts.gui_smoke.subprocess.Popen", side_effect=spawn) as popen,
            patch.object(gui_smoke, "own_process_tree", return_value=service) as own_tree,
        ):
            try:
                gui_smoke.s18_layout_at_two_window_sizes("unused.exe")
            finally:
                self.assertTrue(service.reaped, "成功与几何失败都必须回收本轮独占服务")
                popen.assert_called_once()
                own_tree.assert_called_once_with(service)
                self.assertTrue(window.service_running, "布局验收不得确认退出模型服务")
                self.assertFalse(window.modal_open, "布局验收必须通过返回检查关闭 kind 5")

    def test_review_remediation_s18_rejects_clipping_on_new_pages_and_exit_modal(self) -> None:
        # P2-07 / U-05 / P-13：真正执行 S18，遗漏页面/模态将无法拒绝这些几何反例。
        for page in ("模型服务", "CodeAgent", "confirm5"):
            with self.subTest(page=page), self.assertRaisesRegex(RuntimeError, "裁切|移出窗口"):
                self.run_layout(_ReviewLayoutWindow(page, "clipped"))

    def test_review_remediation_s18_rejects_overlap_on_new_pages_and_exit_modal(self) -> None:
        # P2-07：CodeAgent 的关闭提示已可见；不要求无提示状态出现此按钮。
        for page in ("模型服务", "CodeAgent", "confirm5"):
            with self.subTest(page=page), self.assertRaisesRegex(RuntimeError, "重叠"):
                self.run_layout(_ReviewLayoutWindow(page, "overlap"))

    def test_review_remediation_s18_accepts_normal_geometry_without_stopping_service(self) -> None:
        for show_hint in (False, True):
            with self.subTest(show_hint=show_hint):
                self.run_layout(_ReviewLayoutWindow(show_hint=show_hint))


class _ReviewValueElement:
    def GetCurrentPropertyValue(self, _identifier: int) -> bool:  # noqa: N802 - UIA 协议方法名。
        return True


class _ReviewElementInfo:
    def __init__(self) -> None:
        self.element = _ReviewValueElement()


class _ReviewLogEdit(_ReviewControl):
    def __init__(self, value: str) -> None:
        super().__init__("")
        self.value = value
        self.element_info = _ReviewElementInfo()

    def get_value(self) -> str:
        return self.value


class _ReviewUiaDll:
    UIA_IsValuePatternAvailablePropertyId = 1


class _ReviewUia:
    UIA_dll = _ReviewUiaDll()


class _ReviewConversionWindow:
    def __init__(self, media_name: str, cancellation_log: str) -> None:
        self.media_name = media_name
        self.cancellation_log = cancellation_log
        self.output: Path | None = None
        self.running = False
        self.stopped = False

    def set_directories(self, _window: WindowSpecification, _input: str, output: str) -> None:
        self.output = Path(output)

    def start(self) -> None:
        if self.output is None:
            message = "尚未选择输出目录"
            raise AssertionError(message)
        _ = (self.output / "00_complete_txt.md").write_bytes(b"completed before cancellation\n")
        self.running = True

    def stop(self) -> None:
        self.running = False
        self.stopped = True

    def descendants(self, *, control_type: str) -> list[_ReviewControl]:
        if control_type == "Button":
            if self.running:
                return [_ReviewControl("停止任务", action=self.stop)]
            return [_ReviewControl("开始转换", action=self.start)]
        if control_type == "Text":
            return [_ReviewControl("已停止" if self.stopped else f"正在处理 01_{self.media_name}")]
        if control_type == "Edit":
            return [_ReviewLogEdit(self.cancellation_log if self.stopped else "")]
        return []

    def child_window(self, *, title: str, control_type: str, found_index: int = 0) -> _ReviewControl:
        return [control for control in self.descendants(control_type=control_type) if control.title == title][found_index]


class ReviewRemediationCancellationLogTests(unittest.TestCase):
    def run_s5(self, log: str) -> None:
        # P3-35 / T-23 / P-12：执行真实 S5 主体、产物判据及 wait_log_matching；
        # 只替换桌面/进程边界，不在测试中复制正则。
        media_name = "中文 示例 clip.mp4"
        clock = _ReviewClock()
        window = _ReviewConversionWindow(media_name, log)
        with tempfile.TemporaryDirectory(prefix="jchtools-review-s5-") as temporary:
            media = Path(temporary) / media_name
            _ = media.write_bytes(b"synthetic media; no decoder is launched")
            with (
                patch.dict(os.environ, {"JCHTOOLS_S5_MEDIA": str(media)}),
                patch.object(gui_smoke, "run_stage", side_effect=_ReviewStageDriver(window)),
                patch.object(gui_smoke, "goto_converter"),
                patch.object(gui_smoke, "set_converter_dirs", side_effect=window.set_directories),
                patch.object(gui_smoke, "activate"),
                patch.object(gui_smoke, "IUIA", side_effect=_ReviewUia),
                patch("scripts.gui_smoke.time.time", side_effect=clock.read),
                patch("scripts.gui_smoke.time.monotonic", side_effect=clock.read),
                patch("scripts.gui_smoke.time.sleep", side_effect=clock.sleep),
                patch("scripts.gui_smoke.subprocess.Popen", side_effect=AssertionError("禁止启动真实 GUI")),
            ):
                gui_smoke.s5_markdown_basic_chain("unused.exe")

    def test_review_remediation_s5_accepts_cancelled_media_with_chinese_and_spaces(self) -> None:
        self.run_s5("已取消：01_中文 示例 clip.mp4；未完成结果不提交\n已停止")

    def test_review_remediation_s5_rejects_stopped_without_a_cancellation_entry(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "只读日志超时"):
            self.run_s5("已停止\n没有文件被确认取消")

    def test_review_remediation_s5_rejects_cancellation_suffix_on_another_line(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "只读日志超时"):
            self.run_s5("已取消：01_中文 示例 clip.mp4\n其他事件；未完成结果不提交")


class _ReviewConfirmWindow:
    def __init__(self, observations: tuple[str, ...]) -> None:
        self.observations = iter(observations)
        self.last = "open"
        self.closed = False

    def wait_checkbox(self) -> None:
        if self.closed:
            message = "确认层已删除，旧复选框不可见"
            raise gui_smoke.timings.TimeoutError(message)

    def descendants(self, *, control_type: str) -> list[_ReviewControl]:
        if control_type == "Button":
            return [_ReviewControl("确认")]
        if control_type != "Text":
            return []
        self.last = next(self.observations, self.last)
        self.closed = self.last == "closed"
        if self.last == "error":
            message = "UIA 树正重建"
            raise gui_smoke.timings.TimeoutError(message)
        if self.last == "empty":
            return []
        return [_ReviewControl("后台任务页面" if self.closed else "确认文件处理范围")]

    def child_window(self, *, title: str, control_type: str, found_index: int = 0) -> _ReviewControl:
        if control_type == "CheckBox":
            return _ReviewControl(title, before_wait=self.wait_checkbox)
        return self.descendants(control_type=control_type)[found_index]


class ReviewRemediationConfirmationTests(unittest.TestCase):
    def confirm(self, window: _ReviewConfirmWindow, *, extraction: bool = False) -> None:
        clock = _ReviewClock()
        with (
            patch.object(gui_smoke, "activate"),
            patch("scripts.gui_smoke.time.time", side_effect=clock.read),
            patch("scripts.gui_smoke.time.sleep", side_effect=clock.sleep),
            patch("scripts.gui_smoke.subprocess.Popen", side_effect=AssertionError("禁止启动真实 GUI")),
        ):
            gui_smoke.confirm_dialog(cast("WindowSpecification", window), timeout=15, extraction=extraction)

    def test_review_remediation_confirm_closure_observations_cross_batch_boundary(self) -> None:
        # P3-38 / P-13：第 19 次仍 open，20、21 次 closed；第 20 次后旧 checkbox 消失。
        for extraction in (False, True):
            window = _ReviewConfirmWindow(("open",) * 19 + ("closed", "closed"))
            with self.subTest(extraction=extraction):
                self.confirm(window, extraction=extraction)
                self.assertTrue(window.closed)

    def test_review_remediation_confirm_unreadable_or_open_tree_cannot_report_success(self) -> None:
        # P3-38：空树、瞬态异常、始终打开均不能作为正常关闭的有效观察。
        for observation in ("empty", "error", "open"):
            with self.subTest(observation=observation), self.assertRaises(RuntimeError):
                self.confirm(_ReviewConfirmWindow((observation,)))

    def test_review_remediation_confirm_invalid_reading_breaks_consecutive_closure(self) -> None:
        # P3-38：单次有效关闭观察不得穿过空树、异常或重新出现的标题累积成成功。
        for observation in ("empty", "error", "open"):
            with self.subTest(observation=observation), self.assertRaises(RuntimeError):
                self.confirm(_ReviewConfirmWindow(("closed", observation) * 40))
