"""S15-S18：MD 合并拆分、Git 提交推送、离线组件检查与两档窗口布局。.

由 gui_smoke.py 按域拆出；行为与拆分前一致，入口 scripts/gui_smoke.py 负责 re-export。
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import TYPE_CHECKING

if not __package__:
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

if TYPE_CHECKING:
    from pywinauto.application import WindowSpecification

from scripts.gui_smoke_impl import config, convert_ui, driver, postconditions, records, stages_convert, stages_settings


def s15_markdown_merge_and_split(exe: str) -> None:
    """覆盖 M-01/M-04/M-08/M-10：真实页面到可核对的合并与拆分产物."""
    with tempfile.TemporaryDirectory(prefix="jchtools-gui-md-") as temporary:
        scratch = Path(temporary)
        source, merged, split = (scratch / name for name in ("input", "merged", "split"))
        for directory in (source, merged, split):
            directory.mkdir()
        originals = {"a.md": b"# Alpha\nfirst\n", "b.md": "# 中文\n第二份\n".encode()}
        for name, content in originals.items():
            _ = (source / name).write_bytes(content)
        split_source = scratch / "document.md"
        original = ("中文与 UTF-8 分片测试\n" * 150).encode()
        _ = split_source.write_bytes(original)

        def body(window: WindowSpecification) -> None:
            driver.click(window, driver.find_button(window, "MD 整理"))
            rows = convert_ui.unique_visible_buttons(window, "选择目录…")
            driver.require(len(rows) == config.CONVERT_DIR_ROWS, "合并页应有输入与输出目录两行")
            for button, directory in zip(rows, (source, merged), strict=True):
                convert_ui.set_edit_before_button(window, button, str(directory))
            driver.click(window, driver.find_button(window, "开始合并"))
            _ = convert_ui.wait_text_containing(window, "合并完成")
            result = (merged / "merged.md").read_text(encoding="utf-8")
            driver.require("# a.md" in result and "## Alpha" in result, "合并必须保留文件标题并下移正文标题")
            driver.require("# b.md" in result and "## 中文" in result, "中文 Markdown 必须正常合并")
            for name, content in originals.items():
                driver.require((source / name).read_bytes() == content, "合并不得改写输入")
            driver.click(window, window.child_window(title="拆分 MD", control_type="TabItem"))
            files = convert_ui.unique_visible_buttons(window, "选择文件…")
            directories = convert_ui.unique_visible_buttons(window, "选择目录…")
            driver.require(len(files) == 1 and len(directories) == 1, "拆分页应有文件与输出目录入口")
            convert_ui.set_edit_before_button(window, files[0], str(split_source))
            convert_ui.set_edit_before_button(window, directories[0], str(split))
            driver.click(window, driver.find_button(window, "开始拆分"))
            _ = convert_ui.wait_text_containing(window, "拆分完成")
            pieces = sorted(split.glob("document_*.md"))
            driver.require(len(pieces) > 1, "1 KB 上限应产生多个分片")
            driver.require(b"".join(path.read_bytes() for path in pieces) == original, "分片拼接必须逐字节还原")
            driver.require(
                all(path.stat().st_size <= config.MD_SPLIT_LIMIT_BYTES for path in pieces),
                "分片不得超过 1 KB",
            )
            driver.require(split_source.read_bytes() == original, "拆分不得改写原文件")

        driver.run_stage("S15", exe, body)
    print("S15 PASS：MD 合并与拆分产物正确，原文件不变，UTF-8 分片可无损拼接")


def s16_git_commit_and_push(exe: str) -> None:
    """覆盖 G-01/G-04/G-05/G-13：真实 GUI 到本地 bare 远端的逐文件提交推送."""
    git = shutil.which("git")
    driver.require(git is not None, "Git GUI 验证需要可用 git")
    if git is None:
        return
    with tempfile.TemporaryDirectory(prefix="jchtools-gui-git-") as temporary:
        scratch = Path(temporary)
        repo, remote = scratch / "repo", scratch / "remote.git"
        repo.mkdir()

        def git_output(directory: Path, *args: str) -> str:
            result = subprocess.run([git, *args], cwd=directory, capture_output=True, text=True, check=True)
            return result.stdout.strip()

        _ = git_output(scratch, "init", "--bare", str(remote))
        _ = git_output(repo, "init", "--initial-branch=main")
        _ = git_output(repo, "config", "user.name", "Synthetic GUI test")
        _ = git_output(repo, "config", "user.email", "gui-test@local")
        _ = (repo / "one.txt").write_text("initial\n", encoding="utf-8")
        _ = git_output(repo, "add", "one.txt")
        _ = git_output(repo, "commit", "-m", "initial synthetic fixture")
        _ = git_output(repo, "remote", "add", "origin", str(remote))
        _ = git_output(repo, "push", "--set-upstream", "origin", "main")
        _ = (repo / "one.txt").write_text("changed\n", encoding="utf-8")
        _ = (repo / "two.txt").write_text("second\n", encoding="utf-8")

        def body(window: WindowSpecification) -> None:
            driver.click(window, driver.find_button(window, "Git 工具"))
            driver.set_directory(window, str(repo))
            driver.click(window, driver.find_button(window, "开始"))
            _ = convert_ui.wait_text_containing(window, "全部完成：2 / 2")
            driver.require(git_output(repo, "status", "--porcelain") == "", "GUI 操作后工作树必须干净")
            driver.require(
                git_output(repo, "rev-parse", "HEAD") == git_output(remote, "rev-parse", "main"), "远端必须收到全部提交"
            )
            for commit in ("HEAD", "HEAD~1"):
                files = git_output(repo, "diff-tree", "--no-commit-id", "--name-only", "-r", commit).splitlines()
                driver.require(len(files) == 1, "每个 GUI 提交只能包含一个文件")
            driver.require(git_output(remote, "show", "main:one.txt") == "changed", "远端修改内容必须正确")
            driver.require(git_output(remote, "show", "main:two.txt") == "second", "远端新增内容必须正确")

        driver.run_stage("S16", exe, body)
    print("S16 PASS：GUI 逐文件提交并推送成功，本地工作树与远端内容正确")


def s17_check_configured_components_offline(exe: str) -> None:
    """覆盖 T-05/T-06：预置真实引擎经离线检查补写 notice，并建立真实转换就绪状态."""
    engine_dir = records.plain_windows_path(os.environ.get("JCHTOOLS_TEST_XBERG_DIR", "").strip())
    driver.require(
        bool(engine_dir) and Path(engine_dir, "xberg.exe").is_file(),
        "S17 需要 JCHTOOLS_TEST_XBERG_DIR 指向含 xberg.exe 的真实最新测试引擎目录",
    )
    state = Path(os.environ.get("JCHTOOLS_TEST_STATE_DIR", ""))
    asset_root = Path(os.environ.get("JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT", ""))
    driver.require(state.is_absolute() and asset_root.is_absolute(), "S17 需要绝对隔离状态目录与 Markdown 资产根")
    notice = asset_root / "licenses" / "THIRD_PARTY_NOTICES.md"
    driver.require(not notice.exists(), "S17 需要新隔离资产根，以观察离线检查实际创建 notice")
    original = records.active_xberg_settings(state)
    before = postconditions.snapshot_output_bytes(asset_root)
    with tempfile.TemporaryDirectory(prefix="jchtools-gui-smoke-s17-") as temporary:
        scratch = Path(temporary)
        source, output = scratch / "input", scratch / "output"
        source.mkdir()
        output.mkdir()
        _ = (source / "ready.txt").write_text("真实引擎离线检查夹具\n", encoding="utf-8")

        def body(window: WindowSpecification) -> None:
            stages_settings.wait_legacy_runtime(state, engine_dir)
            stages_settings.goto_settings(window)
            check = driver.find_button(window, config.SETTINGS_CHECK_BUTTON)
            _ = check.wait("visible enabled", timeout=config.TIMEOUT)
            driver.click(window, check)
            deadline = time.monotonic() + config.COMPLETION_TIMEOUT
            while not notice.is_file() and time.monotonic() < deadline:
                time.sleep(0.1)
            driver.require(notice.is_file(), "离线检查必须由生产路径创建许可证 notice")
            driver.require("Xberg" in notice.read_text(encoding="utf-8"), "许可证 notice 必须包含真实引擎的许可说明")
            _ = check.wait("visible enabled", timeout=config.TIMEOUT)
            _ = convert_ui.click_and_wait_text(window, "转 Markdown", "已安装组件就绪，可离线使用")
            stages_convert.set_converter_dirs(window, str(source), str(output))
            _ = convert_ui.wait_button(window, "开始转换", config.COMPLETION_TIMEOUT, enabled=True)
            driver.require(records.active_xberg_settings(state) == original, "离线检查不得切换活动 Xberg 配置")
            after = postconditions.snapshot_output_bytes(asset_root)
            driver.require(
                set(after) == set(before) | {str(notice.relative_to(asset_root))}
                and all(after[name] == digest for name, digest in before.items()),
                "离线检查只能补写 notice，不得下载组件或改动已有资产",
            )

        driver.run_stage("S17", exe, body)
    print("S17 PASS：真实引擎离线检查创建 notice，转换就绪且开始操作可用，未下载组件")


def s18_layout_at_two_window_sizes(exe: str) -> None:
    """覆盖 U-05/P-13/CA-01：隔离后台下检查两档页面及 kind 5，返回检查而不退出服务。."""
    service: subprocess.Popen[bytes] | None = None
    service_owner: driver.OwnedProcessTree | None = None

    def cleanup_service() -> None:
        if service is None:
            return
        try:
            if service_owner is not None:
                service_owner.close()
            elif service.poll() is None:
                service.kill()
        finally:
            _ = service.wait(timeout=15)

    def body(window: WindowSpecification) -> None:
        nonlocal service, service_owner
        # driver.run_stage 先确认本轮 GUI 的 SQLite 与隔离启动日志，才允许启动真实独占服务。
        # 空配置服务无需 Agent 或模型即可提供真实控制状态和退出确认入口。
        service, service_owner = driver.start_owned_process([exe, "--acp-http-service"], env=env)
        postconditions.verify_layout_at_two_sizes(window)
        driver.require(service.poll() is None, "S18 返回检查后真实隔离模型服务必须保持存活")

    with tempfile.TemporaryDirectory(
        prefix="jchtools-gui-smoke-s18-", dir=records.settings_stage_workspace()
    ) as temporary:
        scratch = Path(temporary).resolve()
        env = records.isolated_state_env(scratch)
        # 不继承真实或其他阶段的凭据/注册表；缺失凭据只能产生安全的读取失败。
        env["JCHTOOLS_TEST_CODEAGENT_CREDENTIALS"] = str(scratch / "absent-credentials.json")
        env["JCHTOOLS_TEST_CODEAGENT_REGISTRY"] = rf"Software\JchTools\Tests\CodeAgent\{scratch.name}"
        env["JCHTOOLS_TEST_ACP_SERVICE_EXE"] = str(Path(exe).resolve())
        driver.run_stage("S18", exe, body, env=env, after=cleanup_service)
    print("S18 PASS：两档尺寸覆盖全部页面及退出确认，按钮无裁切/重叠，返回检查未停止服务（非全面视觉验收）")
