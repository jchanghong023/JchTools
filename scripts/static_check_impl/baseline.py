"""static_check 测试基线机制：Rust 测试发现解析、门禁抽取与基线读写（自 static_check.py 原样搬移）.

说明：collect_tests/test_baseline/_collect_file_tests 留在入口 static_check.py——
test_review_trusted 的 AST 回归按入口命名空间 exec 真实声明并覆盖 ROOT，
这三个函数（及其 ROOT 依赖链）必须从入口命名空间解析全局。
"""

from __future__ import annotations

import hashlib
import json
import re
from typing import TYPE_CHECKING

from scripts.static_check_impl.common import (
    ROOT,
    is_obj_list,
    is_str_obj_map,
    parse_json,
    read_text,
)

if TYPE_CHECKING:
    from pathlib import Path


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
    # 完整匹配属性名与括号，避免把 cfg_attr 或其他 cfg 前缀属性当作门禁。
    match = re.fullmatch(r"cfg\s*\((.*)\)", inner.strip(), re.DOTALL)
    if match is None:
        return None
    payload = match.group(1).strip()
    return "" if payload == "test" else payload


def file_level_cfgs(head: str) -> list[str]:
    cfgs: list[str] = []
    for match in re.finditer(r"#!\[", mask_rust_trivia(head)):
        got = _attr_body(head, match.start())
        if got:
            payload = _cfg_payload(got[0])
            if payload and payload not in cfgs:
                cfgs.append(payload)
    return cfgs


def mod_gates(text: str) -> list[tuple[int, int, str]]:
    spans: list[tuple[int, int, str]] = []
    structural = mask_rust_trivia(text)
    attrs = attr_spans(text)
    for match in re.finditer(r"(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{", structural):
        end = _brace_end(structural, match.end() - 1)
        for _, _, inner in _walk_attrs(text, attrs, match.start(), -1):
            payload = _cfg_payload(inner)
            if payload:
                spans.append((match.end(), end, payload))
    return spans


def attr_spans(text: str) -> list[tuple[int, int, str]]:
    spans: list[tuple[int, int, str]] = []
    for match in re.finditer(r"#\[", mask_rust_trivia(text)):
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


def test_gates(
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


# Rust 源里的注释与字符串/字符字面量（花括号配对前先抹除干扰；保持等长以便位置对齐）。
# 近似实现：不处理嵌套块注释；原始字符串按开头 # 数量匹配结束分隔符。
_RUST_TRIVIA_ALTS = (
    r"//[^\n]*",
    r"/\*.*?\*/",
    r'b?r(?P<raw_hashes>#{0,})".*?"(?P=raw_hashes)',
    r'b?"(?:\\.|[^"\\\n])*"',
    r"b?'(?:\\.|[^'\\\n])'",
)
_RUST_TRIVIA = re.compile("|".join(_RUST_TRIVIA_ALTS), re.DOTALL)


def mask_rust_comments(text: str) -> str:
    """等长抹除 Rust 注释（含嵌套块），保留字面量及换行，供属性与门禁定位."""
    parts: list[str] = []
    cursor = search_at = 0
    while match := _RUST_TRIVIA.search(text, search_at):
        start, end = match.span()
        if not match.group().startswith(("//", "/*")):
            search_at = end
            continue
        if text.startswith("/*", start):
            depth, end = 1, start + 2
            while depth:
                opening, closing = text.find("/*", end), text.find("*/", end)
                if closing == -1:
                    end = len(text)
                    break
                if opening != -1 and opening < closing:
                    depth += 1
                    end = opening + 2
                else:
                    depth -= 1
                    end = closing + 2
        parts.append(text[cursor:start])
        parts.append(re.sub(r"[^\n]", " ", text[start:end]))
        cursor = search_at = end
    parts.append(text[cursor:])
    return "".join(parts)


def mask_rust_trivia(text: str) -> str:
    """先处理嵌套注释，再等长遮蔽字面量；保留换行以定位真实 Rust 结构."""
    return _RUST_TRIVIA.sub(
        lambda match: re.sub(r"[^\n]", " ", match.group()),
        mask_rust_comments(text),
    )


def test_body_hash(text: str, signature_end: int) -> str:
    """取普通或 Tokio 异步测试函数体内容并哈希（sha256 前 16 位十六进制）.

    签名（参数/返回值/where 子句）内不含花括号，函数体从其后第一个 "{" 起；
    在剔除注释与字面量后的等长文本上做朴素配对找闭合 "}"，对原文切片哈希。
    找不到闭合时退化为「到文件尾」的哈希——仍然确定，只会更敏感。
    """
    scrubbed = mask_rust_trivia(text)
    open_at = scrubbed.find("{", signature_end)
    if open_at == -1:
        blob = text[signature_end:]
    else:
        depth, end = 0, -1
        for i in range(open_at, len(scrubbed)):
            if scrubbed[i] == "{":
                depth += 1
            elif scrubbed[i] == "}":
                depth -= 1
                if depth == 0:
                    end = i
                    break
        blob = text[signature_end:] if end == -1 else text[open_at : end + 1]
    return hashlib.sha256(blob.encode("utf-8")).hexdigest()[:16]


def write_baseline(rows: list[dict[str, object]]) -> None:
    note = (
        "由 static_check.py --update-test-baseline 生成。删除/改名/新增 ignore 或平台门禁、"
        "以及任何测试体修改（含放宽或清空断言）都必须重新生成本文件，并在提交信息说明理由；"
        "body 哈希用于防止「保留测试名、掏空测试体」式削弱。"
    )
    payload = {"note": note, "tests": sorted(rows, key=lambda row: (row["file"], row["name"]))}
    _ = (ROOT / "scripts" / "test-baseline.json").write_text(
        json.dumps(payload, ensure_ascii=False, indent=2) + "\n",
        newline="\n",
        encoding="utf-8",
    )


def load_baseline_rows(path: Path) -> list[dict[str, object]]:
    loaded: object = parse_json(read_text(path))
    if not is_str_obj_map(loaded):
        detail = "scripts/test-baseline.json 顶层必须是对象"
        raise AssertionError(detail)
    tests = loaded["tests"]
    if not is_obj_list(tests):
        detail = "scripts/test-baseline.json 的 tests 必须是数组"
        raise AssertionError(detail)
    rows: list[dict[str, object]] = []
    for item in tests:
        if not is_str_obj_map(item):
            detail = "scripts/test-baseline.json 的测试项必须是对象"
            raise AssertionError(detail)
        rows.append(item)
    return rows
