"""质量检查角色边界：生产源码严格检查，纯测试只保留语法、编译和实际执行。."""

from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

PYTHON_TEST_FILES = frozenset(
    {
        "scripts/test_gui_smoke.py",
        "scripts/test_markdown_acceptance.py",
        "scripts/test_test_gate.py",
        "scripts/test_review_trusted.py",
        "scripts/test_package_windows.py",
        "tests/ocr_fixtures/test_worker_root_isolation.py",
        "scripts/media_e2e_fixture.py",
    }
)
# 唯一位于生产源码目录中的独立纯测试文件；其余测试位于 tests/。
RUST_TEST_FILES = frozenset({"optional/snap-ocr-worker/src/capture_win_preview_tests.rs"})


class Arguments(argparse.Namespace):
    check: str = ""


def production_python(root: Path) -> list[str]:
    """按精确名单选择可执行生产脚本；声明桩继续由 Ruff 与类型检查覆盖。."""
    tests = PYTHON_TEST_FILES
    return sorted(
        path.relative_to(root).as_posix()
        for path in (root / "scripts").rglob("*.py")
        if path.relative_to(root).as_posix() not in tests
    )


def python_syntax(root: Path) -> int:
    """检查全部维护的 Python 源码，包含测试与夹具，不执行导入副作用。."""
    count = 0
    for directory in ("scripts", "typings", "tests"):
        for path in sorted((root / directory).rglob("*")):
            if path.suffix in {".py", ".pyi"}:
                _ = compile(path.read_bytes(), str(path), "exec")
                count += 1
    print(f"Python 语法检查通过：{count} 个文件（包含测试）。")
    return 0


def rust_format(root: Path) -> int:
    """只格式检查生产文件；文件内测试声明使用 rustfmt::skip 保留其格式。."""
    rustfmt = shutil.which("rustfmt")
    if rustfmt is None:
        message = "缺少 rustfmt；不能执行生产 Rust 格式检查。"
        raise FileNotFoundError(message)
    files = sorted(
        path.relative_to(root).as_posix()
        for directory in ("src", "optional/snap-ocr-core/src", "optional/snap-ocr-worker/src")
        for path in (root / directory).rglob("*.rs")
        if path.relative_to(root).as_posix() not in RUST_TEST_FILES
    )
    files.extend(["build.rs", "optional/snap-ocr-worker/build.rs"])
    result = subprocess.run(
        [rustfmt, "--check", "--edition", "2021", "--config", "skip_children=true", *files],
        cwd=root,
        check=False,
    )
    return result.returncode


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    _ = parser.add_argument("check", choices=("syntax", "rustfmt"))
    arguments = parser.parse_args(namespace=Arguments())
    if arguments.check == "syntax":
        return python_syntax(ROOT)
    return rust_format(ROOT)


if __name__ == "__main__":
    sys.exit(main())
