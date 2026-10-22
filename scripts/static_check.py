#!/usr/bin/env python3
"""Lightweight source-structure checks, NOT rustc, cargo check, or application tests.

Uses Python stdlib; Pygments, if present, adds Rust lexical delimiter checking.
All writes are confined to .tmp/static-check.json under this source tree,
except --update-test-baseline which rewrites scripts/test-baseline.json.

实现拆分：检查实现位于 scripts/static_check_impl/ 包（common/structure/baseline/ui_rules/misc）；
本模块保留模块级流程与检查注册表。受 scripts/test_review_trusted.py 的 AST 回归约束
（按入口命名空间 exec 真实声明并覆盖 ROOT/lex 后调用），rust_lexical、slint_modal_gating、
collect_tests、test_baseline、rust_sources、project_manifests、_collect_file_tests 必须以
真实 def 留在入口：其函数体从入口命名空间解析 ROOT 与 pygments 名称；实现包中的
manifests/product_naming/scope_and_delivery 依赖这些入口函数，由导入后的注入赋值衔接。
"""

from __future__ import annotations

import contextlib
import functools
import io
import json
import re
import shutil
import sys
from pathlib import Path

try:
    import pygments.lexers
    from pygments import lex
    from pygments.lexer import Lexer
    from pygments.token import Comment, Literal
except ImportError:
    lex = None
    Lexer = None
    Comment = None
    Literal = None
    pygments = None

if not __package__:
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
    if not (Path(__file__).resolve().parent / "static_check_impl" / "__init__.py").is_file():
        # 本文件被单独复制到合成 scripts/ 目录运行（可信基回归）时，向上定位最近的实现包。
        for _parent in Path(__file__).resolve().parents:
            if (_parent / "scripts" / "static_check_impl" / "__init__.py").is_file():
                sys.path.insert(0, str(_parent))
                break

from scripts.static_check_impl import baseline, common, misc, structure, ui_rules
from scripts.static_check_impl.baseline import (
    attr_spans,
    file_level_cfgs,
    load_baseline_rows,
    mask_rust_comments,
    mask_rust_trivia,
    mod_gates,
    test_body_hash,
    test_gates,
    write_baseline,
)
from scripts.static_check_impl.common import (
    OPTIONAL_CRATES,
    PRODUCTION_CRATES,
    SkippedError,
    check,
    checks,
    module_attr,
    read_text,
)
from scripts.static_check_impl.misc import (
    acceptance_respects_cargo_target_dir,
    build_rc_prefers_windows_kits,
    requirements_registry,
    scope_and_delivery,
    snap_pipe_command_sync,
)
from scripts.static_check_impl.structure import (
    check_delimiters,
    config_schema,
    manifests,
    ps1_utf8_bom,
    shell_syntax,
    sql_syntax,
    ui_callbacks,
)
from scripts.static_check_impl.ui_rules import product_naming, slint_colors, slint_layout_width

ROOT = Path(__file__).resolve().parent.parent
UPDATE_BASELINE = "--update-test-baseline" in sys.argv[1:]
# 检查明细含中文；Windows CI 默认 cp1252，会把 print/写文件变成 UnicodeEncodeError。
# 明确按 UTF-8 输出（stdout 不可重配时退化为替换字符，不让编码问题变成检查失败）。
with contextlib.suppress(AttributeError):
    stream = sys.stdout
    if isinstance(stream, io.TextIOWrapper):
        stream.reconfigure(encoding="utf-8", errors="replace")


def project_manifests() -> list[Path]:
    """仅扫描本项目，不扫描 vendor 或 Cargo 输出；已登记 package 缺失即失败."""
    required = {
        ROOT / "Cargo.toml",
        *(ROOT / "crates" / name / "Cargo.toml" for name in PRODUCTION_CRATES),
        *(ROOT / "optional" / name / "Cargo.toml" for name in OPTIONAL_CRATES),
    }
    missing = sorted(path.relative_to(ROOT).as_posix() for path in required if not path.is_file())
    if missing:
        detail = f"本项目 Cargo 清单缺失：{missing}"
        raise AssertionError(detail)
    required_sources = {
        *(ROOT / "crates" / name / "src/lib.rs" for name in PRODUCTION_CRATES),
        *(ROOT / "optional" / name / "src/lib.rs" for name in OPTIONAL_CRATES),
        ROOT / "build.rs",
        ROOT / "crates/file-core/build.rs",
        ROOT / "crates/assets/build.rs",
        ROOT / "crates/ui/build.rs",
        ROOT / "optional/snap-ocr-worker/build.rs",
    }
    missing_sources = sorted(path.relative_to(ROOT).as_posix() for path in required_sources if not path.is_file())
    if missing_sources:
        detail = f"本项目库入口或真实 build producer 缺失：{missing_sources}"
        raise AssertionError(detail)
    return sorted(required | set(ROOT.glob("crates/*/Cargo.toml")) | set(ROOT.glob("optional/*/Cargo.toml")))


def rust_sources() -> list[Path]:
    """统一手写 Rust 范围：各 package 的 build/src/tests/examples/benches 及根夹具."""
    paths: set[Path] = set()
    for manifest in project_manifests():
        package_root = manifest.parent
        if not (package_root / "src").is_dir():
            detail = f"本项目源码目录缺失：{package_root.relative_to(ROOT) / 'src'}"
            raise AssertionError(detail)
        build_script = package_root / "build.rs"
        if build_script.is_file():
            paths.add(build_script)
        for directory in ("src", "tests", "examples", "benches"):
            paths.update((package_root / directory).glob("**/*.rs"))
    return sorted(paths)


def _collect_file_tests(path: Path) -> list[dict[str, object]]:
    text = read_text(path)
    structural = mask_rust_comments(text)
    discovery = mask_rust_trivia(text)
    rel = path.relative_to(ROOT).as_posix()
    # 文件头内层属性：#![cfg(...)]（遇到第一个顶层项即停，避免误读宏内的 #![ ]）
    head = structural[
        : len(
            re.split(
                r"\n(?=(?:pub\s+)?(?:use|mod|fn|struct|enum|impl|trait|const|static|type|macro_rules)\b)",
                discovery,
            )[0]
        )
    ]
    file_cfgs = file_level_cfgs(head)
    # 模块级门禁：#[cfg(...)] mod X { ... }
    mod_spans = mod_gates(structural)
    # 全部属性跨度（供逐测试回溯/前瞻；#![ ] 内层属性只出现在文件头，不会被测试链到）
    attrs = attr_spans(structural)
    rows: list[dict[str, object]] = []
    for match in re.finditer(r"#\[(?:tokio::)?test(?:\([^]]*\))?\]", discovery):
        cfgs, ignored = test_gates(structural, attrs, match, file_cfgs, mod_spans)
        name_match = re.match(r"[^{};]*?fn\s+(\w+)\s*\(", discovery[match.end() :])
        if not name_match:
            continue
        rows.append(
            {
                "file": rel,
                "name": name_match.group(1),
                "ignored": ignored,
                "cfg": " && ".join(cfgs) if cfgs else None,
                # 测试体内容哈希：防「保留名字、清空/放宽断言」式削弱（基线 val 的一部分）。
                "body": test_body_hash(text, match.end() + name_match.end()),
            }
        )
    return rows


@functools.cache
def collect_tests() -> list[dict[str, object]]:
    # 提取本项目所有 package 的手写 Rust #[test] / #[tokio::test]。基线键包含生效门禁（cfg）：
    #   1) 文件级 #![cfg(...)]（仅扫文件头部内层属性区）；
    #   2) 包裹测试的 #[cfg(...)] mod 块（花括号配对取范围；cfg(test) 恒真不计）；
    #   3) 紧贴 #[test] 的属性块（前后均可，方括号配对，支持 not(windows) 等嵌套括号）。
    # 已知残留盲区：定义在其它文件里的门禁（如 lib.rs 对模块声明的 cfg）
    # 不在本文件扫描范围内——本检查是执法下界，不是完整 cfg 求值器。
    # 单次进程内 test_baseline 与 scope_and_delivery 复用同一次全量扫描：
    # 源文件在一次运行中只读，结果纯函数于文件内容；调用方均只读，缓存不改变判据。
    rows: list[dict[str, object]] = []
    # 与词法/SQL/实现检查共用范围，包含搬迁的 crate 内联测试与 jchocr 集成测试，
    # 也包含 optional 的 src/tests/examples/benches；实际执行仍受默认成员/显式入口控制。
    for path in rust_sources():
        rows.extend(_collect_file_tests(path))
    keys = [(row["file"], row["name"], row["cfg"]) for row in rows]
    if len(keys) != len(set(keys)):
        detail = f"测试键重复（同名且同门禁）：{sorted(k for k in keys if keys.count(k) > 1)}"
        raise AssertionError(detail)
    return rows


def test_baseline() -> str:
    path = ROOT / "scripts" / "test-baseline.json"
    if not path.is_file():
        message = (
            "scripts/test-baseline.json 缺失；" + "先运行 python scripts/static_check.py --update-test-baseline 生成"
        )
        raise AssertionError(message)

    def key(row: dict[str, object]) -> tuple[object, object, object]:
        return (row["file"], row["name"], row.get("cfg") or None)

    def val(row: dict[str, object]) -> tuple[bool, str]:
        return (bool(row.get("ignored")), str(row.get("body") or ""))

    baseline_rows = load_baseline_rows(path)
    seen_keys: set[tuple[object, object, object]] = set()
    duplicate_keys: set[tuple[object, object, object]] = set()
    for row in baseline_rows:
        row_key = key(row)
        if row_key in seen_keys:
            duplicate_keys.add(row_key)
        else:
            seen_keys.add(row_key)
    if duplicate_keys:
        raise AssertionError({"scripts/test-baseline.json 含重复测试键 (file, name, cfg)": sorted(duplicate_keys)})
    base = {key(row): val(row) for row in baseline_rows}
    now = {key(row): val(row) for row in collect_tests()}
    added = sorted(set(now) - set(base))
    removed = sorted(set(base) - set(now))
    changed = sorted(k for k in set(base) & set(now) if base[k] != now[k])
    if added or removed or changed:
        raise AssertionError(
            {
                "新增测试(更新基线并在提交信息说明覆盖点)": added,
                "删除或改名(提交信息必须说明理由)": removed,
                "ignore状态翻转(提交信息必须说明理由)": [
                    f"{k}: ignored {base[k][0]} -> {now[k][0]}" for k in changed if base[k][0] != now[k][0]
                ],
                "测试体变化(审查确认非削弱后更新基线并说明理由)": [
                    f"{k}: body {base[k][1]} -> {now[k][1]}" for k in changed if base[k][1] != now[k][1]
                ],
                "更新命令": "python scripts/static_check.py --update-test-baseline",
            }
        )
    return f"{len(now)} 个测试与 scripts/test-baseline.json 完全一致（含 ignore、平台门禁与测试体哈希）。"


# rust_lexical 使用 pygments；RustLexer 无类型标注，经 getattr 别名取回并运行期校验。
def rust_lexical() -> str:
    lex_fn = lex
    lexer_base = Lexer
    comment_token = Comment
    literal_token = Literal
    package = pygments
    if lex_fn is None or lexer_base is None or comment_token is None or literal_token is None or package is None:
        message = "Pygments unavailable; Rust lexical delimiter check was not run."
        raise SkippedError(message) from None
    lexer_factory: object = module_attr(package.lexers, "RustLexer")
    if not callable(lexer_factory):
        detail = "pygments 的 RustLexer 不可调用"
        raise TypeError(detail)
    count = 0
    for path in rust_sources():
        stack: list[str] = []
        lexer: object = lexer_factory()
        if not isinstance(lexer, lexer_base):
            detail = "pygments 的 RustLexer 实例类型异常"
            raise TypeError(detail)
        for token, text in lex_fn(read_text(path), lexer):
            if token in comment_token or token in literal_token.String:
                continue
            check_delimiters(path, text, stack)
        if stack:
            raise AssertionError((str(path.relative_to(ROOT)), stack))
        count += 1
    return (
        f"{count} Rust files have balanced lexical delimiters; "
        "this does NOT validate Rust types, APIs, macros or borrow checking."
    )


def slint_modal_gating() -> str:
    # AGENTS.md 第 4 节与仓库既有先例（app.slint 内「键盘可穿透确认框」修复注释）：
    # 确认模态（confirm-kind != 0）的 scrim 只挡鼠标，Tab 焦点仍可到达被遮挡控件；
    # AppWindow 内（确认层之前）所有可交互控件的 enabled 绑定都必须门禁 confirm-kind。
    # 页面拆分后，ui/pages/ 下的页面组件同样受此约束：其 enabled 绑定（属性声明除外）
    # 必须引用转发进来的 confirm-kind 属性；控件库文件不重复接线，规则不扫描。
    bad: list[tuple[str, int, str]] = []
    app = read_text(ROOT / "ui" / "app.slint")
    start = app.index("export component AppWindow inherits Window")
    overlay = app.index("if root.confirm-kind != 0:", start)
    background = app[start:overlay]
    # enabled 绑定到分号才结束；换行不能截断后续的确认门禁。
    for enabled_match in re.finditer(r"(?<![\w-])enabled\s*:([^;]*);", background):
        expr = enabled_match.group(1)
        # 常量 false 不可能交互；含其他运算的表达式仍须显式确认门禁。
        if expr.strip() != "false" and "confirm-kind" not in expr:
            line_number = app.count("\n", 0, start + enabled_match.start()) + 1
            bad.append(("app.slint", line_number, "enabled: " + " ".join(expr.split())))
    pages_dir = ROOT / "ui" / "pages"
    for path in sorted(pages_dir.glob("*.slint")) if pages_dir.is_dir() else []:
        ui = read_text(path)
        # 属性声明（含默认值）不是交互接线，剔除后再找 enabled 绑定。
        masked = re.sub(r"(?:private\s+|in-out\s+|in\s+|out\s+)?property\s*<[^>]*>\s*[\w-]+\s*:[^;]*;", "", ui)
        for enabled_match in re.finditer(r"(?<![\w-])enabled\s*:([^;]*);", masked):
            expr = enabled_match.group(1)
            if expr.strip() != "false" and "confirm-kind" not in expr:
                line_number = ui.count("\n", 0, enabled_match.start()) + 1
                bad.append((path.name, line_number, "enabled: " + " ".join(expr.split())))
    if bad:
        detail = f"AppWindow/页面组件内存在未按 confirm-kind 门禁的 enabled 绑定（文件, 行, 表达式）：{bad}"
        raise AssertionError(detail)
    return "AppWindow 内（确认层之前）所有可交互控件的 enabled 绑定都门禁 confirm-kind。"


# 实现包检查与入口共用同一 ROOT 和入口定义的清单/测试发现函数：单文件复制运行
# （可信基回归把本文件复制到合成 scripts/ 目录）与真实入口采用完全相同的判据。
# 依赖注入用 setattr：实现包把这些名字当作模块级可变绑定使用，
# 显式赋值形式会被类型检查误判为跨模块私有改写。
for _module in (common, structure, baseline, ui_rules, misc):
    setattr(_module, "ROOT", ROOT)  # noqa: B010 - 依赖注入需绕开跨模块改写检查
setattr(structure, "project_manifests", project_manifests)  # noqa: B010 - 同上
setattr(ui_rules, "project_manifests", project_manifests)  # noqa: B010 - 同上
setattr(misc, "collect_tests", collect_tests)  # noqa: B010 - 同上


if UPDATE_BASELINE:
    rows = collect_tests()
    write_baseline(rows)
    print(f"baseline updated: {len(rows)} tests -> scripts/test-baseline.json")
    sys.exit(0)

# 检查注册表：条目名即函数名（fn.__name__），新增检查只需把函数加进列表，
# 避免名称字符串与函数名两处同步漂移。
for fn in [
    manifests,
    config_schema,
    ui_callbacks,
    rust_lexical,
    sql_syntax,
    shell_syntax,
    ps1_utf8_bom,
    test_baseline,
    requirements_registry,
    snap_pipe_command_sync,
    slint_layout_width,
    slint_colors,
    product_naming,
    slint_modal_gating,
    build_rc_prefers_windows_kits,
    acceptance_respects_cargo_target_dir,
    scope_and_delivery,
]:
    check(fn.__name__, fn)
    # 仅调度失败即停；检查定义、判据与独立报告保持不变（2026-10-11 用户批准）。
    if checks[-1]["status"] == "FAIL":
        break

report = {
    "kind": "lightweight static source checks only",
    "platform": sys.platform,
    "python": sys.version.split()[0],
    "rustc": shutil.which("rustc"),
    "cargo": shutil.which("cargo"),
    "powershell": shutil.which("pwsh"),
    "cargo_check": "NOT RUN",
    "cargo_test": "NOT RUN",
    "windows_runtime": "NOT RUN",
    "real_7zip_tests": "NOT RUN",
    "multi_tb_benchmark": "NOT RUN",
    "checks": checks,
}
report_dir = ROOT / ".tmp"
report_dir.mkdir(parents=True, exist_ok=True)
_ = (report_dir / "static-check.json").write_text(
    json.dumps(report, ensure_ascii=False, indent=2) + "\n",
    newline="\n",
    encoding="utf-8",
)
for row in checks:
    print(row["status"], row["name"], row["details"])
if any(entry["status"] == "FAIL" for entry in checks):
    sys.exit(1)
sys.exit(2 if any(entry["status"] == "SKIP" for entry in checks) else 0)
