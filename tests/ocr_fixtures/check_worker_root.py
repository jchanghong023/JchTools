"""回归：发布 worker 必须从自身安装目录加载同一套已校验资产."""

from __future__ import annotations

import argparse
import ctypes
import hashlib
import json
import os
import shutil
import subprocess
import tempfile
import time
from pathlib import Path


def pipe_name(username: str) -> str:
    """按服务协议计算本次测试的命名管道."""
    session = ctypes.c_ulong()
    ok = ctypes.windll.kernel32.ProcessIdToSessionId(os.getpid(), ctypes.byref(session))
    if not ok:
        message = "无法读取 Windows 会话 ID"
        raise OSError(message)
    digest = hashlib.sha256(f"{username}:{session.value}".encode()).hexdigest()[:16]
    return rf"\\.\pipe\jchtools-snap-ocr-{digest}"


def request(pipe: str, command: str) -> dict[str, object]:
    """通过产品公开的本地控制协议发送一条请求."""
    with Path(pipe).open("r+b", buffering=0) as stream:
        stream.write(json.dumps({"command": command}).encode() + b"\n")
        return json.loads(stream.readline())


def wait_model(process: subprocess.Popen[bytes], pipe: str) -> dict[str, object]:
    """等待真实服务完成模型加载并返回可观察状态."""
    deadline = time.monotonic() + 30
    status: dict[str, object] | None = None
    while time.monotonic() < deadline:
        if process.poll() is not None:
            _stdout, stderr = process.communicate(timeout=1)
            message = f"worker 提前退出：{process.returncode}；{stderr.decode(errors='replace')}"
            raise RuntimeError(message)
        try:
            status = request(pipe, "ping")
        except OSError:
            time.sleep(0.2)
            continue
        if status.get("model") in ("ready", "error", "uninitialized"):
            return status
        time.sleep(0.2)
    message = "worker 模型加载状态未收敛"
    raise TimeoutError(message)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--asset-root", required=True, type=Path)
    parser.add_argument("--worker-version", required=True)
    args = parser.parse_args()
    source = args.asset_root.resolve(strict=True)
    temp_root = Path(__file__).resolve().parents[2] / ".tmp"
    temp_root.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="ocr-worker-root-", dir=temp_root) as directory:
        isolated = Path(directory)
        asset_root = isolated / "assets"
        shutil.copytree(source, asset_root)
        worker = asset_root / "worker" / args.worker_version / "snap-ocr-worker.exe"
        if not worker.is_file():
            message = f"worker 不存在：{worker}"
            raise FileNotFoundError(message)
        settings = json.dumps({"hotkey": "Ctrl+Shift+F12"})
        fallback_root = isolated / "local" / "JchTools" / "snap-ocr"
        fallback_root.mkdir(parents=True)
        (fallback_root / "settings.json").write_text(settings, encoding="utf-8")
        (asset_root / "settings.json").write_text(settings, encoding="utf-8")
        username = f"ocr_root_test_{os.getpid()}"
        env = os.environ | {"LOCALAPPDATA": str(isolated / "local"), "USERNAME": username}
        pipe = pipe_name(username)
        process = subprocess.Popen([str(worker), "--service"], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            status = wait_model(process, pipe)
            print(json.dumps({"model": status.get("model"), "ok": status.get("ok")}, ensure_ascii=False))
            assert status.get("model") == "ready", "worker 没有从自身安装目录加载模型"
        finally:
            try:
                request(pipe, "shutdown")
            except OSError:
                process.terminate()
            process.wait(timeout=15)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
