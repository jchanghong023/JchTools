"""工具/目标身份、标签、缓存资格与原生结果分类原语.

从 bazel_gate 根模块拆出的无状态层：工具链/发行版身份、标签、资源、输入键、
复用资格、命令依赖顺序与原生结果分类。只依赖标准库、bazel_action、
gate_runtime 与 bazel_gate_bep，不回读根模块；`__file__` 与 `_doc_cache_safe`
等可补丁接缝的最终判定保留在根模块。
"""

from __future__ import annotations

import hashlib
import importlib.metadata
import importlib.util
import json
import os
import re
import shutil
import sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import TYPE_CHECKING

from scripts.bazel_action import cargo_cache_supported
from scripts.bazel_gate_bep import _BepResult, _BepSummary, _restore_log
from scripts.gate_runtime import Result, read_published_text

if TYPE_CHECKING:
    from scripts.bazel_action import LiveRuntime
    from scripts.bazel_gate_bep import _Evidence
    from scripts.gate_runtime import Stage


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
    cargo_home = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")))
    rustup_home = Path(os.environ.get("RUSTUP_HOME", str(Path.home() / ".rustup")))
    # rustup PATH 入口只是代理；仅哈希当前真实选中的工具链，不扫描无关安装。
    for name in ("rustdoc.exe", "rustc.exe", "cargo.exe"):
        executable = sysroot / "bin" / name
        if not executable.is_file():
            message = f"选中 Rust 工具链缺少工具：{executable}"
            raise FileNotFoundError(message)
        executables.add(str(executable))
    # 可选组件也经 rustup 代理启动；绑定当前工具链实现，而非仅绑定 PATH 代理。
    for pattern in (
        "bin/rustfmt.exe",
        "bin/cargo-clippy.exe",
        "bin/clippy-driver.exe",
        "bin/rustc_driver*.dll",
        "lib/rustc_driver*.dll",
    ):
        executables.update(str(path) for path in sysroot.glob(pattern) if path.is_file())
    # A configured bare rust-lld may resolve through rustc's bundled linker
    # directory rather than PATH. Bind both possibilities, not just its name.
    for pattern in ("bin/rust-lld", "bin/rust-lld.exe", "lib/rustlib/*/bin/rust-lld", "lib/rustlib/*/bin/rust-lld.exe"):
        executables.update(str(path) for path in sysroot.glob(pattern) if path.is_file())
    configuration = [cargo_home / "config", cargo_home / "config.toml", rustup_home / "settings.toml"]
    # 各发行版/可执行文件互不相同；并行只重叠读取与哈希，不改变任何身份内容。
    with ThreadPoolExecutor(max_workers=min(16, (os.cpu_count() or 4))) as pool:
        distributions = list(pool.map(_distribution_identity, importlib.metadata.distributions()))
        # rustup installs several hard-linked command shims. Hash each physical file
        # once in this observation, while retaining every resolved executable path.
        key_by_path: dict[Path, tuple[int, int] | str] = {}
        path_by_key: dict[tuple[int, int] | str, Path] = {}
        for value in sorted(executables):
            path = Path(value).resolve()
            metadata = path.stat()
            key = (metadata.st_dev, metadata.st_ino) if metadata.st_ino else str(path)
            key_by_path[path] = key
            _ = path_by_key.setdefault(key, path)
        physical = list(path_by_key.values())
        digest_by_path = dict(zip(physical, pool.map(_digest, physical), strict=True))
    executable_digests = {str(path): digest_by_path[path_by_key[key]] for path, key in key_by_path.items()}
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


def _cargo_cacheable(stage: Stage) -> bool:
    return cargo_cache_supported(stage.argv, stage.kind, dict(os.environ) | (stage.env or {}))


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
    # 独立 Ruff / 引擎前置先保护 Cargo 生产者；已知清单协议不执行测试体，
    # 因此发现阶段不按后续测试的风险分类额外拆批。
    if _ruff(stage) or (stage.name == "xberg-latest" and stage.argv[1:] == ["scripts/xberg_test_engine.py"]):
        return True
    if not stage.name.startswith(("trusted-", "test-gate-", "static-")):
        return False
    python_listing = (
        stage.kind == "check"
        and not stage.watch_test_failures
        and stage.argv[1:5] == ["-m", "scripts.gate_unittest", "--list", "--"]
    )
    return not (python_listing or _libtest_listing(stage))


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
