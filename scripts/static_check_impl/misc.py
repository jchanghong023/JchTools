"""static_check 杂项检查：rc 优先级、验收口径、范围交付、需求登记与管道命令同步（自 static_check.py 原样搬移）."""

from __future__ import annotations

import re
from pathlib import Path
from typing import TYPE_CHECKING

from scripts.static_check_impl.common import ROOT, read_text

if TYPE_CHECKING:
    from collections.abc import Callable

# 测试发现函数定义在入口 static_check.py（AST 回归按入口命名空间覆盖 ROOT），
# 由入口在导入后注入本属性。
collect_tests: Callable[[], list[dict[str, object]]]


def build_rc_prefers_windows_kits() -> str:
    # 防 PATH 劫持口径：build.rs 解析 rc.exe 必须优先 Windows SDK 目录；PATH 只能
    # 作为回退（且回退时构建日志应有告警）。
    text = read_text(ROOT / "build.rs")
    kits = text.find("Windows Kits")
    from_path = text.find('var_os("PATH")')
    if not (from_path == -1 or (kits != -1 and kits < from_path)):
        detail = "build.rs 中 rc.exe 的 Windows SDK 查找必须先于 PATH 查找（或不搜 PATH）"
        raise AssertionError(detail)
    return "rc.exe 优先从 Windows SDK 解析，PATH 仅作回退。"


def acceptance_respects_cargo_target_dir() -> str:
    # acceptance.ps1 的 gui-smoke exe 路径必须与 package-windows.ps1 同口径尊重
    # CARGO_TARGET_DIR：否则自定义 target 目录的机器会误报验证失败，或冒烟陈旧 exe。
    text = read_text(ROOT / "scripts" / "acceptance.ps1")
    if "CARGO_TARGET_DIR" not in text:
        detail = "acceptance.ps1 的 gui-smoke 必须尊重 CARGO_TARGET_DIR（与 package-windows.ps1 同口径）"
        raise AssertionError(detail)
    return "acceptance.ps1 的 gui-smoke 尊重 CARGO_TARGET_DIR。"


def scope_and_delivery() -> str:
    required = [
        "README.md",
        "先读我.txt",
        "LICENSE",
        "THIRD_PARTY_NOTICES.md",
        "docs/CONTRACT.md",
        "scripts/package-windows.ps1",
        "scripts/fetch-7zip.ps1",
        "scripts/test-baseline.json",
        "tests/core.rs",
        "tests/archive.rs",
        ".github/workflows/check.yml",
    ]
    if not all((ROOT / p).is_file() for p in required):
        raise AssertionError
    for path in sorted(
        [*ROOT.glob("src/**/*.rs"), *ROOT.glob("crates/*/src/**/*.rs"), *ROOT.glob("optional/*/src/**/*.rs")]
    ):
        text = read_text(path)
        if "todo!(" in text or "unimplemented!(" in text:
            raise AssertionError(str(path))
    tests = len(collect_tests())  # noqa: F821  # 定义在入口 static_check.py，由入口导入后注入本模块属性。
    return (
        f"{tests} Rust test functions supplied, NOT executed; no todo!/unimplemented! in Rust implementation; "
        "no prebuilt executable asserted."
    )


_REQUIREMENT_DOCS = [
    "docs/requirements/README.md",
    "docs/requirements/CONTRACT.md",
    "docs/requirements/RECURSIVE-EXTRACT.md",
    "docs/requirements/DIRECTORY-ORGANIZER.md",
    "docs/requirements/MD-TOOLS.md",
    "docs/requirements/GIT-TOOLS.md",
    "docs/requirements/ALL2MARKDOWN.md",
    "docs/requirements/SNAP2TEXT.md",
    "docs/requirements/XBERG-INFERENCE.md",
    "docs/requirements/ACP-MODEL-SERVICE.md",
    "docs/requirements/CODEAGENT.md",
]


_MIN_REQUIREMENT_DOC_LINES = 10


def requirements_registry() -> str:
    """固定需求目录的权威清单执法：文档在场、非空壳且无未登记项.

    登记的需求文档及索引必须在场且非空壳，目录内不得出现未登记的额外 .md
    （新增需求文档须先更新 AGENTS.md 权威分工与本清单）。
    """
    for path in _REQUIREMENT_DOCS:
        if not (ROOT / path).is_file():
            detail = f"权威需求文档缺失：{path}"
            raise AssertionError(detail)
        lines = [line for line in read_text(ROOT / path).splitlines() if line.strip()]
        if len(lines) < _MIN_REQUIREMENT_DOC_LINES:
            detail = f"需求文档内容异常单薄（有效行 <{_MIN_REQUIREMENT_DOC_LINES}）：{path}"
            raise AssertionError(detail)
    expected = {str(Path(p).name) for p in _REQUIREMENT_DOCS}
    names = {str(path.name) for path in (ROOT / "docs" / "requirements").glob("*.md")}
    extra = sorted(names - expected)
    if extra:
        detail = f"docs/requirements/ 存在未登记的需求文档（先更新 AGENTS.md 权威分工与本检查清单）：{extra}"
        raise AssertionError(detail)
    return f"固定需求目录 {len(expected)} 份权威文档及索引在场，无未登记文档：{sorted(expected)}"


def snap_pipe_command_sync() -> str:
    """截图服务命名管道协议双端同步执法：gui 发出的命令服务端必须可处理.

    crates/gui/src/** 下任意文件构造的每个 "command" 字面量都必须在 worker 服务
    分发表（optional/snap-ocr-worker/src/service.rs 及其 service/ 子模块）中有
    对应处理；两端无共享类型，本检查防协议各自演进造成静默失配。扫描范围随
    2026-10 文件拆分扩展为模块子树（原单文件判据已不能覆盖拆分后的位置）。
    """
    gui = chr(10).join(
        read_text(path) for path in [ROOT / "src/main.rs", *sorted(ROOT.glob("crates/gui/src/**/*.rs"))]
    )
    service_files = [ROOT / "optional" / "snap-ocr-worker" / "src" / "service.rs", *sorted(
        (ROOT / "optional" / "snap-ocr-worker" / "src" / "service").glob("**/*.rs")
    )]
    service = chr(10).join(read_text(path) for path in service_files if path.is_file())
    sent: list[str] = sorted(set(re.findall(r'"command"\s*:\s*"([a-z-]+)"', gui)))
    if not sent:
        detail = "crates/gui/src/** 中未发现任何管道命令字面量（检查本身失效）"
        raise AssertionError(detail)
    unhandled = [cmd for cmd in sent if f'"{cmd}"' not in service]
    if unhandled:
        detail = f"管道命令在服务端无处理：{unhandled}（gui 与 worker 服务模块双端必须同步演进）"
        raise AssertionError(detail)
    return f"gui 侧发出的 {len(sent)} 个管道命令在服务端均有处理：{sent}"
