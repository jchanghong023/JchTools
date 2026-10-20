"""用 Bazel 原生依赖追踪、结果缓存及 BEP 证据承接本地门原语.

每轮提交完整目标集，由 Bazel 决定复用；不维护手写成功账本。
可识别测试绑定实际产物及运行材料；未知依赖保留全仓保守闭包。
环境、工具及共享夹具仍参与指纹，不宣称源码语句级精准影响分析。
外部服务、宿主状态和动态夹具每轮真实执行；支持的编译产物使用 Bazel 原生动作缓存。
运行目录通过独占会话传递，不作为缓存语义输入。
PID 和已原子发布的 request/runtime/completion 快照使用 Windows READ/WRITE/DELETE 共享句柄。
仅 PID 启动独占写由 Job 监督器有界采样；完成前重新证明 server 归属，未知权限始终拒绝。
仓库内已识别 libtest 清单只绑定运行工具，不耦合编译/发行包配置；未知入口保持全量。
每批重新观察输入；SDK 仓库 scratch 与动作 TEMP 隔离，避免无关的跨轮重新配置。
实际派发的原生测试必须具有本轮 BEP testResult 与 testSummary 双重通过证据。
"""

from __future__ import annotations

import hashlib
import importlib.metadata
import importlib.util
import json
import math
import msvcrt
import os
import re
import shutil
import subprocess
import sys
import time
from datetime import datetime
from pathlib import Path
from typing import TYPE_CHECKING, NotRequired, TypedDict, cast, final
from urllib.parse import urlparse
from urllib.request import url2pathname

from scripts.bazel_action import (
    cargo_cache_supported,
    restore_cargo_artifact_snapshots,
    restore_cargo_artifacts,
    test_log_payload,
)
from scripts.bazel_service import ServiceLease, bind_requests
from scripts.gate_diagnostics import GateDiagnostics
from scripts.gate_inputs import InputResolver
from scripts.gate_runtime import GateStoppedError, Result, Stage, read_published_text

if TYPE_CHECKING:
    from typing import BinaryIO

    from scripts.bazel_action import DispatchConfig, GateSpec, LiveRuntime
    from scripts.gate_runtime import GateRun


_DYNAMIC_ENV = frozenset(
    {
        "TEMP",
        "TMP",
        "TMPDIR",
        "JCHTOOLS_GATE_FAILURE_FILE",
        "JCHTOOLS_GATE_STARTED",
        "JCHTOOLS_GATE_PROGRESS",
        "JCHTOOLS_GATE_LEVEL",
        "JCHTOOLS_GATE_LIMIT",
        "JCHTOOLS_GATE_BAZEL_SERVICE_LEASE",
        "JCHTOOLS_GATE_BAZEL_CONFIG",
    }
)


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


# 这些原语观察源码文件之外的可变状态，不能仅凭源码未变复用成功。
_EXTERNAL_NAMES = (
    "source-snapshot",
    "pip-audit",
    "xberg-latest",
    "media-e2e-fixtures",
    "real-",
    "package-",
    "ocr-asset",
    "requirement-reference",
)


def _write_stable(path: Path, text: str) -> None:
    if not path.exists() or read_published_text(path) != text:
        # 写方关闭临时文件后再发布；消费者只读取已完整发布的快照。
        temporary = path.with_name(path.name + ".pending")
        _ = temporary.write_text(text, encoding="utf-8", newline="\n")
        _ = temporary.replace(path)


def _json(value: object) -> str:
    return json.dumps(value, ensure_ascii=True, sort_keys=True, separators=(",", ":")) + "\n"


def _digest(path: Path) -> str:
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def _cargo_configuration_identity(root: Path, environment: dict[str, str]) -> dict[str, str]:
    """Bind Cargo's inherited configuration even outside the Bazel workspace."""
    home = environment.get("USERPROFILE") or environment.get("HOME") or str(Path.home())
    cargo_home = Path(environment.get("CARGO_HOME", str(Path(home) / ".cargo")))
    if not cargo_home.is_absolute():
        cargo_home = root / cargo_home
    directories = [root, *root.parents]
    configurations = {directory / ".cargo" / name for directory in directories for name in ("config", "config.toml")}
    configurations.update(cargo_home / name for name in ("config", "config.toml"))
    identity = {str(path): _digest(path) for path in sorted(configurations) if path.is_file()}
    # Stage-local PATH can override the bundled linker independently of the
    # process-wide tool identity. Its actual executable is an input as well.
    linker = shutil.which("rust-lld", path=environment.get("PATH", ""))
    if linker is not None:
        identity[str(Path(linker).resolve())] = _digest(Path(linker))
    return identity


def _distribution_identity(distribution: importlib.metadata.Distribution) -> dict[str, str | None]:
    metadata = distribution.metadata
    files = distribution.files
    if files is None:
        message = f"Python 工具缺少可核对的安装文件清单：{metadata.get('Name', '')}"
        raise ValueError(message)
    contents = hashlib.sha256()
    for entry in sorted(files):
        path = Path(str(distribution.locate_file(entry)))
        if path.suffix == ".pyc":
            source = (
                Path(importlib.util.source_from_cache(str(path)))
                if path.parent.name == "__pycache__"
                else path.with_suffix(".py")
            )
            if source.is_file():
                continue  # 有源码的字节码是派生缓存；纯字节码工具仍绑定实际内容。
        contents.update(str(entry).encode("utf-8") + b"\0")
        contents.update((_digest(path) if path.is_file() else "<missing>").encode("ascii") + b"\0")
    return {
        "name": metadata.get("Name", ""),
        "version": metadata.get("Version", ""),
        "record": distribution.read_text("RECORD"),
        "direct_url": distribution.read_text("direct_url.json"),
        "files_sha256": contents.hexdigest(),
    }


def _tool_identity(sysroot: Path) -> dict[str, object]:
    executables = {sys.executable}
    for name in (
        "cargo",
        "rustc",
        "rustdoc",
        "rustfmt",
        "clippy-driver",
        "rust-lld",
        "git",
        "pwsh",
        "powershell",
        "java",
        "bazel",
        "bazelisk",
    ):
        executable = shutil.which(name)
        if executable:
            executables.add(executable)
    bundled_bazel = Path(__file__).resolve().parents[1] / ".tmp/tools/bazel.exe"
    if bundled_bazel.is_file():
        executables.add(str(bundled_bazel))
    distributions = [_distribution_identity(distribution) for distribution in importlib.metadata.distributions()]
    cargo_home = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")))
    rustup_home = Path(os.environ.get("RUSTUP_HOME", str(Path.home() / ".rustup")))
    # rustup PATH 入口只是代理；仅哈希当前真实选中的工具链，不扫描无关安装。
    for name in ("rustdoc.exe", "rustc.exe", "cargo.exe"):
        executable = sysroot / "bin" / name
        if not executable.is_file():
            message = f"选中 Rust 工具链缺少工具：{executable}"
            raise FileNotFoundError(message)
        executables.add(str(executable))
    for pattern in ("bin/rustc_driver*.dll", "lib/rustc_driver*.dll"):
        executables.update(str(path) for path in sysroot.glob(pattern) if path.is_file())
    # A configured bare rust-lld may resolve through rustc's bundled linker
    # directory rather than PATH. Bind both possibilities, not just its name.
    for pattern in ("bin/rust-lld", "bin/rust-lld.exe", "lib/rustlib/*/bin/rust-lld", "lib/rustlib/*/bin/rust-lld.exe"):
        executables.update(str(path) for path in sysroot.glob(pattern) if path.is_file())
    configuration = [cargo_home / "config", cargo_home / "config.toml", rustup_home / "settings.toml"]
    # rustup installs several hard-linked command shims. Hash each physical file
    # once in this observation, while retaining every resolved executable path.
    observed: dict[tuple[int, int] | str, str] = {}
    executable_digests: dict[str, str] = {}
    for value in sorted(executables):
        path = Path(value).resolve()
        metadata = path.stat()
        key = (metadata.st_dev, metadata.st_ino) if metadata.st_ino else str(path)
        if key not in observed:
            observed[key] = _digest(path)
        executable_digests[str(path)] = observed[key]
    return {
        "executables": executable_digests,
        "adapter": _digest(Path(__file__).with_name("bazel_action.py")),
        "configuration": {str(path): _digest(path) for path in configuration if path.is_file()},
        "rustup_state": {
            str(path): _digest(path) for path in [rustup_home / "update-hashes" / sysroot.name] if path.is_file()
        },
        "generator": _digest(Path(__file__)),
        "python": sys.version,
        "distributions": sorted(distributions, key=lambda item: str(item["name"]).lower()),
    }


def _target(name: str) -> str:
    # 完整用例 ID 留在 spec 和日志中；目标文件名有界，避免 Windows 路径过长。
    safe = re.sub(r"[^a-zA-Z0-9_-]", "_", name)[:48]
    return safe + "_" + hashlib.sha256(name.encode()).hexdigest()[:16]


def _label(name: str) -> str:
    """每个稳定 Stage 使用独立短 package，不随派发集合改变标签."""
    return "//.tmp/bazel-gate/targets/" + hashlib.sha256(name.encode()).hexdigest()[:16] + ":run"


def _resource_name(resource: str) -> str:
    """映射物理共享状态，而不是把互不相干的互斥组提升成全局独占."""
    # media E2E 与 worker/engine 测试均持有 tests/common/engine_session.rs
    # 的仓库级 .tmp/engine-session.lock；状态目录隔离不代表会话独立。
    # 7-Zip 保留现有串行边界；外部引擎路径也可能指向随包文件的同一实体。
    aliases = {
        "shared-xberg-media": "engine-session",
        "real-7zip": "bundled-7zip",
    }
    return "gate_" + _target(aliases.get(resource, resource))


def _resource_tags(stage: Stage, *, parallel: bool) -> list[str]:
    """投影本地调度资源；资源隔离不改变结果缓存资格."""
    if not parallel:
        return []
    # 原生 cpu:n 可超订；自定义容量资源严格阻止同门测试争抢短墙钟预算。
    slots = (os.cpu_count() or 1) if stage.exclusive_resource == "gate-wall-clock" else 1
    tags = [f"resources:gate_host_slots:{slots}"]
    if stage.exclusive_resource:
        tags.append("resources:" + _resource_name(stage.exclusive_resource) + ":1")
    return tags


def _input_key(stage: Stage) -> str:
    """同批次共享依赖分析，不共享测试结果；每次派发重新读取实际输入."""
    argv = stage.argv
    if argv[1:3] == ["-m", "scripts.gate_unittest"] and "--" in argv[3:]:
        argv = [argv[0], "-m", "scripts.unittest_report", *argv[argv.index("--", 3) + 1 :]]
    elif stage.kind == "check" and re.search(
        r"(?:^|[\\/])target[\\/]debug[\\/]deps[\\/][^\\/]+-[0-9a-f]+\.exe$", argv[0], re.IGNORECASE
    ):
        # 只有 libtest 二进制的清单/过滤参数不改变运行材料；不能合并 Python 入口。
        argv = argv[:1] + (["--list"] if _libtest_listing(stage) else [])
    return _json({"argv": argv, "kind": stage.kind, "env": stage.env, "phase": str(stage.phase_file)})


def _doctest(stage: Stage) -> bool:
    if stage.kind != "compiler-observed":
        return False
    arguments = ["test"]
    if stage.name == "root-doctests":
        pass
    elif stage.name in {"snap-ocr-core-doctests", "snap-ocr-worker-doctests"}:
        component = stage.name.removesuffix("-doctests")
        arguments += ["--manifest-path", f"optional/{component}/Cargo.toml"]
    else:
        return False
    return stage.argv[1:] == [*arguments, "--doc", "--features", "test-hooks", "--"]


def _doc_cache_safe(root: Path) -> bool:
    """当前源码不含可运行 rustdoc 材料才复用；新围栏/动态 doc 一律真实执行."""
    for directory, names, files in os.walk(root):
        names[:] = [name for name in names if name not in {".git", ".tmp", "target", "vendor", "node_modules"}]
        for name in files:
            if not name.endswith(".rs"):
                continue
            path = Path(directory) / name
            if path.is_symlink():
                return False
            try:
                source = path.read_text(encoding="utf-8")
            except (OSError, UnicodeError):
                return False
            if (
                re.search(r"#\s*!?\s*\[\s*doc\s*=", source)
                or re.search(r"(?m)^\s*//[/!]\s*(?:```|~~~)", source)
                # rustdoc 先去掉一个分隔空格；五个源空格才形成四格缩进代码。
                or re.search(r"(?m)^\s*//[/!](?: {5,}|\t)\S", source)
            ):
                return False
            if any(
                "```" in match.group(1) or "~~~" in match.group(1)
                for match in re.finditer(r"/\*[*!](.*?)\*/", source, re.DOTALL)
            ):
                return False
    return True


def _repository_engine_cacheable(stage: Stage) -> bool:
    """仅复用完整声明在仓库内、只读且逐用例隔离的 7-Zip 场景."""
    if not stage.name.startswith(("real-engine-tests-", "real-engine-lib-tests-")) or set(stage.env or {}) != {
        "JCHTOOLS_TEST_7ZIP"
    }:
        return False
    try:
        root = Path(__file__).resolve().parents[1]
        engine = Path((stage.env or {})["JCHTOOLS_TEST_7ZIP"]).resolve(strict=True)
        expected = (root / "resources/7zip/7z.exe").resolve(strict=True)
        library = expected.with_name("7z.dll").resolve(strict=True)
        return engine == expected and engine.is_relative_to(root) and library.is_relative_to(root)
    except (OSError, ValueError):
        return False


def _libtest_listing(stage: Stage) -> bool:
    """仅识别仓库内已知 libtest 清单语法；未知程序/参数仍保守真实执行."""
    if stage.watch_test_failures or "--list" not in stage.argv[1:] or "--format=terse" not in stage.argv[1:]:
        return False
    for argument in stage.argv[1:]:
        if not argument.startswith("-") or argument in {
            "--list",
            "--format=terse",
            "--nocapture",
            "--ignored",
            "--include-ignored",
            "--exact",
            "--skip",
        }:
            continue
        if argument.startswith("--test-threads=") and argument.partition("=")[2].isdigit():
            continue
        return False
    try:
        root = Path(__file__).resolve().parents[1]
        executable = Path(stage.argv[0])
        executable = (root / executable).resolve(strict=True)
        relative = executable.relative_to(root).as_posix()
        return executable.is_file() and bool(
            re.fullmatch(r"(?:optional/[^/]+/)?target/debug/deps/[^/]+-[0-9a-f]+\.exe", relative, re.IGNORECASE)
        )
    except (OSError, ValueError):
        return False


def _ruff(stage: Stage) -> bool:
    return stage.argv[1:3] == ["-m", "ruff"] or Path(stage.argv[0]).stem.lower() == "ruff"


def _doctest_cacheable(stage: Stage, *, doc_cache_safe: bool | None) -> bool:
    environment = os.environ | (stage.env or {})
    # 调度/终端输出不引用外部输入，且完整取值仍绑定 action spec。
    if any(
        key.upper().startswith(("CARGO_", "RUST"))
        and key.upper() not in {"CARGO_HOME", "RUSTUP_HOME", "CARGO_BUILD_JOBS", "CARGO_TERM_PROGRESS_WHEN"}
        for key in environment
    ):
        return False
    return doc_cache_safe if doc_cache_safe is not None else _doc_cache_safe(Path(__file__).resolve().parents[1])


def _reuse(stage: Stage, *, doc_cache_safe: bool | None = None) -> bool:
    if (stage.kind != "check" and not _doctest(stage)) or stage.phase_file is not None:
        return False
    # Ruff also consumes .git/info/exclude and global Git ignores, which are not
    # declared repository inputs. Keep these checks real without disabling the
    # action cache for unrelated Cargo producers or changing Ruff's arguments.
    if _ruff(stage):
        return False
    listing = _libtest_listing(stage)
    if any(part in stage.name for part in _EXTERNAL_NAMES) and not listing and not _repository_engine_cacheable(stage):
        return False
    # 宿主显式覆盖可能引用仓库外可变资产；仅有路径字符串不足以复用成功。
    if any(key.upper().startswith("JCHTOOLS_") and not key.upper().startswith("JCHTOOLS_GATE_") for key in os.environ):
        return False
    if _doctest(stage) and not _doctest_cacheable(stage, doc_cache_safe=doc_cache_safe):
        return False
    # 显式模型、引擎和媒体目录是可变的外部依赖。
    return not any(
        any(word in key.upper() for word in ("MODEL", "ENGINE", "MEDIA", "FIXTURE")) for key in stage.env or {}
    )


def _cargo_cacheable(stage: Stage) -> bool:
    return cargo_cache_supported(stage.argv, stage.kind, dict(os.environ) | (stage.env or {}))


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
    with path.open(encoding="utf-8") as source:
        for line in source:
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


def _native_result(
    stage: Stage,
    live: LiveRuntime,
    entry: _Evidence,
    event_path: Path,
    *,
    timed_out: bool,
) -> Result:
    summary: _BepSummary = entry.get("summary", {})
    event: _BepResult = entry.get("result", {})
    status = summary.get("overallStatus") or event.get("status")
    cached = bool(event.get("cachedLocally") or event.get("cachedRemotely") or summary.get("numCached"))
    log = Path(live["log"])
    payload_valid = _restore_log(log, event, summary, cached=cached)
    risk_evidence = not _high_risk(stage) or (
        event.get("status") == "PASSED" and summary.get("overallStatus") == "PASSED"
    )
    complete = bool(status) and status != "INCOMPLETE"
    disposition = "cached" if cached else "executed" if complete else "incomplete"
    outcome = "PASS" if status == "PASSED" and payload_valid and risk_evidence else "FAIL" if complete else "UNVERIFIED"
    if status == "TIMEOUT" or (timed_out and not complete):
        outcome = "TIMEOUT"
    elapsed = float(event.get("testAttemptDurationMillis", 0)) / 1000.0
    return Result(
        stage.name,
        outcome,
        (
            f"Bazel {disposition}; BEP status={status or 'missing'}; payload={payload_valid}; "
            f"testResult={event.get('status', 'missing')}; testSummary={summary.get('overallStatus', 'missing')}; "
            f"evidence={event_path}"
        ),
        log if log.is_file() else None,
        elapsed,
        0 if outcome == "PASS" else None,
    )


def _high_risk(stage: Stage) -> bool:
    # Uncached quality checks must pass in the leading native risk batch, never
    # depend on the Cargo producers whose execution they are meant to guard.
    return stage.name.startswith(("trusted-", "test-gate-", "static-")) or _ruff(stage)


def _command_stage(stage: Stage, *, parallel: bool) -> bool:
    # Native test results must never be represented by build-action completion.
    real_test = (
        stage.watch_test_failures
        or _doctest(stage)
        or stage.argv[1:3] in (["-m", "scripts.gate_unittest"], ["-m", "scripts.unittest_report"])
        or (_high_risk(stage) and stage.name != "static-check")
    )
    return not real_test and not parallel and (stage.kind != "check" or _reuse(stage))


def _uncached_command(stage: Stage, *, parallel: bool) -> bool:
    return _command_stage(stage, parallel=parallel) and not _reuse(stage) and not _cargo_cacheable(stage)


def _command_dependencies(stages: list[Stage]) -> dict[str, list[str]]:
    """按真实命令产物建立 Cargo 生产者的依赖顺序。."""
    checks = [stage for stage in stages if stage.kind == "check"]
    dependencies: dict[str, list[str]] = {stage.name: [] for stage in checks}
    previous = [_label(stage.name) for stage in checks]
    for stage in stages:
        if stage.kind == "check":
            continue
        dependencies[stage.name] = previous
        previous = [_label(stage.name)]
    return dependencies


@final
class BazelSession:
    """把既有 Stage 自动生成原生测试目标或有序命令动作."""

    def __init__(self, run: GateRun, *, bazel_config: str | None = None) -> None:
        self.gate = run
        self.root = run.root
        self.log_dir = run.log_dir
        self.directory = self.root / ".tmp/bazel-gate"
        self._prepared = False
        self._sequence = 0
        self._lock: BinaryIO | None = None
        self._closed = False
        self._identity: dict[str, object] | None = None
        self._requests = self.log_dir / "bazel-service"
        self.bazel = shutil.which("bazel") or shutil.which("bazelisk")
        self._service: ServiceLease | None = None
        self._lease_path: Path | None = None
        self.bazel_config = bazel_config
        self.diagnostics = GateDiagnostics(self.root, self.log_dir, debug=bazel_config == "cache-debug")
        self._input_resolver = InputResolver(self.root)
        self._reusable: dict[str, bool] = {}
        self._failed = True
        if not self.bazel:
            candidates = (
                self.root / ".tmp/tools/bazel.exe",
                Path(__file__).resolve().parents[1] / ".tmp/tools/bazel.exe",
            )
            self.bazel = next((str(path) for path in candidates if path.is_file()), None)

    def _command_result(
        self, stage: Stage, live: LiveRuntime, entry: _Evidence, *, timed_out: bool, defer_restore: bool = False
    ) -> Result:
        completed = entry.get("completed", {})
        actions = entry.get("actions", [])
        native_log = _declared_file(entry, "run.command.log")
        log = Path(live["log"])
        if completed.get("success") is True and native_log is not None:
            _ = shutil.copyfile(native_log, log)
        executed = bool(actions)
        succeeded = (
            completed.get("success") is True
            and native_log is not None
            and all(action.get("success") is True for action in actions)
        )
        failed = completed.get("success") is False or any(action.get("success") is False for action in actions)
        artifacts = "not-requested"
        if succeeded and _cargo_cacheable(stage) and not defer_restore:
            try:
                _restore_declared_cargo(self.root, entry)
                artifacts = "restored"
            except (OSError, ValueError, KeyError, TypeError) as error:
                succeeded, failed = False, True
                artifacts = f"invalid:{type(error).__name__}:{error}"
        disposition = "executed" if executed else "cached" if succeeded else "incomplete"
        outcome = "PASS" if succeeded else "FAIL" if failed else "TIMEOUT" if timed_out else "UNVERIFIED"
        event_path = self.log_dir / f"bazel-{self._sequence:03d}.bep.jsonl"
        return Result(
            stage.name,
            outcome,
            "; ".join(
                (
                    f"Bazel {disposition}",
                    f"BEP targetCompleted={completed.get('success', 'missing')}",
                    f"GateCommand actions={len(actions)}",
                    f"native_log={native_log is not None}",
                    f"artifacts={artifacts}",
                    "reuse=native Cargo artifacts"
                    if _cargo_cacheable(stage)
                    else "reuse=disabled (unsupported producer)"
                    if stage.kind != "check"
                    else "reuse=native action cache",
                    f"evidence={event_path}",
                )
            ),
            log if log.is_file() else None,
            sum(_action_elapsed(action) for action in actions),
            0 if outcome == "PASS" else None,
        )

    def close(self, *, success: bool = True) -> None:
        """Release only quiescent successful work; errors terminate the native lease."""
        closed = False
        try:
            self.gate.close_persistent()
            closed = True
        finally:
            try:
                if self._service is not None:
                    self._service.close(
                        success=success and closed and not self._failed,
                        timeout=min(2.0, self.gate.remaining) if not success or self._failed else 0.0,
                    )
                    self._service = None
            finally:
                if self._lock is not None:
                    self._lock.close()
                    self._lock = None
                self._closed = True

    def _acquire(self) -> bool:
        stream = (self.directory / "session.lock").open("a+b")
        try:
            if stream.seek(0, os.SEEK_END) == 0:
                _ = stream.write(b"\0")
                stream.flush()
            _ = stream.seek(0)
            msvcrt.locking(stream.fileno(), msvcrt.LK_NBLCK, 1)
        except OSError:
            stream.close()
            return False
        self._lock = stream
        return True

    def _prepare(self) -> bool:
        if self._prepared:
            return True
        source_root = Path(__file__).resolve().parents[1]
        _ = self.gate.supervise(
            Stage(
                "bazel-workspace",
                [
                    sys.executable,
                    str(source_root / "scripts/make_tmp.py"),
                    "workspace",
                    "--destination",
                    str(self.directory),
                ],
            )
        )
        if not self._acquire():
            return False
        self._prepare_workspace(source_root)
        compiler = shutil.which("rustc")
        if compiler is None:
            message = "缺少 rustc，无法锁定实际 Rust 工具链身份"
            raise FileNotFoundError(message)
        identity_results = self.gate.supervise(Stage("rust-toolchain-identity", [compiler, "--print", "sysroot"]))
        if len(identity_results) != 1 or identity_results[0].status != "PASS" or identity_results[0].log is None:
            raise GateStoppedError(identity_results)
        sysroot = Path(identity_results[0].log.read_text(encoding="utf-8").strip())
        if not sysroot.is_absolute() or not sysroot.is_dir():
            message = f"rustc --print sysroot 未返回有效绝对工具链目录：{sysroot}"
            raise ValueError(message)
        identity_started = time.perf_counter()
        self._identity = _tool_identity(sysroot)
        _write_stable(self.directory / "tools.json", _json(self._identity))
        # Generator control flow is not a Cargo source dependency. Its semantic
        # projection (argv/environment/spec/rules) is already the action key;
        # the executing adapter and real compiler/toolchain remain bound.
        _write_stable(
            self.directory / "tools-compile.json",
            _json({key: value for key, value in self._identity.items() if key not in {"generator", "distributions"}}),
        )
        self.diagnostics.record_overhead("tool_identity", time.perf_counter() - identity_started)
        python_path = str(Path(sys.executable).resolve())
        _write_stable(
            self.directory / "tools-libtest-list.json",
            _json(
                {
                    "python": self._identity["python"],
                    "executables": {python_path: cast("dict[str, str]", self._identity["executables"])[python_path]},
                }
            ),
        )
        inherited = os.environ.get("JCHTOOLS_GATE_BAZEL_SERVICE_LEASE")
        if inherited and Path(inherited).parent.resolve() == (self.directory / "service").resolve():
            self._lease_path = Path(inherited)
        else:
            self._service = ServiceLease(self.root)
            self._lease_path = self._service.lease
        bind_requests(self._lease_path, self._requests)
        self._prepared = True
        return True

    def _prepare_workspace(self, source_root: Path) -> None:
        """锁定工作区后稳定部署原生规则、adapter 及其真实 resolver 依赖."""
        # SDK repository discovery is unrelated to the per-run action scratch space.
        (self.directory / "repository-tmp").mkdir(exist_ok=True)
        if not (self.root / "MODULE.bazel").exists():
            _write_stable(self.root / "MODULE.bazel", read_published_text(source_root / "MODULE.bazel"))
            # 已知注册表摘要允许复用下载缓存；Bazel 仍自行验证/更新新模块的依赖图。
            source_lock = source_root / "MODULE.bazel.lock"
            if source_lock.is_file() and not (self.root / "MODULE.bazel.lock").exists():
                _write_stable(self.root / "MODULE.bazel.lock", read_published_text(source_lock))
        if not (self.root / "BUILD.bazel").exists():
            _write_stable(
                self.root / "BUILD.bazel",
                (
                    'filegroup(name="gate_sources", srcs=glob(["**"], exclude=["MODULE.bazel.lock", ".tmp/**", '
                    '"bazel-*/**", "**/__pycache__/**"]), visibility=["//visibility:public"])\n'
                ),
            )
        _write_stable(
            self.directory / "gate_rules.bzl", (source_root / "scripts/gate_rules.bzl").read_text(encoding="utf-8")
        )
        _write_stable(
            self.directory / "bazel_action.py", (source_root / "scripts/bazel_action.py").read_text(encoding="utf-8")
        )
        (self.directory / "scripts").mkdir(exist_ok=True)
        _write_stable(
            self.directory / "scripts/gate_inputs.py",
            (source_root / "scripts/gate_inputs.py").read_text(encoding="utf-8"),
        )
        exported_tools = [
            "tools.json",
            "tools-compile.json",
            "tools-libtest-list.json",
            "bazel_action.py",
            "gate_rules.bzl",
            "scripts/gate_inputs.py",
        ]
        # 原生批处理规则直接调用当前解释器；无需 Python 打包或工具链。
        adapter_command = " ".join(
            '"' + str(path).replace("%", "%%") + '"' for path in (sys.executable, self.directory / "bazel_action.py")
        )
        adapter_build = [
            'load("@bazel_skylib//rules:native_binary.bzl", "native_binary")',
            'load("@bazel_skylib//rules:write_file.bzl", "write_file")',
            'package(default_visibility = ["//visibility:public"])',
            f"exports_files({json.dumps(exported_tools)})",
            "write_file(",
            '    name = "gate_adapter_script",',
            '    out = "gate_adapter.command.bat",',
            "    content = "
            + json.dumps(
                ["@echo off", "setlocal DisableDelayedExpansion", adapter_command + " %*", "exit /b %errorlevel%"]
            )
            + ",",
            "    is_executable = True,",
            '    newline = "windows",',
            ")",
            "native_binary(",
            '    name = "gate_adapter",',
            '    src = ":gate_adapter_script",',
            '    out = "gate_adapter.bat",',
            '    data = ["bazel_action.py", "scripts/gate_inputs.py"],',
            ")",
            "",
        ]
        _write_stable(self.directory / "BUILD.bazel", "\n".join(adapter_build))

    def _inputs(self, stage: Stage) -> list[str]:
        selected = None if _doctest(stage) else self._input_resolver.stage_inputs(stage)
        # Native --list cannot execute test bodies or Python distributions. Its real
        # binary/runtime inputs remain declared, and the adapter itself is separate.
        if _libtest_listing(stage):
            tools = "//.tmp/bazel-gate:tools-libtest-list.json"
        elif _cargo_cacheable(stage):
            tools = "//.tmp/bazel-gate:tools-compile.json"
        else:
            tools = "//.tmp/bazel-gate:tools.json"
        # Every adapter imports the real resolver, including non-Cargo fixture actions.
        helpers = ["//.tmp/bazel-gate:scripts/gate_inputs.py"]
        if selected is not None:
            return [*selected, tools, "//.tmp/bazel-gate:bazel_action.py", *helpers]
        inputs = ["//:gate_sources", tools, "//.tmp/bazel-gate:bazel_action.py", *helpers]
        if stage.kind in {"compile", "compiler-observed", "mixed"} and (self.root / "vendor").is_dir():
            inputs.append("//:gate_vendor_sources")
        if _doctest(stage) and (self.root / "target").is_dir():
            inputs.append("//:gate_runtime_artifacts")
        executable = Path(stage.argv[0])
        if not executable.is_absolute():
            found = shutil.which(stage.argv[0])
            executable = Path(found) if found else self.root / executable
        if executable.is_file() and executable.resolve().is_relative_to(self.root.resolve()):
            relative = executable.resolve().relative_to(self.root.resolve()).as_posix()
            inputs.append("//:" + relative)
            if (self.root / "target").is_dir():
                inputs.append("//:gate_runtime_artifacts")
        return inputs

    def _batch_inputs(self, stage: Stage, groups: dict[str, list[str]]) -> list[str]:
        key = _input_key(stage)
        if key not in groups:
            groups[key] = self._inputs(stage)
        return groups[key]

    def _generate_environment(
        self, stage: Stage, environment: dict[str, str], inherited_dynamic: dict[str, str]
    ) -> tuple[dict[str, str], dict[str, str]]:
        semantic = environment | {
            key: value for key, value in (stage.env or {}).items() if key.upper() not in _DYNAMIC_ENV
        }
        dynamic = inherited_dynamic | {
            key: value for key, value in (stage.env or {}).items() if key.upper() in _DYNAMIC_ENV
        }
        if _libtest_listing(stage):
            # Preserve the actual inherited environment, but compiler/package-manager
            # controls are not semantic inputs of a prebuilt libtest --list invocation.
            for key in list(semantic):
                if key.upper().startswith(("CARGO_", "RUSTUP_", "PIP_", "UV_", "RUFF_")) or key.upper() in {
                    "RUSTFLAGS",
                    "RUSTDOCFLAGS",
                }:
                    dynamic[key] = semantic.pop(key)
        # 混合打包的计时标记是输出路径，不是源码输入。
        if stage.phase_file is not None:
            marker = str(stage.phase_file)
            for key, value in list(semantic.items()):
                if value == marker:
                    dynamic[key] = semantic.pop(key)
        return semantic, dynamic

    def _generate(self, stages: list[Stage], *, parallel: bool) -> dict[str, LiveRuntime]:
        runtime: dict[str, LiveRuntime] = {}
        # One observation per dispatch, never a cross-round success/input cache.
        self._input_resolver.refresh()
        doc_cache_safe = _doc_cache_safe(self.root) if any(_doctest(stage) for stage in stages) else None
        self._reusable = {stage.name: _reuse(stage, doc_cache_safe=doc_cache_safe) for stage in stages}
        input_groups: dict[str, list[str]] = {}
        environment = {
            key: value
            for key, value in os.environ.items()
            if key.upper() not in _DYNAMIC_ENV and not key.startswith(("BAZEL_", "TEST_", "RUNFILES_"))
        }
        inherited_dynamic = {key: value for key, value in os.environ.items() if key.upper() in _DYNAMIC_ENV}
        commands = [stage for stage in stages if _command_stage(stage, parallel=parallel)]
        # Native test configurations must never traverse side-effectful command
        # producers. Dispatch order, not test data, provides the phase barrier.
        dependencies: dict[str, list[str]] = {stage.name: [] for stage in stages}
        if not parallel:
            dependencies.update(_command_dependencies(commands))
        for stage in stages:
            command = _command_stage(stage, parallel=parallel)
            cacheable = self._reusable[stage.name] or (not parallel and _cargo_cacheable(stage))
            target = _target(stage.name)
            package = self.root / _label(stage.name).removeprefix("//").split(":")[0]
            package.mkdir(parents=True, exist_ok=True)
            semantic, dynamic = self._generate_environment(stage, environment, inherited_dynamic)
            spec: GateSpec = {
                "name": stage.name,
                "kind": stage.kind,
                "root": str(self.root),
                "argv": stage.argv,
                "environment": semantic,
                "watch_test_failures": stage.watch_test_failures,
                "cargo_artifacts": not parallel and _cargo_cacheable(stage),
            }
            _write_stable(package / "spec.json", _json(spec))
            inputs = self._batch_inputs(stage, input_groups)
            if _cargo_cacheable(stage):
                _write_stable(
                    package / "cargo-configuration.json", _json(_cargo_configuration_identity(self.root, semantic))
                )
                inputs = [*inputs, ":cargo-configuration.json"]
            tags = ["local"]
            if not cacheable:
                tags.append("no-cache")
                if not command:
                    tags.append("external")
            tags.extend(_resource_tags(stage, parallel=not command))
            if _high_risk(stage):
                tags.append("gate-risk-high")
            rules = [
                'load("//.tmp/bazel-gate:gate_rules.bzl", "gate_test", "gate_command")',
                "",
                "gate_command(" if command else "gate_test(",
                '    name = "run",',
                '    visibility = ["//.tmp/bazel-gate:__subpackages__"],',
                '    spec = ":spec.json",',
                "    spec_path = " + json.dumps(str(package / "spec.json")) + ",",
                "    data = " + json.dumps(inputs) + ",",
                "    python = " + json.dumps(sys.executable) + ",",
                "    adapter = " + json.dumps(str(self.directory / "bazel_action.py")) + ",",
                "    windows = " + str(os.name == "nt") + ",",
            ]
            rules.append("    before = " + json.dumps(dependencies[stage.name]) + ",")
            if command:
                rules.append("    cargo_artifacts = " + str(not parallel and _cargo_cacheable(stage)) + ",")
                rules.append("    cacheable = " + str(cacheable) + ",")
            else:
                rules += ['    size = "enormous",', '    timeout = "eternal",']
            rules += ["    tags = " + json.dumps(tags) + ",", ")", ""]
            _write_stable(package / "BUILD.bazel", "\n".join(rules))
            log = self.log_dir / f"native-{self._sequence:03d}-{target}.log"
            runtime[stage.name] = {
                "environment": dynamic,
                "log": str(log),
                "result_file": str(log.with_suffix(".test-results")),
                "TEMP": os.environ.get("TEMP"),
                "TMP": os.environ.get("TMP"),
                "phase_file": str(self.log_dir / f"bazel-{self._sequence:03d}.phase.json")
                if command and not parallel and stage.kind != "mixed"
                else None,
            }
        return runtime

    def _dispatch_argv(self, stages: list[Stage], event_path: Path, *, parallel: bool) -> list[str]:
        if self.bazel is None:
            message = "Bazel 执行器尚未解析。"
            raise RuntimeError(message)
        high_risk = any(_high_risk(stage) for stage in stages)
        risk = "优先执行易错检查" if high_risk else "其余独立必需检查"
        if not parallel:
            risk = "按依赖顺序访问 Cargo 共享产物"
        native_tests = any(not _command_stage(stage, parallel=parallel) for stage in stages)
        labels = [_label(stage.name) for stage in stages]
        # 逐用例目标可能数以千计，不能把全部标签塞进 Windows 命令行。
        target_file = self.log_dir / f"bazel-{self._sequence:03d}.targets"
        _write_stable(target_file, "\n".join(labels) + "\n")
        print(f"BAZEL DISPATCH {self._sequence} reason={risk} parallel={parallel} BEP={event_path}", flush=True)
        print(f"BAZEL REQUIRED count={len(labels)} targets={target_file}", flush=True)
        return [
            self.bazel,
            "--nobatch",
            "--max_idle_secs=10800",
            # 安装与下载缓存按源码 checkout 共享；下方 output_base 仍隔离全部构建/测试状态。
            "--output_user_root=" + str(Path(__file__).resolve().parents[1] / ".tmp/bazel-gate/output"),
            "--output_base=" + str(self.directory / "output" / "base"),
            # JVM 的 JNI/DLL 会跨 CLI 持有，不能解压到本轮动作的隔离 TEMP。
            "--host_jvm_args=-Djava.io.tmpdir=" + str(self.directory / "service" / "jvm-tmp"),
            "--noblock_for_lock",
            "test" if native_tests else "build",
            *(["--config=" + self.bazel_config] if self.bazel_config else []),
            *self.diagnostics.bazel_flags(self._sequence),
            "--repo_contents_cache=",
            "--enable_bzlmod",
            "--lockfile_mode=update",
            "--repo_env=TEMP=" + str(self.directory / "repository-tmp"),
            "--repo_env=TMP=" + str(self.directory / "repository-tmp"),
            "--incompatible_autoload_externally=",
            "--nocheck_visibility",
            "--nokeep_going",
            "--jobs=auto",
            "--build_event_json_file=" + str(event_path),
            "--build_event_publish_all_actions",
            "--build_event_json_file_path_conversion",
            "--color=no",
            "--curses=no",
            "--experimental_convenience_symlinks=ignore",
            *(
                [
                    "--cache_test_results=auto",
                    "--test_strategy=standalone",
                    "--test_output=errors",
                    "--notest_keep_going",
                    f"--test_timeout={max(1, math.ceil(self.gate.limit))}",
                    # 由 Bazel 根据本机 CPU / 内存及目标资源声明限流，不固定四槽。
                    "--local_test_jobs=0",
                    f"--local_resources=gate_host_slots={os.cpu_count() or 1}",
                    *[
                        "--local_resources=" + resource + "=1"
                        for resource in sorted(
                            {_resource_name(stage.exclusive_resource) for stage in stages if stage.exclusive_resource}
                        )
                    ],
                ]
                if native_tests
                else []
            ),
            *(["--nouse_action_cache"] if any(_uncached_command(stage, parallel=parallel) for stage in stages) else []),
            "--target_pattern_file=" + str(target_file),
        ]

    def _restore_batch(self, stages: list[Stage], results: list[Result], evidence: dict[str, _Evidence]) -> None:
        producers = [(stage, result) for stage, result in zip(stages, results, strict=True) if _cargo_cacheable(stage)]
        try:
            snapshots: list[tuple[Path, Path]] = []
            for stage, result in producers:
                if result.status != "PASS":
                    continue
                entry = evidence.get(_label(stage.name), {})
                manifest = _cargo_manifest(stage, entry)
                snapshots.append((manifest.with_suffix(""), manifest))
            restore_cargo_artifact_snapshots(self.root, snapshots)
            for _, result in producers:
                result.detail = result.detail.replace("artifacts=not-requested", "artifacts=restored")
        except (OSError, ValueError, KeyError, TypeError) as error:
            for _, result in producers:
                result.status = "FAIL"
                result.detail += f"; artifacts=invalid:{type(error).__name__}:{error}"

    def _dispatch_request(
        self, stages: list[Stage], runtime_path: Path, argv: list[str], *, parallel: bool
    ) -> tuple[Stage, Path]:
        self._requests.mkdir(parents=True, exist_ok=True)
        config = self._requests / f"{self._sequence:03d}.request.json"
        completion = config.with_suffix(".complete.json")
        pending = config.with_suffix(".pending")
        dispatch_environment = os.environ | {
            "PYTHONPATH": str(Path(__file__).resolve().parents[1]) + os.pathsep + os.environ.get("PYTHONPATH", "")
        }
        _write_stable(pending, _json({"argv": argv, "runtime": str(runtime_path), "environment": dispatch_environment}))
        _ = pending.replace(config)
        compile_only = all(stage.kind in {"compile", "compiler-observed"} for stage in stages)
        check_and_compile = (
            not parallel
            and any(stage.kind == "check" for stage in stages)
            and any(stage.kind in {"compile", "compiler-observed"} for stage in stages)
        )
        supervisor = Stage(
            "bazel-dispatch-" + str(self._sequence),
            [sys.executable, "-m", "scripts.bazel_gate", "--lease-client", str(self._requests)],
            kind="mixed"
            if check_and_compile or (len(stages) == 1 and stages[0].kind == "mixed")
            else "compiler-observed"
            if compile_only
            else "check",
            phase_file=self.log_dir / f"bazel-{self._sequence:03d}.phase.json"
            if check_and_compile
            else stages[0].phase_file
            if len(stages) == 1
            else None,
            env={"PYTHONPATH": dispatch_environment["PYTHONPATH"]},
        )
        return supervisor, completion

    def _runtime_snapshot(self, stages: list[Stage], *, parallel: bool) -> tuple[dict[str, LiveRuntime], Path]:
        generation_started = time.perf_counter()
        runtime = self._generate(stages, parallel=parallel)
        self.log_dir.mkdir(parents=True, exist_ok=True)
        session = hashlib.sha256(str(self.log_dir).encode()).hexdigest()[:16]
        path = self.directory / f"runtime-{session}-{self._sequence:03d}.json"
        _write_stable(path, _json(runtime))
        self.diagnostics.record_overhead("target_generation", time.perf_counter() - generation_started)
        return runtime, path

    def _dispatch(self, stages: list[Stage], *, parallel: bool) -> list[Result]:
        self._sequence += 1
        runtime, runtime_path = self._runtime_snapshot(stages, parallel=parallel)
        event_path = self.log_dir / f"bazel-{self._sequence:03d}.bep.jsonl"
        argv = self._dispatch_argv(stages, event_path, parallel=parallel)
        supervisor, completion = self._dispatch_request(stages, runtime_path, argv, parallel=parallel)
        stopped: GateStoppedError | None = None
        started = time.perf_counter()
        try:
            outer = self.gate.supervise_persistent(supervisor, completion)
        except GateStoppedError as error:
            outer = error.results
            stopped = error
        finally:
            active_error = sys.exception()
            diagnostics_started = time.perf_counter()
            try:
                self.diagnostics.record_dispatch(self._sequence, argv, started, time.perf_counter())
            except Exception as error:
                original = active_error or stopped
                if original is None:
                    raise
                original.add_note(f"Dispatch diagnostics also failed: {type(error).__name__}: {error}")
            finally:
                self.diagnostics.record_overhead("dispatch_diagnostics", time.perf_counter() - diagnostics_started)
        evidence_started = time.perf_counter()
        evidence = _events(event_path)
        timed_out = bool(stopped) and any(item.status == "TIMEOUT" for item in outer)
        results: list[Result] = []
        for stage in stages:
            entry = evidence.get(_label(stage.name), {})
            command = _command_stage(stage, parallel=parallel)
            result = (
                _native_result(
                    stage,
                    runtime[stage.name],
                    entry,
                    event_path,
                    timed_out=timed_out,
                )
                if not command
                else self._command_result(stage, runtime[stage.name], entry, timed_out=timed_out, defer_restore=True)
            )
            if not command:
                reuse_detail = "auto" if self._reusable[stage.name] else "disabled (external/dynamic/compile)"
                result.detail += f"; reuse={reuse_detail}"
            # Payload decoding is also used independently. The dispatch boundary,
            # however, accepts no native test without both current BEP events.
            if (
                not command
                and result.status == "PASS"
                and (
                    entry.get("result", {}).get("status") != "PASSED"
                    or entry.get("summary", {}).get("overallStatus") != "PASSED"
                )
            ):
                result = Result(
                    stage.name,
                    "UNVERIFIED",
                    result.detail + "; native test requires PASSED testResult and testSummary",
                    result.log,
                    result.elapsed,
                )
            results.append(self.gate.validate_result(result))
        self.diagnostics.record_overhead("bep_and_log_restore", time.perf_counter() - evidence_started)
        if not parallel:
            restore_started = time.perf_counter()
            self._restore_batch(stages, results, evidence)
            self.diagnostics.record_overhead("cargo_snapshot_restore", time.perf_counter() - restore_started)
        self.gate.record_results(results)
        if stopped or any(result.status != "PASS" for result in results):
            raise GateStoppedError(results)
        return results

    @staticmethod
    def _run_groups(stages: list[Stage], *, parallel: bool) -> list[tuple[list[Stage], bool]]:
        """先验证风险检查，再独立派发命令图和测试；测试不依赖生产者屏障."""
        # Validate all high-risk native test evidence before ordinary tests are submitted.
        if not parallel:
            high_tests = [stage for stage in stages if _high_risk(stage) and not _command_stage(stage, parallel=False)]
            if high_tests:
                remainder = [stage for stage in stages if stage not in high_tests]
                return [(high_tests, True), *BazelSession._run_groups(remainder, parallel=False)]
        ordinary = [stage for stage in stages if stage.kind != "mixed"]
        mixed = [stage for stage in stages if stage.kind == "mixed"]
        compilers = [stage for stage in ordinary if stage.kind != "check" and not (parallel and _doctest(stage))]
        checks = [stage for stage in ordinary if stage not in compilers]
        groups = [(checks, True), (compilers, False)] if parallel else [(ordinary, False)]
        groups.extend(([stage], False) for stage in mixed)
        bounded: list[tuple[list[Stage], bool]] = []
        for group, concurrent in groups:
            if concurrent:
                uncached = [stage for stage in group if _uncached_command(stage, parallel=True)]
                candidates = [stage for stage in group if stage not in uncached]
                high = [stage for stage in candidates if _high_risk(stage)]
                ordinary_checks = [stage for stage in candidates if not _high_risk(stage)]
                bounded.extend([(uncached, False), (high, True), (ordinary_checks, True)])
                continue
            # native_test 的 data 与命令图分开派发；不把调度屏障当作产物依赖，
            # 避免 Cargo/日志生产者按两个配置重复执行。
            commands = [stage for stage in group if _command_stage(stage, parallel=False)]
            native_tests = [stage for stage in group if not _command_stage(stage, parallel=False)]
            command_groups: list[tuple[list[Stage], bool]] = []
            for stage in commands:
                # Unsupported command caching must not disable the Cargo cache.
                if command_groups and (
                    _uncached_command(command_groups[-1][0][-1], parallel=False)
                    == _uncached_command(stage, parallel=False)
                ):
                    command_groups[-1][0].append(stage)
                else:
                    command_groups.append(([stage], False))
            bounded.extend(command_groups)
            bounded.append((native_tests, True))
        return bounded

    def run(self, stages: list[Stage], *, parallel: bool = True) -> list[Result]:
        if self._closed:
            message = "Bazel 会话已经关闭。"
            raise RuntimeError(message)
        if not stages:
            return []
        if self.bazel is None:
            results = [
                Result(stage.name, "UNVERIFIED", "缺少 Bazel/Bazelisk；须置于 PATH 或 .tmp/tools/bazel.exe。")
                for stage in stages
            ]
            self.gate.record_results(results)
            raise GateStoppedError(results)
        if not self._prepare():
            results = [
                Result(stage.name, "UNVERIFIED", "同一工作区已有 Bazel 门会话；未覆盖其生成图或运行元数据。")
                for stage in stages
            ]
            self.gate.record_results(results)
            raise GateStoppedError(results)
        bounded = self._run_groups(stages, parallel=parallel)
        results: list[Result] = []
        for group, concurrent in bounded:
            if not group:
                continue
            try:
                self._failed = True
                results.extend(self._dispatch(group, parallel=concurrent))
                self._failed = False
            except GateStoppedError as error:
                results.extend(error.results)
                remaining = [stage for stage in stages if stage.name not in {result.name for result in results}]
                unstarted = [
                    Result(stage.name, "UNVERIFIED", "前置 Bazel 风险组已失败，本目标未派发。") for stage in remaining
                ]
                self.gate.record_results(unstarted)
                raise GateStoppedError(results + unstarted) from error
        by_name = {result.name: result for result in results}
        return [by_name[stage.name] for stage in stages]


def _dispatch_main(config_path: str) -> int:
    config = cast("DispatchConfig", json.loads(read_published_text(Path(config_path))))
    runtime_path = Path(config["runtime"])
    runtime = cast("dict[str, LiveRuntime]", json.loads(read_published_text(runtime_path)))
    root = runtime_path.parents[2]
    for name, live in runtime.items():
        live["failure_file"] = os.environ.get("JCHTOOLS_GATE_FAILURE_FILE")
        live["TEMP"] = os.environ.get("TEMP")
        live["TMP"] = os.environ.get("TMP")
        # 每个用例只读取自己的动态元数据，避免 N 个进程各自解析 N 项清单。
        package = root / _label(name).removeprefix("//").split(":")[0]
        _write_stable(package / "spec.runtime.json", _json(live))
    return subprocess.call(config["argv"], cwd=root)


def _lease_client(directory: str) -> int:
    """仅观察本轮请求；原生服务 Job 由外层 CLI 持有。."""
    requests = Path(directory)
    _write_stable(
        requests / "client.json",
        _json({key: os.environ[key] for key in ("JCHTOOLS_GATE_FAILURE_FILE", "TEMP", "TMP") if key in os.environ}),
    )
    while True:
        for completion in requests.glob("*.complete.json"):
            code = cast("object", json.loads(read_published_text(completion)))
            if type(code) is int and code:
                # 留出窗口让 GateRun 读取完成结果，不抢先删除发布文件。
                time.sleep(1)
                return code
        time.sleep(0.02)


if __name__ == "__main__":
    dispatch_argc = 3
    if len(sys.argv) != dispatch_argc or sys.argv[1] not in {"--dispatch", "--lease-client"}:
        message = "请由测试门通过 BazelSession 调用。"
        raise SystemExit(message)
    raise SystemExit(_lease_client(sys.argv[2]) if sys.argv[1] == "--lease-client" else _dispatch_main(sys.argv[2]))
