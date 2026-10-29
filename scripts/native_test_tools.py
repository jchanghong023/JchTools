#!/usr/bin/env python3
"""安装并定位本地测试使用的私有 cargo-affected 工具."""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

if not __package__:
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

ROOT = Path(__file__).resolve().parent.parent
PREFIX = ROOT / ".tmp" / "native-tools" / "cargo-affected"
BIN_DIR = PREFIX / "bin"
PATCH = ROOT / "scripts" / "patches" / "cargo-affected-nextest.patch"
UPSTREAM = "https://github.com/max-sixty/cargo-affected.git"
REVISION = "08000ff968eebb238a26a3a4de38fe314e8ca05e"
VERSION = "0.4.1+jchtools.4"


def _installed_version_matches() -> bool:
    """只检查私有可执行文件的真实版本，不查找全局工具."""
    executable = BIN_DIR / ("cargo-affected.exe" if os.name == "nt" else "cargo-affected")
    try:
        result = subprocess.run([str(executable), "--version"], capture_output=True, text=True, check=False)
    except OSError:
        return False
    return result.returncode == 0 and result.stdout.strip() == f"cargo-affected {VERSION}"


def affected_environment() -> dict[str, str]:
    """提供私有工具优先的 PATH，缺失或版本不符时明确要求安装."""
    if not _installed_version_matches():
        msg = f"私有 cargo-affected 缺失或版本不符；请运行 python scripts/native_test_tools.py（要求 {VERSION}）"
        raise RuntimeError(msg)
    return {"PATH": str(BIN_DIR) + os.pathsep + os.environ.get("PATH", "")}


def main() -> int:
    """显式安装固定上游源码与兼容补丁，复用 Cargo 的普通依赖缓存."""
    if _installed_version_matches():
        print(f"私有 cargo-affected {VERSION} 已安装：{BIN_DIR}")
        return 0

    workspace = ROOT / ".tmp" / "native-tools"
    _ = subprocess.run(
        [sys.executable, str(ROOT / "scripts" / "make_tmp.py"), "workspace", "--destination", str(workspace)],
        cwd=ROOT,
        check=True,
    )
    git = shutil.which("git") or "git"
    cargo = shutil.which("cargo") or "cargo"
    with tempfile.TemporaryDirectory(prefix="cargo-affected-source-", dir=workspace) as temporary:
        source = Path(temporary) / "source"
        _ = subprocess.run([git, "clone", "--no-checkout", UPSTREAM, str(source)], check=True)
        _ = subprocess.run([git, "checkout", "--detach", REVISION], cwd=source, check=True)
        _ = subprocess.run([git, "apply", str(PATCH)], cwd=source, check=True)
        env = os.environ.copy()
        env["CARGO_TARGET_DIR"] = str(PREFIX / "target")
        # 开发工具不使用产品的 vendor 清单；输出仍限于私有前缀。
        _ = subprocess.run(
            [cargo, "build", "--locked", "--release", "--manifest-path", str(source / "Cargo.toml")],
            cwd=Path.home(),
            env=env,
            check=True,
        )
        BIN_DIR.mkdir(parents=True, exist_ok=True)
        executable = "cargo-affected.exe" if os.name == "nt" else "cargo-affected"
        _ = shutil.copy2(PREFIX / "target" / "release" / executable, BIN_DIR / executable)
    if not _installed_version_matches():
        msg = f"安装后的私有 cargo-affected 版本不符：要求 {VERSION}"
        raise RuntimeError(msg)
    print(f"私有 cargo-affected {VERSION} 已安装：{BIN_DIR}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
