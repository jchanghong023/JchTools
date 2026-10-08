"""gui_smoke 纯函数回归单测（S11-06 完成判据与产物快照锚、S11-07 隔离守卫）.

只测纯函数与文件系统快照 helper，不启动 GUI、不写仓库目录（tempfile 例外）；
受 Bandit B101 管控的 assert 逐条附 nosec 说明。
"""

from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
import unittest
import zipfile
from pathlib import Path
from typing import TYPE_CHECKING, cast
from unittest.mock import patch

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
