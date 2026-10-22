"""BEP 证据解析与缓存日志、Cargo 声明产物恢复原语.

从 bazel_gate 根模块拆出的纯解析层：只依赖标准库、bazel_action 与
gate_runtime，不回读根模块，`python -m scripts.bazel_gate` 的入口语义不变。
"""

from __future__ import annotations

import json
from datetime import datetime
from pathlib import Path
from typing import TYPE_CHECKING, NotRequired, TypedDict, cast
from urllib.parse import urlparse
from urllib.request import url2pathname

from scripts.bazel_action import restore_cargo_artifacts, test_log_payload

if TYPE_CHECKING:
    from scripts.gate_runtime import Stage


class _BepFile(TypedDict):
    name: NotRequired[str]
    uri: NotRequired[str]


class _BepResult(TypedDict):
    status: NotRequired[str]
    cachedLocally: NotRequired[bool]
    cachedRemotely: NotRequired[bool]
    testAttemptDurationMillis: NotRequired[int | str]
    testActionOutput: NotRequired[list[_BepFile]]


class _BepSummary(TypedDict):
    overallStatus: NotRequired[str]
    numCached: NotRequired[int]
    passed: NotRequired[list[_BepFile]]
    failed: NotRequired[list[_BepFile]]


class _BepFileSet(TypedDict):
    id: str


class _BepNamedSet(TypedDict):
    files: NotRequired[list[_BepFile]]
    fileSets: NotRequired[list[_BepFileSet]]


class _BepOutputGroup(TypedDict):
    name: NotRequired[str]
    fileSets: NotRequired[list[_BepFileSet]]
    incomplete: NotRequired[bool]


class _BepCompleted(TypedDict):
    success: NotRequired[bool]
    outputGroup: NotRequired[list[_BepOutputGroup]]


class _BepAction(TypedDict):
    success: NotRequired[bool]
    type: NotRequired[str]
    exitCode: NotRequired[int]
    primaryOutput: NotRequired[_BepFile]
    startTime: NotRequired[str]
    endTime: NotRequired[str]


class _BepEvent(TypedDict):
    id: dict[str, dict[str, str]]
    testResult: NotRequired[_BepResult]
    testSummary: NotRequired[_BepSummary]
    completed: NotRequired[_BepCompleted]
    namedSetOfFiles: NotRequired[_BepNamedSet]
    action: NotRequired[_BepAction]


class _Evidence(TypedDict):
    result: NotRequired[_BepResult]
    summary: NotRequired[_BepSummary]
    completed: NotRequired[_BepCompleted]
    outputs: NotRequired[list[_BepFile]]
    actions: NotRequired[list[_BepAction]]


def _file_uri(uri: str) -> Path:
    parsed = urlparse(uri)
    path = url2pathname(parsed.path)
    if parsed.netloc:
        path = "//" + parsed.netloc + path
    return Path(path)


def _collect_event(
    event: _BepEvent,
    evidence: dict[str, _Evidence],
    sets: dict[str, _BepNamedSet],
    actions: list[tuple[str, str, _BepAction]],
) -> None:
    identifier = event.get("id", {})
    named = identifier.get("namedSet", {})
    if "namedSetOfFiles" in event and "id" in named:
        sets[named["id"]] = event["namedSetOfFiles"]
    action_id = identifier.get("actionCompleted", {})
    if "action" in event:
        actions.append((action_id.get("label", ""), action_id.get("primaryOutput", ""), event["action"]))
    target_id = identifier.get("testResult") or identifier.get("testSummary") or identifier.get("targetCompleted")
    if not isinstance(target_id, dict) or not isinstance(target_id.get("label"), str):
        return
    entry = evidence.setdefault(target_id["label"], {})
    if "testResult" in event:
        entry["result"] = event["testResult"]
    if "testSummary" in event:
        entry["summary"] = event["testSummary"]
    if "completed" in event:
        entry["completed"] = event["completed"]


def _output_files(completed: _BepCompleted, sets: dict[str, _BepNamedSet]) -> list[_BepFile]:
    outputs: list[_BepFile] = []
    pending = [reference["id"] for group in completed.get("outputGroup", []) for reference in group.get("fileSets", [])]
    visited: set[str] = set()
    while pending:
        key = pending.pop()
        if key in visited:
            continue
        visited.add(key)
        named_set = sets.get(key, {})
        outputs.extend(named_set.get("files", []))
        pending.extend(reference["id"] for reference in named_set.get("fileSets", []))
    return outputs


def _events(path: Path) -> dict[str, _Evidence]:
    evidence: dict[str, _Evidence] = {}
    sets: dict[str, _BepNamedSet] = {}
    actions: list[tuple[str, str, _BepAction]] = []
    if not path.exists():
        return evidence
    # 只预筛与本轮证据相关的事件键；不含这些键的行不可能进入 _collect_event。
    relevant = (
        b'"namedSetOfFiles"',
        b'"action"',
        b'"testResult"',
        b'"testSummary"',
        b'"targetCompleted"',
        b'"completed"',
    )
    with path.open("rb") as source:
        for line in source:
            if not any(key in line for key in relevant):
                continue
            try:
                event = cast("_BepEvent", json.loads(line))
            except ValueError:
                continue  # 强制终止可能使最后一行 BEP 不完整；不据此推断成功。
            _collect_event(event, evidence, sets, actions)
    for label, entry in evidence.items():
        entry["outputs"] = _output_files(entry.get("completed", {}), sets)
        output_name = label.removeprefix("//").replace(":", "/") + ".command.log"
        entry["actions"] = [
            action
            for action_label, primary, action in actions
            if action.get("type") == "GateCommand"
            and (
                action_label == label
                or (
                    not action_label
                    and any(
                        value.replace("\\", "/").endswith("/" + output_name) or value.replace("\\", "/") == output_name
                        for value in (primary, action.get("primaryOutput", {}).get("name", ""))
                    )
                )
            )
        ]
    return evidence


def _restore_log(log: Path, event: _BepResult, summary: _BepSummary, *, cached: bool) -> bool:
    if log.exists() and not cached:
        return True
    # 缓存消费者只接受完整适配器帧，Bazel 原生 wrapper 留在原生证据中。
    outputs = event.get("testActionOutput", []) + summary.get("passed", []) + summary.get("failed", [])
    for output in outputs:
        uri = output.get("uri", "")
        if (output.get("name") == "test.log" or "name" not in output) and uri.startswith("file:"):
            source = _file_uri(uri)
            if source.is_file():
                try:
                    payload = test_log_payload(source.read_bytes())
                except (OSError, ValueError):
                    return False
                _ = log.write_bytes(payload)
                return True
    return False


def _action_elapsed(action: _BepAction) -> float:
    start, end = action.get("startTime"), action.get("endTime")
    if start is None or end is None:
        return 0.0
    return max(0.0, (datetime.fromisoformat(end) - datetime.fromisoformat(start)).total_seconds())


def _declared_file(entry: _Evidence, name: str) -> Path | None:
    for output in entry.get("outputs", []):
        uri = output.get("uri", "")
        if output.get("name", "").replace("\\", "/").rsplit("/", 1)[-1] == name and uri.startswith("file:"):
            path = _file_uri(uri)
            if path.is_file():
                return path
    return None


def _cargo_manifest(stage: Stage, entry: _Evidence) -> Path:
    manifest = _declared_file(entry, "run.cargo.json")
    if manifest is None:
        message = f"{stage.name}: Cargo 动作缺少声明产物清单"
        raise ValueError(message)
    return manifest


def _restore_declared_cargo(root: Path, entry: _Evidence) -> None:
    manifest = _declared_file(entry, "run.cargo.json")
    if manifest is None:
        message = "Cargo 动作缺少原生声明产物清单，不能仅凭缓存日志通过。"
        raise ValueError(message)
    restore_cargo_artifacts(root, manifest.with_suffix(""), manifest)
