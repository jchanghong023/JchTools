"""报告原生证据，不影响执行、成功判定或缓存决策.

上一轮报告仅用于诊断，不参与缓存。默认仅处理 BEP 与计时；cache-debug 才扫描
Git 工作树并采集 profile/explain/compact。所有方法均在门禁共享计时范围内调用。
"""

from __future__ import annotations

import gzip
import hashlib
import json
import os
import re
import shutil
import subprocess
import time
from datetime import datetime
from pathlib import Path
from typing import cast, final

Record = dict[str, object]
_SAFE_OPTIONS = frozenset(
    {
        "startup_time",
        "command_wait_time",
        "restart_reason",
        "config",
        "jobs",
        "cache_test_results",
        "test_timeout",
        "test_strategy",
        "test_output",
        "local_test_jobs",
        "local_resources",
        "keep_state_after_build",
        "nodiscard_analysis_cache",
        "noallow_analysis_cache_discard",
        "max_idle_secs",
    }
)


def _object(value: object) -> Record:
    if not isinstance(value, dict):
        message = "诊断 JSON 应为对象"
        raise TypeError(message)
    return cast("Record", value)


def _array(value: object) -> list[object]:
    if not isinstance(value, list):
        message = "诊断 JSON 应为数组"
        raise TypeError(message)
    return cast("list[object]", value)


def _number(value: object) -> float:
    if not isinstance(value, (str, int, float)):
        message = "原生指标应为数值"
        raise TypeError(message)
    return float(value)


def _text(value: object) -> str:
    if not isinstance(value, str):
        message = "诊断值应为字符串"
        raise TypeError(message)
    return value


def _load(text: str) -> Record:
    return _object(cast("object", json.loads(text)))


def _safe_options(argv: list[str]) -> Record:
    # 不复制客户端或动作环境变量、远程请求头或完整 argv。
    result: Record = {}
    for arg in argv:
        key, _, value = arg.removeprefix("--").partition("=")
        if arg.startswith("--") and key in _SAFE_OPTIONS:
            result[key] = value if re.fullmatch(r"[A-Za-z0-9_.+-]*", value) else "[已隐去]"
    return result


def _atomic_json(path: Path, value: Record) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    pending = path.with_name(path.name + f".{os.getpid()}.pending")
    _ = pending.write_text(json.dumps(value, ensure_ascii=True, indent=2) + "\n", encoding="utf-8")
    _ = pending.replace(path)


def _seconds(value: object) -> float | None:
    return None if value is None else _number(value) / 1000


def _elapsed(action: Record) -> float | None:
    start, end = action.get("startTime"), action.get("endTime")
    if not start or not end:
        return None
    return (datetime.fromisoformat(_text(end)) - datetime.fromisoformat(_text(start))).total_seconds()


def _profile(path: Path) -> Record:
    with gzip.open(path, "rt", encoding="utf-8") as stream:
        trace = cast("object", json.load(stream))
    events = _array(_object(cast("object", trace))["traceEvents"] if isinstance(trace, dict) else trace)
    groups: dict[str, list[tuple[float, float]]] = {
        "loading": [],
        "root_package": [],
        "fsvc_getDirtyKeys": [],
    }
    for item in events:
        event = _object(item)
        if event.get("ph") != "X" or "dur" not in event:
            continue
        name, category = str(event.get("name", "")), str(event.get("cat", ""))
        keys: list[str] = []
        if category.lower() in {"loading", "package loading", "package creation"} or name.lower() in {
            "loading phase",
            "load packages",
        }:
            keys.append("loading")
        if "fsvc.getDirtyKeys" in name:
            keys.append("fsvc_getDirtyKeys")
        if (category.lower() == "package creation" and not name) or re.search(
            r"(?:Creating|Loading|Create) package:?\s*(?:\[\]|//)?$", name, re.IGNORECASE
        ):
            keys.append("root_package")
        for key in keys:
            first = _number(event["ts"])
            groups[key].append((first, first + _number(event["dur"])))
    result: Record = {}
    for key, spans in groups.items():
        covered, end = 0.0, float("-inf")
        for first, last in sorted(spans):
            covered += max(0.0, last - max(first, end))
            end = max(end, last)
        result[key] = {
            "events": len(spans),
            "interval_union_seconds": covered / 1e6 if spans else None,
            "summed_event_seconds": sum(last - first for first, last in spans) / 1e6 if spans else None,
            "status": "observed" if spans else "pending: 无匹配的 trace 事件",
        }
    result["note"] = "trace 分组可能重叠；事件时长之和不等于实际耗时。"
    return result


def _fingerprint(path: Path, algorithm: str) -> Record:
    if path.is_symlink():
        data = os.fsencode(path.readlink())
        identity = hashlib.new(algorithm, b"blob " + str(len(data)).encode() + b"\0" + data)
        return {"sha256": hashlib.sha256(data).hexdigest(), "git_blob": identity.hexdigest()}
    if path.is_file():
        size = path.stat().st_size
        digest = hashlib.sha256()
        blob = hashlib.new(algorithm, b"blob " + str(size).encode() + b"\0")
        with path.open("rb") as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                digest.update(chunk)
                blob.update(chunk)
        return {"sha256": digest.hexdigest(), "git_blob": blob.hexdigest()}
    return {"pending": "目录或子模块未做字节比较"} if path.exists() else {"absent": True}


def _record_event(
    event: Record, record: Record, actions: list[Record], tests: list[Record], summaries: list[Record]
) -> None:
    if "started" in event:
        native = _object(event["started"])
        record["server"] = {"version": native.get("buildToolVersion"), "pid": native.get("serverPid")}
    if "unstructuredCommandLine" in event:
        native_argv = _array(_object(event["unstructuredCommandLine"]).get("args", []))
        options = _safe_options([_text(arg) for arg in native_argv])
        record["startup"] = {
            "startup_seconds": _seconds(options.get("startup_time")),
            "command_wait_seconds": _seconds(options.get("command_wait_time")),
            "restart_reason": options.get("restart_reason"),
            "source": "BEP unstructuredCommandLine 原生毫秒计数器",
        }
    if "buildMetrics" in event:
        metrics = _object(event["buildMetrics"])
        timing = _object(metrics.get("timingMetrics", {}))
        record["timing_seconds"] = {
            key: _seconds(timing.get(field))
            for key, field in (
                ("wall", "wallTimeInMs"),
                ("analysis", "analysisPhaseTimeInMs"),
                ("execution", "executionPhaseTimeInMs"),
                ("cpu", "cpuTimeInMs"),
            )
        }
        summary = _object(metrics.get("actionSummary", {}))
        action_data = [_object(item) for item in _array(summary.get("actionData", []))]
        record["action_counts"] = {
            "all": int(_number(summary.get("actionsExecuted", 0))),
            **{
                kind: sum(
                    int(_number(item.get("actionsExecuted", 0))) for item in action_data if item.get("mnemonic") == kind
                )
                for kind in ("GateCommand", "TestRunner")
            },
            "source": "BEP buildMetrics.actionSummary（非缓存或测试计数）",
        }
    if "action" in event:
        action = _object(event["action"])
        actions.append(
            {
                "label": action.get("label"),
                "type": action.get("type"),
                "seconds": _elapsed(action),
                "success": action.get("success"),
            }
        )
    if "testResult" in event:
        test = _object(event["testResult"])
        identity = _object(_object(event.get("id", {})).get("testResult", {}))
        cached = bool(test.get("cachedLocally") or test.get("cachedRemotely"))
        duration = _seconds(test.get("testAttemptDurationMillis"))
        tests.append(
            {
                "label": identity.get("label"),
                "run": identity.get("run"),
                "shard": identity.get("shard"),
                "attempt": identity.get("attempt"),
                "cached": cached,
                "status": test.get("status"),
                "runtime_seconds": 0.0 if cached else duration,
                "historical_duration_seconds": duration if cached else None,
            }
        )
    if "testSummary" in event:
        summary = _object(event["testSummary"])
        identity = _object(_object(event.get("id", {})).get("testSummary", {}))
        summaries.append(
            {
                "label": identity.get("label"),
                "status": summary.get("overallStatus"),
                "num_cached": int(_number(summary.get("numCached", 0))),
                "total_run_count": summary.get("totalRunCount"),
            }
        )


@final
class GateDiagnostics:
    """每次 CLI 运行生成一份报告，不隐藏原始证据或报告错误."""

    def __init__(self, root: Path, log_dir: Path, *, debug: bool = False) -> None:
        self.root = root.resolve()
        self.log_dir = log_dir.resolve()
        self.latest = self.root / ".tmp/bazel-gate/diagnostics/latest.json"
        self.debug = debug
        git = shutil.which("git")
        if git is None:
            message = "门禁诊断需要 Git"
            raise FileNotFoundError(message)
        self.git = git
        self.path = self.log_dir / "diagnostics.json"
        self.previous = _load(self.latest.read_text(encoding="utf-8")) if self.latest.exists() else None
        self.dispatches: list[Record] = []
        self.overheads: dict[str, float] = {}
        self._flags: dict[int, dict[str, Path]] = {}
        self.snapshot: Record = (
            self._snapshot() if debug else {"status": "disabled: 使用 cache-debug 采集 Git 字节快照", "files": {}}
        )
        self.changes = self._changes(_object(self.previous["snapshot"]) if self.previous else None, self.snapshot)

    def _git(self, *args: str) -> bytes:
        return subprocess.run(
            [self.git, "-C", str(self.root), *args],
            check=True,
            capture_output=True,
        ).stdout

    def _snapshot(self, current: Record | None = None) -> Record:
        probe = subprocess.run(
            [self.git, "-C", str(self.root), "rev-parse", "--show-toplevel"],
            check=False,
            capture_output=True,
        )
        if probe.returncode:
            if b"not a git repository" in probe.stderr.lower():
                return {"status": "pending: 非 Git 夹具", "files": {}}
            message = "Git 诊断仓库探测失败（未展示标准错误）"
            raise RuntimeError(message)
        if Path(os.fsdecode(probe.stdout).strip()).resolve() != self.root:
            return {"status": "pending: 根目录不是 Git 工作树", "files": {}}
        revision = subprocess.run(
            [self.git, "-C", str(self.root), "rev-parse", "--verify", "--quiet", "HEAD"],
            check=False,
            capture_output=True,
        )
        if revision.returncode == 1:
            return {"status": "pending: Git 夹具没有 HEAD", "files": {}}
        revision.check_returncode()
        head = revision.stdout.decode("ascii").strip()
        algorithm = self._git("rev-parse", "--show-object-format").decode("ascii").strip()
        entries = self._git("status", "--porcelain=v1", "-z", "--untracked-files=all", "--no-renames").split(b"\0")
        dirty = {os.fsdecode(entry[3:]) for entry in entries if entry}
        candidates = dirty.copy()
        previous = _object(self.previous["snapshot"]) if self.previous else {}
        candidates.update(_text(name) for name in _array(previous.get("dirty_paths", [])))
        if current:
            candidates.update(_object(current["files"]))
        if previous.get("head") and previous["head"] != head:
            candidates.update(
                os.fsdecode(name)
                for name in self._git(
                    "diff",
                    "--name-only",
                    "-z",
                    _text(previous["head"]),
                    head,
                    "--",
                ).split(b"\0")
                if name
            )
        files: Record = {name: _fingerprint(self.root / name, algorithm) for name in sorted(candidates)}
        return {
            "status": "observed",
            "head": head,
            "files": files,
            "dirty_paths": sorted(dirty),
            "coverage": "仅覆盖脏文件、未跟踪文件、上一轮候选及 HEAD 差异；其他输入待确认",
        }

    def _head_files(self, head: str, names: list[str]) -> Record:
        baseline: Record = {}
        for offset in range(0, len(names), 100):
            output = self._git("ls-tree", "-z", head, "--", *names[offset : offset + 100])
            for entry in output.split(b"\0"):
                if not entry:
                    continue
                metadata, name = entry.split(b"\t", 1)
                _, kind, identity = metadata.split()
                baseline[os.fsdecode(name)] = (
                    {"git_blob": identity.decode("ascii"), "baseline": "上一轮 HEAD 的 Git 规范字节"}
                    if kind == b"blob"
                    else {"pending": "非 blob 的 Git 条目"}
                )
        return baseline

    def _changes(self, before: Record | None, after: Record) -> Record:
        if not before or before.get("status") != "observed" or after.get("status") != "observed":
            return {"status": "pending: 无可比较的 Git 前序快照", "files": [], "head_changed": None}
        old_files, new_files = _object(before["files"]), _object(after["files"])
        missing = sorted(set(new_files) - set(old_files))
        baseline = self._head_files(_text(before["head"]), missing)
        changes: list[Record] = []
        for name, value in new_files.items():
            current = _object(value)
            old = _object(old_files.get(name, baseline.get(name, {"absent": True})))
            if "pending" in old or "pending" in current:
                changes.append({"path": name, "before": old, "after": current, "status": "pending"})
            elif old.get("git_blob") != current.get("git_blob") or old.get("absent") != current.get("absent"):
                comparison = "工作树快照精确比较"
                changed = True
                if "baseline" in old:
                    # 仅对新增脏文件或差异候选比较 Git 规范字节。
                    # 由 Git 应用 clean 过滤器及 CRLF 转换，
                    # 不将原始 CRLF 字节与规范化后的 HEAD blob 直接比较。
                    canonical = current.get("git_blob")
                    path = self.root / name
                    if path.is_file() and not path.is_symlink():
                        canonical = self._git("hash-object", "--path=" + name, "--", name).decode("ascii").strip()
                    current = {**current, "canonical_git_blob": canonical}
                    changed = canonical != old.get("git_blob") or old.get("absent") != current.get("absent")
                    comparison = "上一轮 HEAD 与当前 Git 规范标识比较；上一轮原始工作树字节不可用"
                if changed:
                    changes.append(
                        {"path": name, "before": old, "after": current, "status": "changed", "comparison": comparison}
                    )
        return {
            "status": "observed",
            "files": changes,
            "head_changed": before["head"] != after["head"],
            "previous_head": before["head"],
            "current_head": after["head"],
            "unobserved_inputs": "pending: 未比较被忽略的运行时输入及外部输入",
        }

    def record_overhead(self, phase: str, seconds: float) -> None:
        """记录 Bazel 墙钟之外的实际工作，不改变共享计时或缓存."""
        self.overheads[phase] = self.overheads.get(phase, 0.0) + seconds

    def bazel_flags(self, sequence: int) -> list[str]:
        if sequence in self._flags:
            message = f"诊断序号重复：{sequence}"
            raise ValueError(message)
        self.log_dir.mkdir(parents=True, exist_ok=True)
        if not self.debug:
            self._flags[sequence] = {}
            return []
        paths = {
            "profile": self.log_dir / f"bazel-{sequence:03d}.profile.json.gz",
            "explain": self.log_dir / f"bazel-{sequence:03d}.explain.log",
            "compact": self.log_dir / f"bazel-{sequence:03d}.execution.compact",
        }
        self._flags[sequence] = paths
        return [
            "--profile=" + str(paths["profile"]),
            "--generate_json_trace_profile",
            "--explain=" + str(paths["explain"]),
            "--verbose_explanations",
            "--execution_log_compact_file=" + str(paths["compact"]),
        ]

    def record_dispatch(self, sequence: int, argv: list[str], started: float, finished: float) -> None:
        paths = self._flags[sequence]
        bep_args = [arg.split("=", 1)[1] for arg in argv if arg.startswith("--build_event_json_file=")]
        if len(bep_args) != 1:
            message = "必须恰好指定一个原生 BEP 输出"
            raise ValueError(message)
        bep = Path(bep_args[0])
        if not bep.is_absolute():
            bep = self.root / bep
        actions: list[Record] = []
        tests: list[Record] = []
        summaries: list[Record] = []
        record: Record = {
            "sequence": sequence,
            "dispatch_seconds": finished - started,
            "evidence": {"bep": str(bep), **{key: str(path) for key, path in paths.items()}},
            "safe_options": _safe_options(argv),
            "server": {},
            "timing_seconds": {},
            "output_base_file_lock_wait_seconds": None,
            "output_base_file_lock_wait_status": "pending: command_wait_time 不衡量文件锁等待",
            "actions": actions,
            "tests": tests,
            "test_summaries": summaries,
            "compact_input_comparison": "pending: 已保留原始 compact 日志，尚未解码"
            if self.debug
            else "disabled: 使用 cache-debug 采集 compact 日志",
        }
        with bep.open(encoding="utf-8") as stream:
            for line in stream:
                if not line.strip():
                    continue
                _record_event(_load(line), record, actions, tests, summaries)
        if not record["server"]:
            message = "BEP 缺少 started 事件"
            raise ValueError(message)
        if self.debug:
            record["profile"] = _profile(paths["profile"])
            record["compact_bytes"] = paths["compact"].stat().st_size
            record["change_chains"] = self._chains(paths["explain"], actions)
        record["test_counts"] = {
            "cached_attempts": sum(bool(test["cached"]) for test in tests),
            "rerun_attempts": sum(not test["cached"] for test in tests),
            "rerun_runtime_seconds": sum(
                _number(test["runtime_seconds"]) for test in tests if test["runtime_seconds"] is not None
            ),
        }
        self.dispatches.append(record)
        _atomic_json(self.path, self._report())

    def _chains(self, explain: Path, actions: list[Record]) -> list[Record]:
        lines = explain.read_text(encoding="utf-8").splitlines()
        blocks: list[tuple[int, list[str]]] = []
        for number, line in enumerate(lines, 1):
            if re.search(r"Executing action ['\"]", line):
                blocks.append((number, [line]))
            elif blocks:
                blocks[-1][1].append(line)
        chains: list[Record] = []
        changed = [
            _text(entry["path"])
            for item in _array(self.changes["files"])
            if (entry := _object(item))["status"] == "changed"
        ]
        for action in actions:
            label = action["label"]
            matches = [
                (number, "\n".join(block))
                for number, block in blocks
                if isinstance(label, str)
                and re.search(
                    r"(?<![\w./:@+-])" + re.escape(label) + r"(?![\w./:@+-])",
                    block[0],
                )
            ]
            proven: list[Record] = []
            for number, text in matches:
                for name in changed:
                    pattern = r"(?<![\w./\\-])" + re.escape(name).replace("/", r"[/\\]") + r"(?![\w./\\-])"
                    if any(
                        re.search(pattern, line)
                        and re.search(
                            r"\b(?:changed|different|modified|missing|removed|deleted|added)\b",
                            line,
                            re.IGNORECASE,
                        )
                        for line in text.splitlines()
                    ):
                        proven.append({"path": name, "explain_line": number})
            chains.append(
                {
                    "target": label,
                    "action": action["type"],
                    "rerun_seconds": action["seconds"],
                    "changes": proven,
                    "reason": "explain 已明确关联变更文件与目标" if proven else "pending: 无已证实的变更文件与动作关联",
                    "evidence": str(explain),
                }
            )
        return chains

    def _report(self) -> Record:
        previous_dispatches = (
            [_object(item) for item in _array(self.previous.get("dispatches", []))] if self.previous else []
        )
        return {
            "schema": 1,
            "report_only": True,
            "evidence_path": str(self.path),
            "previous_evidence_path": self.previous.get("evidence_path") if self.previous else None,
            "previous_compact_logs": [
                evidence["compact"]
                for entry in previous_dispatches
                if "compact" in (evidence := _object(entry["evidence"]))
            ],
            "snapshot": self.snapshot,
            "changes_since_previous": self.changes,
            "dispatches": self.dispatches,
            "client_overhead_seconds": self.overheads,
        }

    def finish(self, status: str, total: float, compile_excluded: float) -> None:
        # 调用方须在停止共享计时或退出监督器之前完成此方法。
        entered = time.monotonic()
        after = self._snapshot(self.snapshot) if self.debug else self.snapshot
        report = self._report()
        report["changes_during_run"] = self._changes(self.snapshot, after)
        report["snapshot"] = after
        walls = [_object(item["timing_seconds"]).get("wall") for item in self.dispatches]
        wall = sum(_number(value) for value in walls) if walls and all(value is not None for value in walls) else None
        timing: Record = {
            "cli_shared_clock_at_finish_entry_seconds": total,
            "compile_excluded_seconds": compile_excluded,
            "bazel_wall_seconds": wall,
            "cli_outside_bazel_wall_at_finish_entry_seconds": total - wall if wall is not None else None,
            "pair_outer_wall_seconds": None,
            "pair_outer_wall_status": "pending: 须由外部调用方测量",
            "finish_report_seconds_before_publication": time.monotonic() - entered,
            "scope": "入口总耗时来自调用方计时；收尾与发布仍计入调用方最终耗时，不代表两轮外部耗时",
        }
        report.update({"status": status, "timing": timing})
        _atomic_json(self.path, report)
        _atomic_json(self.latest, report)
        counts = [entry.get("action_counts", {}) for entry in self.dispatches]
        tests = [entry["test_counts"] for entry in self.dispatches]
        analysis = [_object(entry["timing_seconds"]).get("analysis") for entry in self.dispatches]
        print(
            "GATE DIAGNOSTICS | CLI（入口）| 启动/排队；文件锁 | 加载/分析 | ",
            "动作 all/GateCommand/TestRunner | 测试缓存/重跑 | Bazel 外耗时（入口）",
            sep="",
            flush=True,
        )
        print(
            f"{status} | {total:.3f}s | {[entry.get('startup') for entry in self.dispatches]}; 文件锁=待确认 | ",
            f"分析={analysis}（加载：JSON）| {counts} | {tests} | ",
            f"{timing['cli_outside_bazel_wall_at_finish_entry_seconds']}",
            sep="",
            flush=True,
        )
        print(
            f"GATE EVIDENCE {self.path} 变更={len(_array(self.changes['files']))} ",
            "关联链=JSON；未证实原因=待确认",
            sep="",
            flush=True,
        )
