"""回归：发布 worker 必须从自身安装目录加载同一套已校验资产."""

from __future__ import annotations

import argparse
import contextlib
import ctypes
import hashlib
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import TYPE_CHECKING, cast

if TYPE_CHECKING:
    from collections.abc import Callable, Generator

ROOT = Path(__file__).resolve().parents[2]
# 独立脚本的 sys.path 起点是 tests/ocr_fixtures；复用同仓 GUI 验收的进程所有权设施。
sys.path.insert(0, str(ROOT))
from scripts.gui_smoke import own_process_tree  # noqa: E402


def process_id_to_session_id() -> Callable[[int, object], int]:
    """返回 Windows 会话查询函数的明确类型."""
    return cast("Callable[[int, object], int]", ctypes.windll.kernel32.ProcessIdToSessionId)


def pipe_name(asset_root: Path) -> str:
    """按 test-hooks 服务协议计算隔离测试命名管道."""
    session = ctypes.c_ulong()
    ok = process_id_to_session_id()(os.getpid(), ctypes.byref(session))
    if not ok:
        message = "无法读取 Windows 会话 ID"
        raise OSError(message)
    digest = hashlib.sha256(b"test:" + os.fsencode(str(asset_root))).hexdigest()[:16]
    return rf"\\.\pipe\jchtools-snap-ocr-test-{digest}"


def legacy_pipe_name(username: str) -> str:
    """兼容未启用 test-hooks 的已构建 worker；仍使用本测试隔离用户名."""
    session = ctypes.c_ulong()
    ok = process_id_to_session_id()(os.getpid(), ctypes.byref(session))
    if not ok:
        message = "无法读取 Windows 会话 ID"
        raise OSError(message)
    digest = hashlib.sha256(f"{username}:{session.value}".encode()).hexdigest()[:16]
    return rf"\\.\pipe\jchtools-snap-ocr-{digest}"


def request(pipe: str, command: str) -> dict[str, object]:
    """通过产品公开的本地控制协议发送一条请求."""
    with Path(pipe).open("r+b", buffering=0) as stream:
        _ = stream.write(json.dumps({"command": command}).encode() + b"\n")
        return cast("dict[str, object]", json.loads(stream.readline()))


def wait_model(process: subprocess.Popen[bytes], pipes: tuple[str, ...]) -> tuple[dict[str, object], str]:
    """等待真实服务完成模型加载并返回可观察状态."""
    deadline = time.monotonic() + 30
    status: dict[str, object] | None = None
    while time.monotonic() < deadline:
        if process.poll() is not None:
            _stdout, stderr = process.communicate(timeout=1)
            message = f"worker 提前退出：{process.returncode}；{stderr.decode(errors='replace')}"
            raise RuntimeError(message)
        for pipe in pipes:
            try:
                status = request(pipe, "ping")
            except OSError:
                continue
            if status.get("model") in ("ready", "error", "uninitialized"):
                return status, pipe
        time.sleep(0.2)
    message = "worker 模型加载状态未收敛"
    raise TimeoutError(message)


def broker_pipe_name(state_root: Path) -> str:
    """计算隔离 state 目录对应的共享 Xberg broker 管道名."""
    session = ctypes.c_ulong()
    ok = process_id_to_session_id()(os.getpid(), ctypes.byref(session))
    if not ok:
        message = "无法读取 Windows 会话 ID"
        raise OSError(message)
    digest = hashlib.sha256(f"{state_root}:{session.value}".encode()).hexdigest()
    return rf"\\.\pipe\jchtools-xberg-{digest}"


def broker_request(state_root: Path, command: str) -> dict[str, object]:
    """向本测试创建的 broker 发送受控状态/停止请求."""
    with Path(broker_pipe_name(state_root)).open("r+b", buffering=0) as stream:
        _ = stream.write(json.dumps({"request": {"id": "ocr-worker-root-test", "command": command}}).encode() + b"\n")
        return cast("dict[str, object]", json.loads(stream.readline()))


def stop_broker(state_root: Path) -> None:
    """等待本测试隔离 broker 安全停止，不终止其他用户任务."""
    deadline = time.monotonic() + 15
    sent_stop = False
    while time.monotonic() < deadline:
        try:
            response = broker_request(state_root, "broker-state" if sent_stop else "broker-stop")
        except OSError:
            return
        sent_stop = True
        if response.get("running") is False:
            return
        time.sleep(0.2)
    message = "隔离 Xberg broker 未在安全停止期限内退出"
    raise TimeoutError(message)


def write_isolated_settings(state_root: Path, engine_root: Path) -> None:
    """创建本测试专用 SQLite 配置，不读取或修改用户生产数据库."""
    _ = state_root.mkdir(parents=True, exist_ok=True)
    schema = Path(__file__).resolve().parents[2] / "src" / "app_settings.sql"
    database = state_root / "config.sqlite3"
    # sqlite3 的事务 context manager 不会关闭连接；Windows 清理前必须释放句柄。
    with contextlib.closing(sqlite3.connect(database)) as connection:
        _ = connection.executescript(schema.read_text(encoding="utf-8"))
        _ = connection.execute(
            "INSERT OR REPLACE INTO app_settings(key, value) VALUES('xberg_directory', ?)",
            (str(engine_root),),
        )
        _ = connection.commit()


def copy_font_root(source: Path, asset_root: Path) -> None:
    """独立复制结果窗字体根，避免测试偶然依赖源资产树的目录句柄."""
    source_fonts = source / "fonts"
    target_fonts = asset_root / "fonts"
    if not source_fonts.is_dir():
        message = f"字体目录不存在：{source_fonts}"
        raise FileNotFoundError(message)
    _ = shutil.copytree(source_fonts, target_fonts)


@contextlib.contextmanager
def owned_test_directory(temp_root: Path) -> Generator[str, None, None]:
    """进程已退出后，等待 Windows 释放本轮文件占用；清理超时仍明确失败。."""
    temporary = tempfile.TemporaryDirectory(prefix="ocr-worker-root-", dir=temp_root)
    try:
        yield temporary.name
    finally:
        deadline = time.monotonic() + 15
        while True:
            try:
                temporary.cleanup()
            except PermissionError:
                if time.monotonic() >= deadline:
                    raise
                time.sleep(0.1)
            else:
                break


def run_worker_root(source: Path, worker_version: str, engine_root: Path) -> None:
    """在隔离目录中启动并安全清理本次 worker 与 broker."""
    temp_root = Path(__file__).resolve().parents[2] / ".tmp"
    _ = temp_root.mkdir(exist_ok=True)
    with owned_test_directory(temp_root) as directory:
        isolated = Path(directory)
        asset_root = isolated / "assets"
        _ = shutil.copytree(source, asset_root, ignore=shutil.ignore_patterns("fonts"))
        copy_font_root(source, asset_root)
        worker = asset_root / "worker" / worker_version / "snap-ocr-worker.exe"
        if not worker.is_file():
            message = f"worker 不存在：{worker}"
            raise FileNotFoundError(message)
        settings = json.dumps({"hotkey": "Ctrl+Shift+F12"})
        local_appdata = isolated / "local"
        state_root = local_appdata / "JchTools" / "data"
        write_isolated_settings(state_root, engine_root)
        _ = (asset_root / "settings.json").write_text(settings, encoding="utf-8")
        username = f"ocr_root_test_{os.getpid()}"
        env = os.environ | {
            "LOCALAPPDATA": str(local_appdata),
            "USERNAME": username,
            "JCHTOOLS_TEST_STATE_DIR": str(state_root),
            "JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT": str(asset_root),
        }
        pipes = (pipe_name(asset_root), legacy_pipe_name(username))
        process = subprocess.Popen([str(worker), "--service"], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        owner = None
        try:
            owner = own_process_tree(process)
            status, _pipe = wait_model(process, pipes)
            print(json.dumps({"model": status.get("model"), "ok": status.get("ok")}, ensure_ascii=False))
            assert status.get("model") == "ready", "worker 没有从自身安装目录加载模型"
        finally:
            try:
                stop_broker(state_root)
            finally:
                try:
                    if process.poll() is None:
                        _ = process.terminate()
                    try:
                        _ = process.wait(timeout=15)
                    except subprocess.TimeoutExpired:
                        _ = process.kill()
                        _ = process.wait(timeout=5)
                finally:
                    if owner is not None:
                        owner.close()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    _ = parser.add_argument("--asset-root", required=True, type=Path)
    _ = parser.add_argument("--worker-version", required=True)
    args = parser.parse_args()
    asset_argument = cast("Path", args.asset_root)
    worker_version = cast("str", args.worker_version)
    source = asset_argument.resolve(strict=True)
    engine_value = os.environ.get("JCHTOOLS_TEST_XBERG_DIR")
    if not engine_value:
        message = "必须通过 JCHTOOLS_TEST_XBERG_DIR 提供隔离 Xberg 测试目录"
        raise RuntimeError(message)
    engine_root = Path(engine_value).resolve(strict=True)
    if not (engine_root / "xberg.exe").is_file():
        message = f"隔离 Xberg 测试目录缺少 xberg.exe：{engine_root}"
        raise FileNotFoundError(message)
    run_worker_root(source, worker_version, engine_root)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
