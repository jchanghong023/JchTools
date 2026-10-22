"""S6-S9：设置页与共享运行目录阶段。.

由 gui_smoke.py 按域拆出；行为与拆分前一致，入口 scripts/gui_smoke.py 负责 re-export。
"""

from __future__ import annotations

import os
import shutil
import sys
import tempfile
import time
from pathlib import Path
from typing import TYPE_CHECKING

from pywinauto import timings

if not __package__:
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

if TYPE_CHECKING:
    from pywinauto.application import WindowSpecification

from scripts.gui_smoke_impl import config, convert_ui, driver, postconditions, records


def goto_settings(window: WindowSpecification) -> None:
    """切到设置页：等待设置专属离线检查控件，兼容下载失败后的重试按钮."""
    for _attempt in range(5):
        driver.click(window, driver.find_button(window, "设置"))
        try:
            _ = driver.find_button(window, config.SETTINGS_CHECK_BUTTON).wait("visible", timeout=config.ATTEMPT_TIMEOUT)
        except (RuntimeError, timings.TimeoutError):
            continue
        else:
            return
    message = "点击「设置」后始终未出现设置页专属的离线检查控件"
    raise RuntimeError(message)


def wait_bundled_settings(state: Path, runtime: Path) -> dict[str, str]:
    deadline = time.monotonic() + config.TIMEOUT
    actual: dict[str, str] = {}
    while time.monotonic() < deadline:
        actual = records.active_xberg_settings(state)
        if actual.get("xberg_source") == "bundled" and all(
            records.same_runtime_path(actual.get(key), runtime)
            for key in ("xberg_directory", "xberg_bundled_directory")
        ):
            return actual
        time.sleep(0.1)
    records.verify_bundled_settings(actual, runtime)
    return actual


def provision_bundled_runtime(exe: str, state: Path) -> tuple[Path, dict[str, str]]:
    """仅复制明确预置/打包的真实基础包；从不下载，也不写源包或用户安装目录."""
    configured = os.environ.get("JCHTOOLS_TEST_BUNDLED_XBERG_DIR")
    source = Path(records.plain_windows_path(configured)) if configured else Path(exe).resolve().parent / "xberg"
    driver.require(
        source.is_absolute() and source.is_dir(),
        "S6-S8 需要预置真实随包基础目录 JCHTOOLS_TEST_BUNDLED_XBERG_DIR",
    )
    before = postconditions.verify_bundled_package(source)
    runtime = state / "xberg"
    _ = shutil.copytree(source, runtime)
    driver.require(postconditions.snapshot_output_bytes(runtime) == before, "隔离基础包复制不完整")
    return runtime, before


def wait_legacy_runtime(state: Path, directory: str) -> None:
    """等待旧文本迁移的有效活动路径；供外部预置引擎阶段使用."""
    deadline = time.monotonic() + config.TIMEOUT
    while time.monotonic() < deadline:
        actual = records.read_xberg_settings(state).get("xberg_directory")
        if actual and Path(records.plain_windows_path(actual)).resolve() == Path(directory).resolve():
            return
        time.sleep(0.1)
    message = f"旧配置未迁移或活动 Xberg 目录被改变：{directory}"
    raise RuntimeError(message)


def wait_base_ready(window: WindowSpecification, state: Path, runtime: Path) -> dict[str, str]:
    """观察真实 GUI 的基础/音频独立状态，并确认同一个实际活动来源."""
    goto_settings(window)
    settings = wait_bundled_settings(state, runtime)
    for label in config.SETTINGS_BASE_LABELS:
        _ = convert_ui.wait_text_containing(window, f"{label}：已安装")
    for label in ("FFmpeg 及配套运行库", "SenseVoice / VAD 音频模型"):
        _ = convert_ui.wait_text_containing(window, f"{label}：未下载")
    _ = convert_ui.wait_text_containing(window, "音视频转写：未就绪，请主动下载音频资产")
    _ = convert_ui.wait_text_containing(window, f"当前共享目录：{runtime}")
    _ = driver.find_button(window, config.SETTINGS_DOWNLOAD_BUTTON).wait("visible enabled", timeout=config.TIMEOUT)
    driver.require(not driver.find_button(window, "取消下载").exists(), "未主动操作时不得自动下载音频")
    return settings


def s6_bundled_readiness_and_active_source(exe: str) -> None:
    """S6 真实随包文档/截图就绪、可选音频缺失，并持久化唯一活动来源."""
    with tempfile.TemporaryDirectory(prefix="s6-", dir=records.settings_stage_workspace()) as temporary:
        state = Path(temporary)
        runtime, before = provision_bundled_runtime(exe, state)

        def body(window: WindowSpecification) -> None:
            _ = wait_base_ready(window, state, runtime)
            driver.require(postconditions.snapshot_output_bytes(runtime) == before, "启动就绪检查不得修改随包基础资产")

        driver.run_stage("S6", exe, body, env=records.isolated_state_env(state, runtime))
        records.verify_bundled_settings(records.active_xberg_settings(state), runtime)
        driver.require(postconditions.verify_bundled_package(runtime) == before, "退出后基础包必须保持不变")
    print("S6 PASS：真实随包基础资产就绪、无默认音频，GUI 与 SQLite 活动来源一致且持久保存")


def s7_restart_restores_active_runtime(exe: str) -> None:
    """S7 从有效旧文本路径迁移后重启，保留旧目录/原件并恢复相同活动来源."""
    with tempfile.TemporaryDirectory(prefix="s7-", dir=records.settings_stage_workspace()) as temporary:
        state = Path(temporary)
        runtime, before = provision_bundled_runtime(exe, state)
        legacy_runtime = state / "legacy-runtime"
        _ = shutil.copytree(runtime, legacy_runtime)
        directory = records.seed_legacy_runtime(state, legacy_runtime)
        env = records.isolated_state_env(state, runtime)

        def activated(window: WindowSpecification) -> None:
            current = wait_base_ready(window, state, runtime)
            driver.require(
                records.same_runtime_path(current.get("xberg_custom_directory"), legacy_runtime),
                "随包迁移必须保留旧有效目录的来源记录",
            )

        def restored(window: WindowSpecification) -> None:
            _ = wait_base_ready(window, state, runtime)
            driver.require(records.active_xberg_settings(state) == saved_runtime, "重启不得改变活动来源或历史目录记录")

        driver.run_stage("S7-first", exe, activated, env=env)
        saved_runtime = records.active_xberg_settings(state)
        driver.run_stage("S7", exe, restored, env=env)
        driver.require(records.active_xberg_settings(state) == saved_runtime, "退出后活动目录与来源必须保持不变")
        legacy = state / "markdown-assets" / "xberg-runtime-path.txt"
        driver.require(legacy.read_text(encoding="utf-8") == directory, "旧路径原件必须保留")
        driver.require(postconditions.snapshot_output_bytes(legacy_runtime) == before, "迁移不得改写旧运行目录")
        driver.require(postconditions.verify_bundled_package(runtime) == before, "无兼容音频时迁移不得改动基础包")
    print("S7 PASS：重启恢复同一随包活动目录，旧来源、旧目录字节与路径原件保留")


def wait_download_barrier(marker: Path) -> None:
    """只有真实 audio API 到达 test-hooks 预网络屏障后才能确认启动下载."""
    deadline = time.monotonic() + config.TIMEOUT
    while not marker.is_file() and time.monotonic() < deadline:
        time.sleep(0.1)
    driver.require(marker.is_file(), "音频下载未进入受控预网络屏障；不能把点击成功当作下载已启动")


def s8_explicit_audio_download_cancel_and_retry(exe: str) -> None:
    """S8 真实音频动作与取消；test-hooks 在网络前拦住，绝不下载真实发布资产."""
    with tempfile.TemporaryDirectory(prefix="s8-", dir=records.settings_stage_workspace()) as temporary:
        state = Path(temporary)
        runtime, before = provision_bundled_runtime(exe, state)
        marker = state / "audio-download-barrier"
        env = records.isolated_state_env(state, runtime)
        env["JCHTOOLS_TEST_AUDIO_DOWNLOAD_BARRIER"] = str(marker)

        def body(window: WindowSpecification) -> None:
            original = wait_base_ready(window, state, runtime)
            driver.require(not marker.exists(), "启动/进入设置不得隐式调用音频下载")
            notice = state / "markdown-assets" / "licenses" / "THIRD_PARTY_NOTICES.md"
            notice.unlink(missing_ok=True)
            driver.click(window, driver.find_button(window, config.SETTINGS_CHECK_BUTTON))
            deadline = time.monotonic() + config.COMPLETION_TIMEOUT
            while not notice.is_file() and time.monotonic() < deadline:
                time.sleep(0.1)
            driver.require(notice.is_file(), "真实离线检查必须生成许可证 notice，不能把未生效的点击当作检查完成")
            _ = driver.find_button(window, config.SETTINGS_CHECK_BUTTON).wait(
                "visible enabled", timeout=config.COMPLETION_TIMEOUT
            )
            driver.require(not marker.exists(), "离线检查不得调用音频下载")
            driver.require(records.active_xberg_settings(state) == original, "离线检查不得切换活动目录")
            for caption in (config.SETTINGS_DOWNLOAD_BUTTON, config.SETTINGS_RETRY_BUTTON):
                button = driver.find_button(window, caption)
                _ = button.wait("visible enabled", timeout=config.TIMEOUT)
                driver.click(window, button)
                wait_download_barrier(marker)
                cancel = driver.find_button(window, "取消下载")
                _ = cancel.wait("visible enabled", timeout=config.TIMEOUT)
                driver.click(window, cancel)
                _ = driver.find_button(window, config.SETTINGS_RETRY_BUTTON).wait(
                    "visible enabled", timeout=config.CONVERT_STOP_TIMEOUT
                )
                _ = convert_ui.wait_text_containing(window, "已取消", timeout=config.CONVERT_STOP_TIMEOUT)
                driver.require(records.active_xberg_settings(state) == original, "取消音频下载不得改动活动配置")
                driver.require(
                    postconditions.snapshot_output_bytes(runtime) == before,
                    "取消音频下载不得改动基础包或留下暂存文件",
                )
                marker.unlink()
            driver.require(not driver.find_button(window, "取消下载").exists(), "取消完成后不得仍处于下载状态")

        driver.run_stage("S8", exe, body, env=env)
        driver.require(not marker.exists(), "退出不得隐式重启音频下载")
        driver.require(postconditions.verify_bundled_package(runtime) == before, "受控取消后基础包必须完整且没有音频")
    print("S8 PASS：仅显式动作进入预网络屏障；取消与重试真实生效，配置/基础文件不变（未连接真实网络）")


def s9_missing_base_requires_install_repair(exe: str) -> None:
    """S9 故意缺失 bundle；转换/音频禁用，离线检查给出安装修复而非基础下载."""
    with tempfile.TemporaryDirectory(prefix="s9-", dir=records.settings_stage_workspace()) as temporary:
        state = Path(temporary)
        env = records.isolated_state_env(state)
        marker = state / "audio-download-barrier"
        env["JCHTOOLS_TEST_AUDIO_DOWNLOAD_BARRIER"] = str(marker)

        def body(window: WindowSpecification) -> None:
            _ = convert_ui.click_and_wait_text(window, "转 Markdown", config.CONVERT_NEED_RUNTIME_TEXT)
            start = driver.find_button(window, "开始转换")
            _ = start.wait("visible", timeout=config.TIMEOUT)
            driver.require(not start.is_enabled(), "缺失基础包时「开始转换」必须禁用")
            _ = driver.find_button(window, "前往设置").wait("visible", timeout=config.TIMEOUT)
            goto_settings(window)
            for label in config.SETTINGS_BASE_LABELS:
                _ = convert_ui.wait_text_containing(window, f"{label}：未找到运行目录")
            _ = convert_ui.wait_text_containing(window, "当前共享目录：未找到，请修复安装包或重新解包")
            _ = convert_ui.wait_text_containing(window, "重新解包便携 ZIP")
            download = driver.find_button(window, config.SETTINGS_DOWNLOAD_BUTTON)
            _ = download.wait("visible", timeout=config.TIMEOUT)
            driver.require(not download.is_enabled(), "缺失基础包时不得启用可选音频下载")
            driver.click(window, driver.find_button(window, config.SETTINGS_CHECK_BUTTON))
            _ = convert_ui.wait_text_containing(window, "未找到有效的 Xberg 运行目录：")
            _ = driver.find_button(window, config.SETTINGS_CHECK_BUTTON).wait("visible enabled", timeout=config.TIMEOUT)
            driver.require(not download.is_enabled(), "离线检查失败不得解除音频下载门控")
            driver.require(not driver.find_button(window, "取消下载").exists(), "缺失基础包不得自动启动下载")
            driver.require(not records.active_xberg_settings(state), "缺失包不得伪造活动 Xberg 来源")
            driver.require(not marker.exists(), "缺失包/离线检查不得调用音频下载")
            driver.require(
                not postconditions.snapshot_output_bytes(state / "markdown-assets"),
                "缺失基础包不得下载或初始化 Markdown 资产",
            )
            driver.require(not Path(env["JCHTOOLS_TEST_BUNDLED_XBERG_DIR"]).exists(), "检查不得悄悄创建/下载基础包")

        driver.run_stage("S9", exe, body, env=env)
    print("S9 PASS：缺失基础包时转换与音频禁用，离线检查如实失败并提供安装修复/解包指引，无自动下载")
