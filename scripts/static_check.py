#!/usr/bin/env python3
"""Lightweight source-structure checks, NOT rustc, cargo check, or application tests.

Uses Python stdlib; Pygments, if present, adds Rust lexical delimiter checking.
All writes are confined to .tmp/static-check.json under this source tree,
except --update-test-baseline which rewrites scripts/test-baseline.json.
"""

from __future__ import annotations

import contextlib
import hashlib
import io
import json
import re
import shutil
import sqlite3
import subprocess
import sys
import tomllib
from pathlib import Path
from typing import TYPE_CHECKING

import defusedxml.ElementTree

if TYPE_CHECKING:
    from collections.abc import Callable
    from typing import TypeIs

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

ROOT = Path(__file__).resolve().parent.parent
UPDATE_BASELINE = "--update-test-baseline" in sys.argv[1:]
# 检查明细含中文；Windows CI 默认 cp1252，会把 print/写文件变成 UnicodeEncodeError。
# 明确按 UTF-8 输出（stdout 不可重配时退化为替换字符，不让编码问题变成检查失败）。
with contextlib.suppress(AttributeError):
    stream = sys.stdout
    if isinstance(stream, io.TextIOWrapper):
        stream.reconfigure(encoding="utf-8", errors="replace")

# json.loads / tomllib.loads / getattr 的返回含 Any；经固定签名别名收口为 object，
# 随后用 TypeIs 守卫逐层校验成精确类型。
_parse_json: Callable[[str], object] = json.loads
_parse_toml: Callable[[str], object] = tomllib.loads
_module_attr: Callable[[object, str], object] = getattr
# rules.json 单条 choice 规则至少要有的选项数；bash「命令不存在」的退出码；
# 非 ASCII 判定阈值（0x00-0x7F 在 UTF-8 与任何 ANSI 单字节代码页下解码一致）。
_MIN_CHOICE_COUNT = 2
_BASH_COMMAND_NOT_FOUND = 127
_ASCII_LIMIT = 0x80


def _is_str(value: object) -> TypeIs[str]:
    return isinstance(value, str)


def _is_str_obj_map(value: object) -> TypeIs[dict[str, object]]:
    return isinstance(value, dict)


def _is_obj_list(value: object) -> TypeIs[list[object]]:
    return isinstance(value, list)


def read_text(path: Path) -> str:
    return path.read_text(encoding="utf-8")


checks: list[dict[str, object]] = []


# 检查在本机不可执行（依赖缺失等）；与 PASS/FAIL 区分，避免“没跑”被记成“通过”。
class SkippedError(Exception):
    pass


def check(name: str, fn: Callable[[], object]) -> None:
    try:
        details = fn()
        checks.append({"name": name, "status": "PASS", "details": details})
    except SkippedError as exc:
        checks.append({"name": name, "status": "SKIP", "details": str(exc)})
    # BLE001：这里必须捕获任意 Exception——check() 的职责就是把单个检查的任何异常记为
    # FAIL 而不是让整个脚本崩溃；可出现的异常类型无法穷举，重抛又会改变退出行为。
    # （异常转为 FAIL 条目而非静默吞掉，属检查器逐项汇总场景。）
    except Exception as exc:  # noqa: BLE001
        checks.append({"name": name, "status": "FAIL", "details": str(exc)})


def manifests() -> str:
    cargo_value: object = _parse_toml(read_text(ROOT / "Cargo.toml"))
    if not _is_str_obj_map(cargo_value):
        detail = "Cargo.toml 顶层必须是表"
        raise AssertionError(detail)
    bin_value: object = cargo_value["bin"]
    if not _is_obj_list(bin_value):
        detail = "Cargo.toml 的 bin 必须是数组"
        raise AssertionError(detail)
    for binary in bin_value:
        if not _is_str_obj_map(binary):
            detail = "Cargo.toml 的 bin 项必须是表"
            raise AssertionError(detail)
        bin_path = binary["path"]
        if not _is_str(bin_path):
            detail = "Cargo.toml 的 bin 项 path 必须是字符串"
            raise AssertionError(detail)
        if not (ROOT / bin_path).is_file():
            raise AssertionError
    _ = defusedxml.ElementTree.parse(ROOT / "resources/windows.manifest")
    return "Cargo TOML, declared binary paths, Windows XML parsed."


def _rules_rows() -> list[dict[str, object]]:
    rules_value: object = _parse_json(read_text(ROOT / "resources/rules.json"))
    if not _is_obj_list(rules_value):
        detail = "resources/rules.json 顶层必须是数组"
        raise AssertionError(detail)
    rows: list[dict[str, object]] = []
    for item in rules_value:
        if not _is_str_obj_map(item):
            detail = "resources/rules.json 的规则项必须是对象"
            raise AssertionError(detail)
        rows.append(item)
    return rows


def _choice_values(choices: list[object]) -> list[str]:
    if len(choices) < _MIN_CHOICE_COUNT:
        raise AssertionError
    values: list[str] = []
    for choice in choices:
        if not _is_obj_list(choice):
            detail = "resources/rules.json 的 choice 项必须是列表"
            raise AssertionError(detail)
        first = choice[0]
        if not _is_str(first):
            detail = "resources/rules.json 的 choice 首项必须是字符串"
            raise AssertionError(detail)
        values.append(first)
    if len(values) != len(set(values)):
        raise AssertionError
    return values


def _expected_enum_values(ty: str, config: str) -> set[str]:
    enum_match = re.search("pub enum " + ty + r"\s*\{(.*?)\}", config, re.DOTALL)
    if enum_match is None:
        detail = f"找不到配置枚举 {ty}"
        raise AssertionError(detail)
    return {re.sub(r"(?<!^)(?=[A-Z])", "_", v.strip()).lower() for v in enum_match.group(1).split(",") if v.strip()}


def _check_rule_choices(row: dict[str, object], ty: str, config: str) -> None:
    choices = row["choices"]
    if not _is_obj_list(choices):
        detail = "resources/rules.json 的 choices 必须是数组"
        raise AssertionError(detail)
    values = _choice_values(choices)
    if ty != "String":
        expected = _expected_enum_values(ty, config)
        if set(values) != expected:
            raise AssertionError((row["key"], values, expected))


def _check_rule_row(row: dict[str, object], fields: dict[str, str], config: str) -> None:
    if not (row["title"] and row["hint"]):
        raise AssertionError
    key_value = row["key"]
    if not _is_str(key_value):
        detail = "resources/rules.json 的 key 必须是字符串"
        raise AssertionError(detail)
    ty = fields[key_value]
    if ty == "bool" and row["kind"] != "bool":
        raise AssertionError
    if ty in ("usize", "u64", "u32") and row["kind"] != "number":
        raise AssertionError
    if row["kind"] == "choice":
        _check_rule_choices(row, ty, config)


def config_schema() -> str:
    config = read_text(ROOT / "src/config.rs")
    match = re.search(r"pub struct Config\s*\{(.*?)\n\}", config, re.DOTALL)
    if match is None:
        detail = "src/config.rs 中找不到 pub struct Config 定义"
        raise AssertionError(detail)
    fields = dict(re.findall(r"pub\s+(\w+)\s*:\s*([\w:]+)", match.group(1)))
    rows = _rules_rows()
    keys = [row["key"] for row in rows]
    if len(keys) != len(set(keys)):
        raise AssertionError
    # 有意不进规则表的配置字段：theme 已挪到「关于」页；detect_type 是合并行的
    # 影子键（「修正扩展名」一行驱动两个细粒度字段），引擎 / 旧任务库仍读原值。
    # 去重三类（同名/副本名/不同名同内容）按 R-04 是独立规则行，不在此列。
    hidden = {"theme", "detect_type"}
    if set(fields) != set(keys) | hidden:
        raise AssertionError((set(fields) - set(keys) - hidden, set(keys) - set(fields)))
    for row in rows:
        _check_rule_row(row, fields, config)
    return f"{len(rows)} UI settings match serialized Config fields ({len(hidden)} engine-only) and enum values."


def ui_callbacks() -> str:
    ui = read_text(ROOT / "ui/app.slint")
    # GUI 组装层在 src/gui.rs（bin main.rs 只是薄壳入口），两者都可能有 ui.on_* 接线。
    rs = chr(10).join(read_text(ROOT / p) for p in sorted(ROOT.glob("src/*.rs")))
    # 只有导出组件（窗口）上的回调是应用级 API；组件内部的回调在 .slint 内部接线。
    text = ui[ui.index("export component AppWindow inherits Window") :]
    depth = 0
    end = len(text)
    for index, char in enumerate(text):
        if char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
            if depth == 0:
                end = index
                break
    callback_names: list[str] = re.findall(r"callback\s+([\w-]+)\(", text[:end])
    declared = {name.replace("-", "_") for name in callback_names}
    handler_names: list[str] = re.findall(r"ui\.on_(\w+)\(", rs)
    wired = set(handler_names)
    if declared != wired:
        raise AssertionError({"missing_handlers": sorted(declared - wired), "extra_handlers": sorted(wired - declared)})
    return f"{len(declared)} declared window callbacks have Rust handlers."


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
    lexer_factory: object = _module_attr(package.lexers, "RustLexer")
    if not callable(lexer_factory):
        detail = "pygments 的 RustLexer 不可调用"
        raise TypeError(detail)
    count = 0
    for path in [ROOT / "build.rs", *ROOT.glob("src/**/*.rs"), *ROOT.glob("tests/**/*.rs")]:
        stack: list[str] = []
        lexer: object = lexer_factory()
        if not isinstance(lexer, lexer_base):
            detail = "pygments 的 RustLexer 实例类型异常"
            raise TypeError(detail)
        for token, text in lex_fn(read_text(path), lexer):
            if token in comment_token or token in literal_token.String:
                continue
            _check_delimiters(path, text, stack)
        if stack:
            raise AssertionError((str(path.relative_to(ROOT)), stack))
        count += 1
    return (
        f"{count} Rust files have balanced lexical delimiters; "
        "this does NOT validate Rust types, APIs, macros or borrow checking."
    )


def _check_delimiters(path: Path, text: str, stack: list[str]) -> None:
    for ch in text:
        if ch in "([{":
            stack.append(ch)
        elif ch in ")]}" and not (stack and "([{".index(stack.pop()) == ")]}".index(ch)):
            raise AssertionError(str(path.relative_to(ROOT)))


def sql_syntax() -> str:
    conn = sqlite3.connect(":memory:")
    _ = conn.executescript(read_text(ROOT / "src/schema.sql"))
    _ = conn.executescript("""CREATE TEMP TABLE duplicate_order(seq INTEGER,id INTEGER);
        CREATE TEMP TABLE empty_order(seq INTEGER,rel TEXT);
        CREATE TEMP TABLE empty_will(rel TEXT PRIMARY KEY);
        CREATE TEMP TABLE stay_parents(parent TEXT);
        CREATE TEMP TABLE dir_children(parent TEXT,rel TEXT);
        CREATE TEMP TABLE hash_candidates(id INTEGER PRIMARY KEY);
        CREATE TEMP TABLE scan_taint(rel TEXT PRIMARY KEY);
        CREATE TEMP TABLE tainted_will(rel TEXT PRIMARY KEY);
        CREATE TABLE hash_cache(identity TEXT NOT NULL, size INTEGER NOT NULL, mtime INTEGER NOT NULL,
            hash TEXT NOT NULL, updated INTEGER NOT NULL, PRIMARY KEY(identity,size,mtime));""")
    file_columns = "id,rel,name,normal,size,mtime,identity,links,hash,cleanable"
    statements: set[str] = set()
    for path in ROOT.glob("src/**/*.rs"):
        for match in re.finditer(r'"((?:[^"\\]|\\.)*)"', read_text(path)):
            raw = match.group(1)
            if not re.match(r"^(SELECT|UPDATE|INSERT|DELETE)\b", raw):
                continue
            raw = raw.replace("{FILE_COLUMNS}", file_columns)
            raw = raw.replace("{key_expr}", "name")
            raw = raw.replace("{filter}", "active=1 AND name=?1 AND size=?2")
            if "{" in raw or "}" in raw or ";" in raw:
                continue
            raw = raw.replace('\\"', '"')
            question_params: list[str] = re.findall(r"\?(\d+)", raw)
            params = max([int(x) for x in question_params] + [0])
            _ = conn.execute("EXPLAIN " + raw, [None] * params)
            statements.add(raw)
    return (
        f"SQLite schema and {len(statements)} concrete DML statements prepare successfully "
        "against empty schema; no organizer application executed."
    )


def shell_syntax() -> str:
    # 平台范围按合同 P-07 仅 Windows：scripts/ 下可能没有 .sh；有则逐个 bash -n 校验。
    scripts = sorted((ROOT / "scripts").glob("*.sh"))
    if not scripts:
        return "no shell scripts under scripts/; nothing to check"
    bash = shutil.which("bash")
    if not bash:
        message = "bash not found; bash -n and PowerShell syntax checks run in the Windows CI job."
        raise SkippedError(message)
    for script in scripts:
        done = subprocess.run([bash, "-n", str(script)], capture_output=True, text=True, check=False)
        if done.returncode != 0 and (
            "not found" in (done.stderr or "").lower() or done.returncode == _BASH_COMMAND_NOT_FOUND
        ):
            message = "bash launcher is unavailable on this host; bash -n runs in the Windows CI job."
            raise SkippedError(message)
        if done.returncode != 0:
            message = f"{script}: {done.stderr}"
            raise AssertionError(message)
    names = ", ".join(path.name for path in scripts)
    return (
        f"bash -n passed for {len(scripts)} shell script(s) ({names}); "
        "PowerShell syntax check is defined in Windows CI."
    )


def ps1_utf8_bom() -> str:
    # 反证（2026-09-19，run 35430517207 与 zh-CN 本机）：Windows PowerShell 5.1 对无 BOM 的
    # .ps1 按 ANSI 代码页解码，在 cp936/cp932 等多字节代码页下中文注释会吞掉后续换行
    # （fetch-7zip.ps1 实测 83 行→81 行），把下一行代码并进注释而静默失效——该文件的
    # `$previousEap = $ErrorActionPreference` 即如此丢失，finally 引用未赋值变量报错；
    # CI 的 en-US/cp1252 是单字节代码页，不吞换行，故 CI 绿色掩盖了此缺陷。
    # 带 BOM 后 PowerShell 5.1 一律按 UTF-8 解码，与文件实际编码一致；
    # 纯 ASCII 文件不在此列：其字节在 ANSI 与 UTF-8 下解码结果相同，本就不受该缺陷影响。
    scripts = sorted((ROOT / "scripts").glob("*.ps1"))
    missing: list[str] = []
    for path in scripts:
        raw = path.read_bytes()
        if raw.startswith(b"\xef\xbb\xbf") or all(byte < _ASCII_LIMIT for byte in raw):
            continue
        missing.append(path.name)
    if missing:
        detail = f"scripts/*.ps1 含非 ASCII 内容时必须带 UTF-8 BOM（否则多字节 ANSI 代码页下会被误解码）：{missing}"
        raise AssertionError(detail)
    return f"{len(scripts)} 个 .ps1 均带 UTF-8 BOM 或为纯 ASCII；PowerShell 5.1 解码不受 ANSI 代码页影响。"


# text[i] 指向 '#'：解析 #[...] 与 #![...] 属性（括号配对，允许嵌套括号）。
# 返回 (属性体如 'cfg(not(windows))' 或 'ignore = "..."', 结束下标+1)；不是属性则 None。
def _attr_body(text: str, i: int) -> tuple[str, int] | None:
    if i + 1 >= len(text) or text[i] != "#":
        return None
    j = i + 1
    if text[j] == "!":
        j += 1
    if j >= len(text) or text[j] != "[":
        return None
    depth = 0
    for k in range(j, len(text)):
        if text[k] == "[":
            depth += 1
        elif text[k] == "]":
            depth -= 1
            if depth == 0:
                return text[j + 1 : k], k + 1
    return None


# 'cfg(...)' -> '...'；其余属性返回 None。'test' 门禁对 cargo test 恒真，返回 '' 表示忽略。
def _cfg_payload(inner: str) -> str | None:
    s = inner.strip()
    if not s.startswith("cfg"):
        return None
    s = s[3:].strip()
    if s.startswith("(") and s.endswith(")"):
        s = s[1:-1].strip()
    return "" if s == "test" else s


def _file_level_cfgs(head: str) -> list[str]:
    cfgs: list[str] = []
    for match in re.finditer(r"#!\[", head):
        got = _attr_body(head, match.start())
        if got:
            payload = _cfg_payload(got[0])
            if payload and payload not in cfgs:
                cfgs.append(payload)
    return cfgs


def _mod_gates(text: str) -> list[tuple[int, int, str]]:
    spans: list[tuple[int, int, str]] = []
    pattern = r"#\[cfg\s*\(([^()]*(?:\([^()]*\)[^()]*)*)\)\]\s*" + r"(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*\{"
    for match in re.finditer(pattern, text):
        payload = match.group(1).strip()
        if payload == "test":
            continue
        end = _brace_end(text, text.find("{", match.end() - 1))
        spans.append((match.end(), end, payload))
    return spans


def _attr_spans(text: str) -> list[tuple[int, int, str]]:
    spans: list[tuple[int, int, str]] = []
    for match in re.finditer(r"#\[", text):
        got = _attr_body(text, match.start())
        if got:
            spans.append((match.start(), got[1], got[0]))
    return spans


def _adjacent(attr: tuple[int, int, str], cursor: int, text: str, step: int) -> bool:
    if step < 0:
        return attr[1] <= cursor and text[attr[1] : cursor].strip() == ""
    if step > 0:
        return attr[0] >= cursor and text[cursor : attr[0]].strip() == ""
    return False


# 收集紧贴的属性行（在 #[test] 之前 step=-1，之后 step=+1）。
def _walk_attrs(text: str, attrs: list[tuple[int, int, str]], pos: int, step: int) -> list[tuple[int, int, str]]:
    out: list[tuple[int, int, str]] = []
    cursor = pos
    while True:
        candidate = None
        for attr in attrs:
            if _adjacent(attr, cursor, text, step):
                candidate = attr
        if candidate is None:
            break
        out.append(candidate)
        cursor = candidate[0] if step < 0 else candidate[1]
    return out


def _brace_end(text: str, open_idx: int) -> int:
    depth = 0
    for i in range(open_idx, len(text)):
        if text[i] == "{":
            depth += 1
        elif text[i] == "}":
            depth -= 1
            if depth == 0:
                return i
    return len(text) - 1


def _test_gates(
    text: str,
    attrs: list[tuple[int, int, str]],
    match: re.Match[str],
    file_cfgs: list[str],
    mod_spans: list[tuple[int, int, str]],
) -> tuple[list[str], bool]:
    start = match.start()
    cfgs = list(file_cfgs)
    ignored = False
    neighbours = _walk_attrs(text, attrs, start, -1) + list(reversed(_walk_attrs(text, attrs, match.end(), 1)))
    for _, _, inner in neighbours:
        payload = _cfg_payload(inner)
        if payload is not None and payload and payload not in cfgs:
            cfgs.append(payload)
        if inner.strip().startswith("ignore"):
            ignored = True
    for span_start, span_end, payload in mod_spans:
        if span_start < start < span_end and payload not in cfgs:
            cfgs.append(payload)
    return cfgs, ignored


def _collect_file_tests(path: Path) -> list[dict[str, object]]:
    text = read_text(path)
    rel = path.relative_to(ROOT).as_posix()
    # 文件头内层属性：#![cfg(...)]（遇到第一个顶层项即停，避免误读宏内的 #![ ]）
    head = re.split(r"\n(?=(?:pub\s+)?(?:use|mod|fn|struct|enum|impl|trait|const|static|type|macro_rules)\b)", text)[0]
    file_cfgs = _file_level_cfgs(head)
    # 模块级门禁：#[cfg(...)] mod X { ... }
    mod_spans = _mod_gates(text)
    # 全部属性跨度（供逐测试回溯/前瞻；#![ ] 内层属性只出现在文件头，不会被测试链到）
    attrs = _attr_spans(text)
    rows: list[dict[str, object]] = []
    for match in re.finditer(r"#\[test\]", text):
        cfgs, ignored = _test_gates(text, attrs, match, file_cfgs, mod_spans)
        name_match = re.match(r"[^{};]*?fn\s+(\w+)\s*\(", text[match.end() :])
        if not name_match:
            continue
        rows.append(
            {
                "file": rel,
                "name": name_match.group(1),
                "ignored": ignored,
                "cfg": " && ".join(cfgs) if cfgs else None,
            }
        )
    return rows


def collect_tests() -> list[dict[str, object]]:
    # 提取 src/ 与 tests/ 全部 #[test]。基线键包含生效门禁（cfg）：
    #   1) 文件级 #![cfg(...)]（仅扫文件头部内层属性区）；
    #   2) 包裹测试的 #[cfg(...)] mod 块（花括号配对取范围；cfg(test) 恒真不计）；
    #   3) 紧贴 #[test] 的属性块（前后均可，方括号配对，支持 not(windows) 等嵌套括号）。
    # 已知残留盲区：定义在其它文件里的门禁（如 lib.rs 的 #[cfg(feature = "gui")] pub mod gui;）
    # 不在本文件扫描范围内——本检查是执法下界，不是完整 cfg 求值器。
    rows: list[dict[str, object]] = []
    for path in [*ROOT.glob("src/**/*.rs"), *ROOT.glob("tests/**/*.rs")]:
        rows.extend(_collect_file_tests(path))
    keys = [(row["file"], row["name"], row["cfg"]) for row in rows]
    if len(keys) != len(set(keys)):
        detail = f"测试键重复（同名且同门禁）：{sorted(k for k in keys if keys.count(k) > 1)}"
        raise AssertionError(detail)
    return rows


def write_baseline(rows: list[dict[str, object]]) -> None:
    note = (
        "由 static_check.py --update-test-baseline 生成。删除/改名/放宽断言/新增 ignore 或平台门禁时"
        "必须重新生成本文件，并在提交信息说明理由；这是防止「为变绿而削弱测试」的门禁。"
    )
    payload = {"note": note, "tests": sorted(rows, key=lambda row: (row["file"], row["name"]))}
    _ = (ROOT / "scripts" / "test-baseline.json").write_text(
        json.dumps(payload, ensure_ascii=False, indent=2) + "\n",
        newline="\n",
        encoding="utf-8",
    )


def _baseline_rows(path: Path) -> list[dict[str, object]]:
    loaded: object = _parse_json(read_text(path))
    if not _is_str_obj_map(loaded):
        detail = "scripts/test-baseline.json 顶层必须是对象"
        raise AssertionError(detail)
    tests = loaded["tests"]
    if not _is_obj_list(tests):
        detail = "scripts/test-baseline.json 的 tests 必须是数组"
        raise AssertionError(detail)
    rows: list[dict[str, object]] = []
    for item in tests:
        if not _is_str_obj_map(item):
            detail = "scripts/test-baseline.json 的测试项必须是对象"
            raise AssertionError(detail)
        rows.append(item)
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

    def val(row: dict[str, object]) -> bool:
        return bool(row.get("ignored"))

    base = {key(row): val(row) for row in _baseline_rows(path)}
    now = {key(row): val(row) for row in collect_tests()}
    added = sorted(set(now) - set(base))
    removed = sorted(set(base) - set(now))
    changed = sorted(k for k in set(base) & set(now) if base[k] != now[k])
    if added or removed or changed:
        raise AssertionError(
            {
                "新增测试(更新基线并在提交信息说明覆盖点)": added,
                "删除或改名(提交信息必须说明理由)": removed,
                "ignore状态翻转(提交信息必须说明理由)": [f"{k}: ignored {base[k]} -> {now[k]}" for k in changed],
                "更新命令": "python scripts/static_check.py --update-test-baseline",
            }
        )
    return f"{len(now)} 个测试与 scripts/test-baseline.json 完全一致（含 ignore 与平台门禁状态）。"


def slint_blocks(ui: str) -> list[tuple[int, int, str]]:
    # 列表元素为 (open_pos, close_pos, 块头标识符)：块头是紧贴 { 前的最后一个标识符（如 Rectangle）。
    pairs: list[tuple[int, int, str]] = []
    stack: list[tuple[int, str]] = []
    for i, ch in enumerate(ui):
        if ch == "{":
            head = re.search(r"([A-Za-z_][\w-]*)\s*$", ui[max(0, i - 80) : i])
            stack.append((i, head.group(1) if head else ""))
        elif ch == "}":
            open_pos, head = stack.pop()
            pairs.append((open_pos, i, head))
    return pairs


def enclosing(pairs: list[tuple[int, int, str]], pos: int) -> tuple[int, int, str] | None:
    best: tuple[int, int, str] | None = None
    for open_pos, close_pos, head in pairs:
        if open_pos < pos < close_pos and (best is None or close_pos - open_pos < best[1] - best[0]):
            best = (open_pos, close_pos, head)
    return best


LAYOUTS = {"HorizontalLayout", "VerticalLayout", "GridLayout"}


def slint_layout_width() -> str:
    # AGENTS.md 第 4 节：布局容器直接子项不得用 root/parent.width 绑定自身宽度（绑定环）。
    # 匹配位置在元素 E 自身块内，E 的父块（再上溯一层）是布局时才违规；绝对定位下的填充用法不受影响。
    ui = read_text(ROOT / "ui" / "app.slint")
    pairs = slint_blocks(ui)
    bad: list[tuple[int, str]] = []
    for match in re.finditer(r"(?:preferred-|min-|max-)?width\s*:\s*(root|parent)\.width\b", ui):
        inner = enclosing(pairs, match.start())
        outer = enclosing(pairs, inner[0]) if inner else None
        if outer and outer[2] in LAYOUTS:
            bad.append((ui.count("\n", 0, match.start()) + 1, match.group(0).strip()))
    if bad:
        detail = f"布局直接子项的宽度绑定了 root/parent.width（第 行, 表达式）：{bad}"
        raise AssertionError(detail)
    return "布局容器内没有用 root/parent.width 绑定子项自身宽度。"


def slint_colors() -> str:
    # AGENTS.md 第 4 节：十六进制颜色只允许出现在 Design 全局内（浅深主题由 Design.dark 切换）。
    ui = read_text(ROOT / "ui" / "app.slint")
    pairs = slint_blocks(ui)
    match = re.search(r"global\s+Design\s*\{", ui)
    if match is None:
        detail = "找不到 Design 全局定义"
        raise AssertionError(detail)
    design = [pair for pair in pairs if pair[0] == match.end() - 1]
    if not design:
        detail = "Design 块花括号配对异常"
        raise AssertionError(detail)
    open_pos, close_pos, _ = design[0]
    bad = [
        (ui.count("\n", 0, hex_match.start()) + 1, hex_match.group(0))
        for hex_match in re.finditer(r"#[0-9a-fA-F]{3,8}\b", ui)
        if not (open_pos < hex_match.start() < close_pos)
    ]
    if bad:
        detail = f"Design 全局之外出现硬编码十六进制颜色（行, 颜色）：{bad}"
        raise AssertionError(detail)
    return "十六进制颜色只出现在 Design 全局内。"


def slint_accessibility() -> str:
    # AGENTS.md 第 4 节：自绘可交互控件必须有 accessible-role。这里检查下界：
    # 任何包含 TouchArea 的组件定义（含 AppWindow）本身必须声明 accessible-role。
    ui = read_text(ROOT / "ui" / "app.slint")
    pairs = slint_blocks(ui)
    missing: list[str] = []
    for match in re.finditer(r"(?:export\s+)?component\s+([\w-]+)[^{]*\{", ui):
        span = [pair for pair in pairs if pair[0] == match.end() - 1]
        if not span:
            continue
        _, close, _ = span[0]
        body = ui[match.end() - 1 : close]
        if "TouchArea" in body and "accessible-role" not in body:
            missing.append(match.group(1))
    if missing:
        detail = f"含 TouchArea 的组件缺少 accessible-role：{missing}"
        raise AssertionError(detail)
    return "所有含 TouchArea 的组件都声明了 accessible-role。"


def product_naming() -> str:
    # AGENTS.md 第 1/5 节的产品名同步下界：标题与状态目录必须是 JchTools，且无旧名残留。
    ui = read_text(ROOT / "ui" / "app.slint")
    config = read_text(ROOT / "src" / "config.rs")
    if not re.search(r'title:\s*"JchTools"', ui):
        detail = "窗口标题必须是产品名 JchTools"
        raise AssertionError(detail)
    if '"JchTools"' not in config:
        detail = 'src/config.rs 的用户数据目录名必须是 "JchTools"'
        raise AssertionError(detail)
    for rel in ["ui/app.slint", "src/config.rs", "src/registry.rs", "Cargo.toml"]:
        if "MyTools" in read_text(ROOT / rel):
            detail = f"{rel} 残留旧产品名 MyTools"
            raise AssertionError(detail)
    return "窗口标题、状态目录与 Cargo 清单中的产品名一致（JchTools），无旧名残留。"


def slint_modal_gating() -> str:
    # AGENTS.md 第 4 节与仓库既有先例（app.slint 内「键盘可穿透确认框」修复注释）：
    # 确认模态（confirm-kind != 0）的 scrim 只挡鼠标，Tab 焦点仍可到达被遮挡控件；
    # AppWindow 内（确认层之前）所有可交互控件的 enabled 绑定都必须门禁 confirm-kind。
    ui = read_text(ROOT / "ui" / "app.slint")
    start = ui.index("export component AppWindow inherits Window")
    overlay = ui.index("if root.confirm-kind != 0:", start)
    # 冲突层是模态层，其自身控件在 conflict-visible 打开时必须可用，不适用背景门禁
    # （它们只受 confirm-kind 门禁，见 slint_conflict_modal_gating）。
    conflict = ui.find("if root.conflict-visible:", start)
    if conflict != -1:
        overlay = min(overlay, conflict)
    start_line = ui.count("\n", 0, start) + 1
    bad: list[tuple[int, str]] = []
    for offset, line in enumerate(ui[start:overlay].split("\n")):
        # 只解析 enabled 绑定表达式本身（排除 accessible-enabled 与属性声明）；
        # 「同行的 if 渲染条件里有 busy 字样」不再作为豁免依据——冲突弹出时 busy 恒真，
        # if root.busy: 控件恰在此时渲染，恰恰是最需要 conflict-visible 门禁的形态。
        enabled_match = re.search(r"(?<![\w-])enabled\s*:(.*)", line)
        if not enabled_match:
            continue
        expr = enabled_match.group(1)
        # confirm-kind 门禁只防确认模态：冲突模态（conflict-visible）下还需要
        # enabled 含 !root.busy / conflict-visible，或本行渲染条件保证 !root.busy
        # （该控件在冲突弹出时根本不渲染）。
        if "confirm-kind" not in expr or not (
            re.search(r"!\s*root\.busy", expr)
            or "conflict-visible" in expr
            or re.search(r"!\s*root\.busy", line[: enabled_match.start()])
        ):
            bad.append((start_line + offset, line.strip()))
    if bad:
        detail = f"AppWindow 内存在未按 confirm-kind 门禁的 enabled 绑定（行, 表达式）：{bad}"
        raise AssertionError(detail)
    return "AppWindow 内（确认层之前）所有可交互控件的 enabled 绑定都门禁 confirm-kind。"


def build_rc_prefers_windows_kits() -> str:
    # 与 process.rs 的 system_tool 防 PATH 劫持口径一致：build.rs 解析 rc.exe 必须
    # 优先 Windows SDK 目录；PATH 只能作为回退（且回退时构建日志应有告警）。
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


def slint_conflict_modal_gating() -> str:
    # 冲突层画在确认层之下（app.slint 注释自证 kind=3 确认层会盖在其上）：确认模态
    # 打开期间，冲突对话框的全部可交互控件必须禁用，否则键盘可穿透确认层直接改
    # 冲突策略或取消任务。slint_modal_gating 只覆盖「有 enabled 绑定」的行，本规则
    # 补上冲突层内无 enabled 绑定的控件这一盲区。
    ui = read_text(ROOT / "ui" / "app.slint")
    match = re.search(r"if root\.conflict-visible:.*?\{", ui)
    if match is None:
        detail = "找不到冲突层声明"
        raise AssertionError(detail)
    depth = 0
    end = len(ui)
    for i, ch in enumerate(ui[match.end() - 1 :]):
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth == 0:
                end = match.end() - 1 + i
                break
    body = ui[match.end() : end]
    bad = [
        line.strip()
        for line in body.split("\n")
        if re.search(r"\b(Button|ComboBox|CheckBox|LineEdit) \{", line) and "confirm-kind" not in line
    ]
    if bad:
        detail = f"冲突对话框内存在未按 confirm-kind 门禁的可交互控件：{bad}"
        raise AssertionError(detail)
    return "冲突对话框内全部可交互控件都门禁 confirm-kind。"


def _listed_digests(raw: bytes) -> dict[str, str]:
    listed: dict[str, str] = {}
    for line in raw.decode("utf-8").splitlines():
        if not line.strip():
            continue
        digest, name = line.split(" ", 1)
        if name[:1] in ("*", " "):
            name = name[1:]  # 兼容 sha256sum -b 的 *name 与文本模式的双空格 name
        listed[name] = digest.lower()
    return listed


def _tracked_paths(out: bytes) -> list[str]:
    tracked = [p.decode("utf-8").replace("\\", "/") for p in out.split(b"\0") if p]
    return [p for p in tracked if p != "SHA256SUMS.txt"]


def _missing_worktree_files(expected: list[str]) -> list[str]:
    return [name for name in expected if not (ROOT / name).is_file()]


def _stale_files(expected: list[str], listed: dict[str, str]) -> list[str]:
    return [
        name
        for name in expected
        if (ROOT / name).is_file() and hashlib.sha256((ROOT / name).read_bytes()).hexdigest() != listed[name]
    ]


def sums_integrity() -> str:
    # SHA256SUMS.txt 契约：覆盖除自身外的全部 git 跟踪文件、哈希与
    # 工作树一致、LF 行尾（CRLF 会让 `sha256sum -c` 在 Git Bash 下整单失败）。
    raw = (ROOT / "SHA256SUMS.txt").read_bytes()
    if b"\r" in raw:
        detail = "SHA256SUMS.txt 必须是 LF 行尾（CRLF 会让 sha256sum -c 全部失败）"
        raise AssertionError(detail)
    listed = _listed_digests(raw)
    # 解析 PATH 上 git 的绝对路径：避免用部分可执行名启动进程（S607）。
    git = shutil.which("git")
    if git is None:
        message = "git 不可用，跳过：PATH 上找不到 git"
        raise SkippedError(message)
    try:
        out = subprocess.run(
            [git, "-c", "core.quotePath=false", "ls-files", "-z"], cwd=ROOT, capture_output=True, check=True
        ).stdout
    except Exception as exc:
        message = f"git 不可用，跳过：{exc}"
        raise SkippedError(message) from exc
    expected = _tracked_paths(out)
    missing = sorted(set(expected) - set(listed))
    if missing:
        detail = f"跟踪文件未列入 SHA256SUMS.txt（用 sha256sum -b 重建）：{missing}"
        raise AssertionError(detail)
    extra = sorted(set(listed) - set(expected))
    if extra:
        detail = f"清单含未跟踪/多余条目：{extra}"
        raise AssertionError(detail)
    gone = _missing_worktree_files(expected)
    if gone:
        detail = f"跟踪文件已从工作树删除（先恢复文件或 git add -A 后重建清单）：{gone}"
        raise AssertionError(detail)
    stale = _stale_files(expected, listed)
    if stale:
        detail = f"哈希与工作树不一致（文件已改，需重建 SHA256SUMS.txt）：{stale}"
        raise AssertionError(detail)
    return f"{len(expected)} 个跟踪文件的 SHA256 全部与工作树一致（LF 行尾）。"


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
    for path in ROOT.glob("src/**/*.rs"):
        text = read_text(path)
        if "todo!(" in text or "unimplemented!(" in text:
            raise AssertionError(str(path))
    tests = sum(len(re.findall(r"#\[test\]", read_text(p))) for p in ROOT.glob("tests/**/*.rs"))
    return (
        f"{tests} Rust test functions supplied, NOT executed; no todo!/unimplemented! in Rust implementation; "
        "no prebuilt executable asserted."
    )


if UPDATE_BASELINE:
    rows = collect_tests()
    write_baseline(rows)
    print(f"baseline updated: {len(rows)} tests -> scripts/test-baseline.json")
    sys.exit(0)

for name, fn in [
    ("manifests", manifests),
    ("config_schema", config_schema),
    ("ui_callbacks", ui_callbacks),
    ("rust_lexical", rust_lexical),
    ("sql_syntax", sql_syntax),
    ("shell_syntax", shell_syntax),
    ("ps1_utf8_bom", ps1_utf8_bom),
    ("test_baseline", test_baseline),
    ("slint_layout_width", slint_layout_width),
    ("slint_colors", slint_colors),
    ("slint_accessibility", slint_accessibility),
    ("product_naming", product_naming),
    ("slint_modal_gating", slint_modal_gating),
    ("build_rc_prefers_windows_kits", build_rc_prefers_windows_kits),
    ("acceptance_respects_cargo_target_dir", acceptance_respects_cargo_target_dir),
    ("slint_conflict_modal_gating", slint_conflict_modal_gating),
    ("sums_integrity", sums_integrity),
    ("scope_and_delivery", scope_and_delivery),
]:
    check(name, fn)

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
sys.exit(any(entry["status"] == "FAIL" for entry in checks))
