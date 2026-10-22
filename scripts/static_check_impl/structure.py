"""static_check 结构与配置检查：清单、规则、UI 回调、词法、SQL、Shell 与 PS1 BOM（自 static_check.py 搬移）."""

from __future__ import annotations

import re
import shutil
import sqlite3
import subprocess
from typing import TYPE_CHECKING

import defusedxml.ElementTree

from scripts.static_check_impl.common import (
    ASCII_LIMIT,
    BASH_COMMAND_NOT_FOUND,
    MIN_CHOICE_COUNT,
    PRODUCTION_CRATES,
    ROOT,
    SkippedError,
    is_obj_list,
    is_str,
    is_str_obj_map,
    manifest_targets,
    parse_json,
    parse_toml,
    read_text,
)

if TYPE_CHECKING:
    from collections.abc import Callable
    from pathlib import Path

# 清单来源函数定义在入口 static_check.py（test_review_trusted 的 AST 回归按入口命名空间
# 覆盖 ROOT 并 exec 真实声明），由入口在导入后注入本属性。
project_manifests: Callable[[], list[Path]]


def manifests() -> str:
    paths = project_manifests()  # noqa: F821  # 定义在入口 static_check.py，由入口导入后注入本模块属性。
    for path in paths:
        cargo_value: object = parse_toml(read_text(path))
        if not is_str_obj_map(cargo_value):
            detail = f"{path.relative_to(ROOT)} 顶层必须是表"
            raise AssertionError(detail)
        if path == ROOT / "Cargo.toml":
            if not is_obj_list(cargo_value.get("bin")):
                detail = "Cargo.toml 的 bin 必须是数组"
                raise AssertionError(detail)
            workspace = cargo_value.get("workspace")
            if not is_str_obj_map(workspace):
                detail = "Cargo.toml 的 workspace 必须是表"
                raise AssertionError(detail)
            default_members = workspace.get("default-members")
            expected = {".", *(f"crates/{name}" for name in PRODUCTION_CRATES)}
            if not is_obj_list(default_members) or set(default_members) != expected:
                detail = f"default-members 必须完整覆盖根与生产 crates，不包含 optional：{default_members}"
                raise AssertionError(detail)
        manifest_targets(path, cargo_value)
    _ = defusedxml.ElementTree.parse(ROOT / "resources/windows.manifest")
    summary = f"{len(paths)} project Cargo TOML files, declared target/build paths,"
    return f"{summary} default members and Windows XML parsed."


def _rules_rows() -> list[dict[str, object]]:
    rules_value: object = parse_json(read_text(ROOT / "resources/rules.json"))
    if not is_obj_list(rules_value):
        detail = "resources/rules.json 顶层必须是数组"
        raise AssertionError(detail)
    rows: list[dict[str, object]] = []
    for item in rules_value:
        if not is_str_obj_map(item):
            detail = "resources/rules.json 的规则项必须是对象"
            raise AssertionError(detail)
        rows.append(item)
    return rows


def _choice_values(choices: list[object]) -> list[str]:
    if len(choices) < MIN_CHOICE_COUNT:
        raise AssertionError
    values: list[str] = []
    for choice in choices:
        if not is_obj_list(choice):
            detail = "resources/rules.json 的 choice 项必须是列表"
            raise AssertionError(detail)
        first = choice[0]
        if not is_str(first):
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
    if not is_obj_list(choices):
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
    if not is_str(key_value):
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
    config = read_text(ROOT / "crates/file-core/src/config.rs")
    match = re.search(r"pub struct Config\s*\{(.*?)\n\}", config, re.DOTALL)
    if match is None:
        detail = "crates/file-core/src/config.rs 中找不到 pub struct Config 定义"
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
    # GUI 组装层在真实 gui crate；bin main.rs 只是薄壳入口。
    rs = chr(10).join(read_text(path) for path in [ROOT / "src/main.rs", *sorted(ROOT.glob("crates/gui/src/**/*.rs"))])
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


def check_delimiters(path: Path, text: str, stack: list[str]) -> None:
    for ch in text:
        if ch in "([{":
            stack.append(ch)
        elif ch in ")]}" and not (stack and "([{".index(stack.pop()) == ")]}".index(ch)):
            raise AssertionError(str(path.relative_to(ROOT)))


def sql_syntax() -> str:
    conn = sqlite3.connect(":memory:")
    _ = conn.executescript(read_text(ROOT / "crates/file-core/src/schema.sql"))
    # XB-18：直接读取产品使用的应用配置 schema，避免检查器另造一份表定义。
    _ = conn.executescript(read_text(ROOT / "crates/foundation/src/app_settings.sql"))
    _ = conn.executescript("""CREATE TEMP TABLE duplicate_order(seq INTEGER,id INTEGER);
        CREATE TEMP TABLE empty_order(seq INTEGER,rel TEXT);
        CREATE TEMP TABLE empty_will(rel TEXT PRIMARY KEY);
        CREATE TEMP TABLE stay_parents(parent TEXT);
        CREATE TEMP TABLE dir_children(parent TEXT,rel TEXT);
        CREATE TEMP TABLE hash_candidates(id INTEGER PRIMARY KEY);
        CREATE TEMP TABLE scan_taint(rel TEXT PRIMARY KEY);
        CREATE TEMP TABLE git_roots(rel TEXT PRIMARY KEY);
        CREATE TEMP TABLE tainted_will(rel TEXT PRIMARY KEY);
        CREATE TEMP TABLE organizer_created_dirs(rel TEXT PRIMARY KEY);
        CREATE TEMP TABLE doomed_sources(rel TEXT PRIMARY KEY);
        CREATE TABLE hash_cache(identity TEXT NOT NULL, size INTEGER NOT NULL, mtime INTEGER NOT NULL,
            hash TEXT NOT NULL, updated INTEGER NOT NULL, PRIMARY KEY(identity,size,mtime));""")
    file_columns = "id,rel,name,normal,size,mtime,identity,links,hash,cleanable"
    statements: set[str] = set()
    # SQL 判据保持原生产实现范围；独立 crate 的真实实现取代原 src/，
    # 不把 optional 热键字面量 "INSERT"/"DELETE" 或测试输入误当成产品 SQL。
    for path in sorted([*ROOT.glob("src/**/*.rs"), *ROOT.glob("crates/*/src/**/*.rs")]):
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
            "not found" in (done.stderr or "").lower() or done.returncode == BASH_COMMAND_NOT_FOUND
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
        if raw.startswith(b"\xef\xbb\xbf") or all(byte < ASCII_LIMIT for byte in raw):
            continue
        missing.append(path.name)
    if missing:
        detail = f"scripts/*.ps1 含非 ASCII 内容时必须带 UTF-8 BOM（否则多字节 ANSI 代码页下会被误解码）：{missing}"
        raise AssertionError(detail)
    return f"{len(scripts)} 个 .ps1 均带 UTF-8 BOM 或为纯 ASCII；PowerShell 5.1 解码不受 ANSI 代码页影响。"
