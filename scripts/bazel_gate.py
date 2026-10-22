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

# 回归接缝：test_test_gate.py 经 `bazel_gate.importlib.metadata` 读取并 patch
# 标准库发行版 API；工具身份本体已拆至 bazel_gate_identity。
import importlib.metadata  # noqa: F401  # pyright: ignore[reportUnusedImport]
import json
import msvcrt
import os
import shutil
import sys
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import TYPE_CHECKING, cast, final

from scripts.bazel_action import restore_cargo_artifact_snapshots
from scripts.bazel_gate_bep import (
    Evidence,
    action_elapsed,
    cargo_manifest,
    declared_file,
    events,
    restore_declared_cargo,
)
from scripts.bazel_gate_identity import (
    bazel_label,
    canonical_json,
    cargo_cacheable,
    cargo_configuration_identity,
    command_dependencies,
    doc_cache_probe,
    doctest,
    input_key,
    libtest_listing,
    native_result,
    resource_name,
    resource_tags,
    ruff,
    stage_is_high_risk,
    target_label,
    tool_identity,
    write_stable,
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


def _doctest_cacheable(stage: Stage, *, doc_cache_safe: bool | None) -> bool:
    environment = os.environ | (stage.env or {})
    # 调度/终端输出不引用外部输入，且完整取值仍绑定 action spec。
    if any(
        key.upper().startswith(("CARGO_", "RUST"))
        and key.upper() not in {"CARGO_HOME", "RUSTUP_HOME", "CARGO_BUILD_JOBS", "CARGO_TERM_PROGRESS_WHEN"}
        for key in environment
    ):
        return False
    return doc_cache_safe if doc_cache_safe is not None else doc_cache_probe(Path(__file__).resolve().parents[1])


def _reuse(stage: Stage, *, doc_cache_safe: bool | None = None) -> bool:
    if (stage.kind != "check" and not doctest(stage)) or stage.phase_file is not None:
        return False
    # Ruff also consumes .git/info/exclude and global Git ignores, which are not
    # declared repository inputs. Keep these checks real without disabling the
    # action cache for unrelated Cargo producers or changing Ruff's arguments.
    if ruff(stage):
        return False
    listing = libtest_listing(stage)
    if any(part in stage.name for part in _EXTERNAL_NAMES) and not listing and not _repository_engine_cacheable(stage):
        return False
    # 宿主显式覆盖可能引用仓库外可变资产；仅有路径字符串不足以复用成功。
    if any(key.upper().startswith("JCHTOOLS_") and not key.upper().startswith("JCHTOOLS_GATE_") for key in os.environ):
        return False
    if doctest(stage) and not _doctest_cacheable(stage, doc_cache_safe=doc_cache_safe):
        return False
    # 显式模型、引擎和媒体目录是可变的外部依赖。
    return not any(
        any(word in key.upper() for word in ("MODEL", "ENGINE", "MEDIA", "FIXTURE")) for key in stage.env or {}
    )


def _command_stage(stage: Stage, *, parallel: bool) -> bool:
    # Native test results must never be represented by build-action completion.
    real_test = (
        stage.watch_test_failures
        or doctest(stage)
        or stage.argv[1:3] in (["-m", "scripts.gate_unittest"], ["-m", "scripts.unittest_report"])
        or (stage_is_high_risk(stage) and stage.name != "static-check")
    )
    return not real_test and not parallel and (stage.kind != "check" or _reuse(stage))


def _uncached_command(stage: Stage, *, parallel: bool) -> bool:
    return _command_stage(stage, parallel=parallel) and not _reuse(stage) and not cargo_cacheable(stage)


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
        self, stage: Stage, live: LiveRuntime, entry: Evidence, *, timed_out: bool, defer_restore: bool = False
    ) -> Result:
        completed = entry.get("completed", {})
        actions = entry.get("actions", [])
        native_log = declared_file(entry, "run.command.log")
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
        if succeeded and cargo_cacheable(stage) and not defer_restore:
            try:
                restore_declared_cargo(self.root, entry)
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
                    if cargo_cacheable(stage)
                    else "reuse=disabled (unsupported producer)"
                    if stage.kind != "check"
                    else "reuse=native action cache",
                    f"evidence={event_path}",
                )
            ),
            log if log.is_file() else None,
            sum(action_elapsed(action) for action in actions),
            0 if outcome == "PASS" else None,
        )

    def close(self, *, success: bool = True) -> None:
        """Close this round's observer/actions and release its lease; keep the native server alive."""
        try:
            self.gate.close_persistent()
        finally:
            try:
                if self._service is not None:
                    self._service.close(
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
        self._identity = tool_identity(sysroot)
        # 调度实现不等于测试执行输入；规则、spec、适配器和实际导入仍各自绑定。
        runtime_identity = {key: value for key, value in self._identity.items() if key != "generator"}
        write_stable(self.directory / "tools.json", canonical_json(runtime_identity))
        # Generator control flow is not a Cargo source dependency. Its semantic
        # projection (argv/environment/spec/rules) is already the action key;
        # the executing adapter and real compiler/toolchain remain bound.
        write_stable(
            self.directory / "tools-compile.json",
            canonical_json(
                {key: value for key, value in self._identity.items() if key not in {"generator", "distributions"}}
            ),
        )
        self.diagnostics.record_overhead("tool_identity", time.perf_counter() - identity_started)
        python_path = str(Path(sys.executable).resolve())
        write_stable(
            self.directory / "tools-libtest-list.json",
            canonical_json(
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
            write_stable(self.root / "MODULE.bazel", read_published_text(source_root / "MODULE.bazel"))
            # 已知注册表摘要允许复用下载缓存；Bazel 仍自行验证/更新新模块的依赖图。
            source_lock = source_root / "MODULE.bazel.lock"
            if source_lock.is_file() and not (self.root / "MODULE.bazel.lock").exists():
                write_stable(self.root / "MODULE.bazel.lock", read_published_text(source_lock))
        if not (self.root / "BUILD.bazel").exists():
            write_stable(
                self.root / "BUILD.bazel",
                (
                    'filegroup(name="gate_sources", srcs=glob(["**"], exclude=["MODULE.bazel.lock", ".tmp/**", '
                    '"bazel-*/**", "**/__pycache__/**"]), visibility=["//visibility:public"])\n'
                    'filegroup(name = "gate_cargo_sources", srcs=glob(["**"], exclude=["MODULE.bazel.lock", '
                    '".tmp/**", "bazel-*", "**/__pycache__/**"]), visibility=["//visibility:public"])\n'
                ),
            )
        write_stable(
            self.directory / "gate_rules.bzl", (source_root / "scripts/gate_rules.bzl").read_text(encoding="utf-8")
        )
        write_stable(
            self.directory / "bazel_action.py", (source_root / "scripts/bazel_action.py").read_text(encoding="utf-8")
        )
        (self.directory / "scripts").mkdir(exist_ok=True)
        write_stable(
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
        write_stable(self.directory / "BUILD.bazel", "\n".join(adapter_build))

    def _inputs(self, stage: Stage) -> list[str]:
        selected = None if doctest(stage) else self._input_resolver.stage_inputs(stage)
        # Native --list cannot execute test bodies or Python distributions. Its real
        # binary/runtime inputs remain declared, and the adapter itself is separate.
        if libtest_listing(stage):
            tools = "//.tmp/bazel-gate:tools-libtest-list.json"
        elif cargo_cacheable(stage):
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
        if doctest(stage) and (self.root / "target").is_dir():
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
        key = input_key(stage)
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
        if libtest_listing(stage):
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
        doc_cache_safe = doc_cache_probe(self.root) if any(doctest(stage) for stage in stages) else None
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
            dependencies.update(command_dependencies(commands))

        def emit(stage: Stage) -> LiveRuntime:
            # 各阶段的 package/spec/BUILD 互相独立；共享的 InputResolver 缓存与
            # input_groups 在 GIL 下的竞争只会重复计算同一确定性结果，不改变输出。
            command = _command_stage(stage, parallel=parallel)
            cacheable = self._reusable[stage.name] or (not parallel and cargo_cacheable(stage))
            target = target_label(stage.name)
            package = self.root / bazel_label(stage.name).removeprefix("//").split(":")[0]
            package.mkdir(parents=True, exist_ok=True)
            semantic, dynamic = self._generate_environment(stage, environment, inherited_dynamic)
            spec: GateSpec = {
                "name": stage.name,
                "kind": stage.kind,
                "root": str(self.root),
                "argv": stage.argv,
                "environment": semantic,
                "watch_test_failures": stage.watch_test_failures,
                "cargo_artifacts": not parallel and cargo_cacheable(stage),
            }
            write_stable(package / "spec.json", canonical_json(spec))
            inputs = self._batch_inputs(stage, input_groups)
            if cargo_cacheable(stage):
                write_stable(
                    package / "cargo-configuration.json",
                    canonical_json(cargo_configuration_identity(self.root, semantic)),
                )
                inputs = [*inputs, ":cargo-configuration.json"]
            tags = ["local"]
            if not cacheable:
                tags.append("no-cache")
                if not command:
                    tags.append("external")
            tags.extend(resource_tags(stage, parallel=not command))
            if stage_is_high_risk(stage):
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
                rules.append("    cargo_artifacts = " + str(not parallel and cargo_cacheable(stage)) + ",")
                rules.append("    cacheable = " + str(cacheable) + ",")
            else:
                rules += ['    size = "enormous",', '    timeout = "eternal",']
            rules += ["    tags = " + json.dumps(tags) + ",", ")", ""]
            write_stable(package / "BUILD.bazel", "\n".join(rules))
            log = self.log_dir / f"native-{self._sequence:03d}-{target}.log"
            return {
                "environment": dynamic,
                "log": str(log),
                "result_file": str(log.with_suffix(".test-results")),
                "TEMP": os.environ.get("TEMP"),
                "TMP": os.environ.get("TMP"),
                "phase_file": str(self.log_dir / f"bazel-{self._sequence:03d}.phase.json")
                if command and not parallel and stage.kind != "mixed"
                else None,
            }

        with ThreadPoolExecutor(max_workers=min(8, (os.cpu_count() or 4))) as pool:
            # pool.map 保持阶段顺序，runtime 的构造与线程调度无关。
            lives = list(pool.map(emit, stages))
        runtime.update(zip((stage.name for stage in stages), lives, strict=True))
        return runtime

    def _dispatch_argv(self, stages: list[Stage], event_path: Path, *, parallel: bool) -> list[str]:
        if self.bazel is None:
            message = "Bazel 执行器尚未解析。"
            raise RuntimeError(message)
        high_risk = any(stage_is_high_risk(stage) for stage in stages)
        risk = "优先执行易错检查" if high_risk else "其余独立必需检查"
        if not parallel:
            risk = "按依赖顺序访问 Cargo 共享产物"
        native_tests = any(not _command_stage(stage, parallel=parallel) for stage in stages)
        labels = [bazel_label(stage.name) for stage in stages]
        # 逐用例目标可能数以千计，不能把全部标签塞进 Windows 命令行。
        target_file = self.log_dir / f"bazel-{self._sequence:03d}.targets"
        write_stable(target_file, "\n".join(labels) + "\n")
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
                    "--flaky_test_attempts=1",
                    # 同一目标跨门级/预算覆盖保持原生超时一致；
                    # 更严格的整门计费预算仍由外层监督器执行。
                    "--test_timeout=1500",
                    # 由 Bazel 根据本机 CPU / 内存及目标资源声明限流，不固定四槽。
                    "--local_test_jobs=0",
                    f"--local_resources=gate_host_slots={os.cpu_count() or 1}",
                    *[
                        "--local_resources=" + resource + "=1"
                        for resource in sorted(
                            {resource_name(stage.exclusive_resource) for stage in stages if stage.exclusive_resource}
                        )
                    ],
                ]
                if native_tests
                else []
            ),
            *(["--nouse_action_cache"] if any(_uncached_command(stage, parallel=parallel) for stage in stages) else []),
            "--target_pattern_file=" + str(target_file),
        ]

    def _restore_batch(self, stages: list[Stage], results: list[Result], evidence: dict[str, Evidence]) -> None:
        producers = [(stage, result) for stage, result in zip(stages, results, strict=True) if cargo_cacheable(stage)]
        try:
            snapshots: list[tuple[Path, Path]] = []
            for stage, result in producers:
                if result.status != "PASS":
                    continue
                entry = evidence.get(bazel_label(stage.name), {})
                manifest = cargo_manifest(stage, entry)
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
            "PYTHONPATH": str(Path(__file__).resolve().parents[1]) + os.pathsep + os.environ.get("PYTHONPATH", ""),
            "JCHTOOLS_GATE_BAZEL_SERVICE_LEASE": str(self._lease_path),
        }
        write_stable(
            pending,
            canonical_json({"argv": argv, "runtime": str(runtime_path), "environment": dispatch_environment}),
        )
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
        write_stable(path, canonical_json(runtime))
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
        evidence = events(event_path)
        timed_out = bool(stopped) and any(item.status == "TIMEOUT" for item in outer)
        results: list[Result] = []
        for stage in stages:
            entry = evidence.get(bazel_label(stage.name), {})
            command = _command_stage(stage, parallel=parallel)
            result = (
                native_result(
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
            high_tests = [
                stage for stage in stages if stage_is_high_risk(stage) and not _command_stage(stage, parallel=False)
            ]
            if high_tests:
                remainder = [stage for stage in stages if stage not in high_tests]
                return [(high_tests, True), *BazelSession._run_groups(remainder, parallel=False)]
        ordinary = [stage for stage in stages if stage.kind != "mixed"]
        mixed = [stage for stage in stages if stage.kind == "mixed"]
        compilers = [stage for stage in ordinary if stage.kind != "check" and not (parallel and doctest(stage))]
        checks = [stage for stage in ordinary if stage not in compilers]
        groups = [(checks, True), (compilers, False)] if parallel else [(ordinary, False)]
        groups.extend(([stage], False) for stage in mixed)
        bounded: list[tuple[list[Stage], bool]] = []
        for group, concurrent in groups:
            if concurrent:
                uncached = [stage for stage in group if _uncached_command(stage, parallel=True)]
                candidates = [stage for stage in group if stage not in uncached]
                high = [stage for stage in candidates if stage_is_high_risk(stage)]
                ordinary_checks = [stage for stage in candidates if not stage_is_high_risk(stage)]
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
    """发布本轮动作运行材料；原生 client 由持久 broker 直接持有，避免启动取消遗留子进程。."""
    config = cast("DispatchConfig", json.loads(read_published_text(Path(config_path))))
    runtime_path = Path(config["runtime"])
    runtime = cast("dict[str, LiveRuntime]", json.loads(read_published_text(runtime_path)))
    root = runtime_path.parents[2]
    for name, live in runtime.items():
        live["failure_file"] = os.environ.get("JCHTOOLS_GATE_FAILURE_FILE")
        live["TEMP"] = os.environ.get("TEMP")
        live["TMP"] = os.environ.get("TMP")
        # 每个用例只读取自己的动态元数据，避免 N 个进程各自解析 N 项清单。
        package = root / bazel_label(name).removeprefix("//").split(":")[0]
        write_stable(package / "spec.runtime.json", canonical_json(live))
    return 0


def _lease_client(directory: str) -> int:
    """仅观察本轮请求；原生服务 Job 由外层 CLI 持有。."""
    requests = Path(directory)
    write_stable(
        requests / "client.json",
        canonical_json(
            {key: os.environ[key] for key in ("JCHTOOLS_GATE_FAILURE_FILE", "TEMP", "TMP") if key in os.environ}
        ),
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
