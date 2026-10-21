"""复用既有检查和测试原语；不改判据，不触发桌面鼠标或远程流水线."""

from __future__ import annotations

import json
import os
import shutil
import sys
from dataclasses import replace
from pathlib import Path
from typing import TYPE_CHECKING

from scripts.gate_runtime import GateStoppedError, Result, Stage
from scripts.quality_scope import production_python
from scripts.xberg_test_engine import EXPECTED_TEST_ENGINE

if TYPE_CHECKING:
    from collections.abc import Callable
    from typing import TypeIs

    from scripts.bazel_gate import BazelSession

_parse_json: Callable[[str], object] = json.loads

# 这些 artifact 共用 tests/common/engine_session.rs 的跨进程锁。
# 声明给 Bazel 原生资源调度，避免耗尽测试内部的 180s 锁等待预算。
_ENGINE_SESSION_TEST_TARGETS = frozenset(
    {
        "background_settings",
        "engine_cleanup_filter",
        "jchocr",
        "markdown_convert_gate",
        "markdown_media_e2e",
        "xberg_legacy_protocol",
        "xberg_shared_process",
        "xberg_unknown_response",
    }
)


def _object_array(value: object) -> TypeIs[list[object]]:
    return isinstance(value, list)


def _mapping(value: object) -> TypeIs[dict[str, object]]:
    return isinstance(value, dict)


def static_plan(root: Path) -> list[Stage]:
    """严格静态检查仅覆盖生产；语法检查仍覆盖全部 Python，不执行 unittest."""
    python_files = production_python(root)
    return [
        Stage("static-check", [sys.executable, str(root / "scripts/static_check.py")]),
        Stage("rustfmt-check", [sys.executable, "-m", "scripts.quality_scope", "rustfmt"]),
        Stage("pyquality-syntax", [sys.executable, "-m", "scripts.quality_scope", "syntax"]),
        Stage("pyquality-ruff-format", [sys.executable, "-m", "ruff", "format", "--check", "scripts", "typings"]),
        Stage("pyquality-ruff-lint", [sys.executable, "-m", "ruff", "check", "scripts", "typings"]),
        Stage("pyquality-basedpyright", [sys.executable, "-m", "basedpyright"]),
        Stage("pyquality-vulture", [sys.executable, "-m", "vulture", *python_files, "--min-confidence", "100"]),
        Stage("pyquality-bandit", [sys.executable, "-m", "bandit", *python_files, "-s", "B404,B603", "-q"]),
    ]


def gate_plan(level: str, root: Path, log_dir: Path, ps_env: dict[str, str]) -> list[Stage]:
    """功能门保留非打包覆盖；slow 增加完整本地发布验证，optional 桌面边界不变。."""
    cargo = shutil.which("cargo") or "cargo"
    plan = [
        *static_plan(root),
        Stage("production-build", [cargo, "build", "--bin", "JchTools"], "compile"),
        Stage("test-target-check", [cargo, "check", "--all-targets", "--features", "test-hooks"], "compile"),
        Stage(
            "clippy",
            [
                cargo,
                "clippy",
                "--lib",
                "--bin",
                "JchTools",
                "--bin",
                "jchocr",
                "--features",
                "test-hooks",
                "--",
                "-D",
                "warnings",
            ],
            "compiler-observed",
        ),
    ]
    if level == "fastcheck":
        return plan
    plan += [
        Stage(
            "markdown-acceptance-unit",
            [sys.executable, "-m", "scripts.unittest_report", "-f", "scripts.test_markdown_acceptance"],
        ),
        Stage("gui-automation-unit", [sys.executable, "-m", "scripts.unittest_report", "-f", "scripts.test_gui_smoke"]),
        Stage("test-gate-unit", [sys.executable, "-m", "scripts.unittest_report", "-f", "scripts.test_test_gate"]),
        Stage(
            "trusted-review-unit",
            [sys.executable, "-m", "scripts.unittest_report", "-f", "scripts.test_review_trusted"],
        ),
        Stage(
            "worker-root-isolation-unit",
            [
                sys.executable,
                "-m",
                "scripts.unittest_report",
                "discover",
                "-f",
                "-s",
                "tests/ocr_fixtures",
                "-p",
                "test_*.py",
            ],
        ),
        Stage("ocr-asset-manifest", [sys.executable, "tests/ocr_fixtures/check_asset_manifest.py"]),
        Stage("requirement-reference-inventory", [sys.executable, "scripts/requirement_coverage.py"]),
        Stage("xberg-latest", [sys.executable, "scripts/xberg_test_engine.py"]),
        Stage(
            "clippy-perf-tracing",
            [
                cargo,
                "clippy",
                "--lib",
                "--bin",
                "JchTools",
                "--bin",
                "jchocr",
                "--features",
                "perf-tracing,test-hooks",
                "--",
                "-D",
                "warnings",
            ],
            "compiler-observed",
        ),
        # cargo test --all-targets --no-run 只保证测试 artifact；公开后台入口
        # 必须另行构建 test-hooks 程序，不能沿用前面的生产 EXE。
        Stage(
            "test-program-build",
            [cargo, "build", "--bins", "--features", "test-hooks"],
            "compile",
        ),
        Stage(
            "root-test-build",
            [cargo, "test", "--all-targets", "--features", "test-hooks", "--no-run", "--message-format=json"],
            "compile",
        ),
        Stage(
            "root-doctests",
            [cargo, "test", "--doc", "--features", "test-hooks", "--"],
            "compiler-observed",
            watch_test_failures=True,
        ),
    ]
    for component in ("snap-ocr-core", "snap-ocr-worker"):
        manifest = f"optional/{component}/Cargo.toml"
        if component == "snap-ocr-worker":
            # 库测试会用独立 worker EXE 作隔离代理；测试 artifact 不能代替该程序。
            plan.append(
                Stage(
                    "snap-ocr-worker-program-build",
                    [
                        cargo,
                        "build",
                        "--manifest-path",
                        manifest,
                        "--bin",
                        "snap-ocr-worker",
                        "--features",
                        "test-hooks",
                    ],
                    "compile",
                )
            )
        plan += [
            Stage(
                f"{component}-clippy",
                [
                    cargo,
                    "clippy",
                    "--manifest-path",
                    manifest,
                    "--lib",
                    *(["--bin", "snap-ocr-worker"] if component == "snap-ocr-worker" else []),
                    "--features",
                    "test-hooks",
                    "--",
                    "-D",
                    "warnings",
                ],
                "compiler-observed",
            ),
            Stage(
                f"{component}-test-build",
                [
                    cargo,
                    "test",
                    "--manifest-path",
                    manifest,
                    "--all-targets",
                    "--features",
                    "test-hooks",
                    "--no-run",
                    "--message-format=json",
                ],
                "compile",
            ),
            Stage(
                f"{component}-doctests",
                [cargo, "test", "--manifest-path", manifest, "--doc", "--features", "test-hooks", "--"],
                "compiler-observed",
                watch_test_failures=True,
            ),
        ]
    if level != "slowtest":
        return plan
    plan += [
        # 全量在线供应链审计依赖外部索引响应，不属于快速功能回归。
        Stage("pyquality-pip-audit", [sys.executable, "-m", "pip_audit", "-r", "scripts/requirements-dev.txt"]),
        Stage("release-target-build", [cargo, "build", "--all-targets", "--release"], "compile"),
        Stage(
            "package-windows-unit",
            [sys.executable, "-m", "scripts.unittest_report", "-f", "scripts.test_package_windows"],
            # 独立用例进程共用类级工作目录，其创建者清理不能与同套件并发。
            exclusive_resource="packaging-test-workspace",
        ),
        Stage(
            "fetch-7zip-unit",
            [sys.executable, "-m", "scripts.unittest_report", "-f", "scripts.test_fetch_7zip"],
        ),
    ]
    phase = log_dir / "package-phase.json"
    powershell = shutil.which("powershell") or "powershell"
    plan.append(
        Stage(
            "package",
            [
                powershell,
                "-NoProfile",
                "-File",
                str(root / "scripts/package-windows.ps1"),
                "-SkipTests",
                "-DestinationRoot",
                str(log_dir / "package"),
                "-GateTimingFile",
                str(phase),
            ],
            "mixed",
            ps_env,
            phase,
            exclusive_resource="bundled-7zip",
        )
    )
    return plan


def _test_binary_stages(build: Result) -> list[Stage]:
    """使用本轮 Cargo 发布的 artifact，不猜旧 target 中的文件名."""
    if build.log is None:
        return []
    stages: list[Stage] = []
    for line in build.log.read_text(encoding="utf-8-sig", errors="replace").splitlines():
        try:
            message = _parse_json(line)
        except ValueError:
            continue
        if not _mapping(message) or message.get("reason") != "compiler-artifact":
            continue
        profile, target, executable = message.get("profile"), message.get("target"), message.get("executable")
        if (
            not _mapping(profile)
            or profile.get("test") is not True
            or not _mapping(target)
            or not isinstance(executable, str)
        ):
            continue
        name = target.get("name")
        if isinstance(name, str):
            if name == "gui_flow":
                continue  # 真实 winit 窗口会激活用户桌面，保留独立入口。
            argv = [executable, "--nocapture", "--test-threads=2"]
            if name == "acp_service_process":
                for test in (
                    "real_gui_save_close_kill_reopen_and_confirmed_service_exit",
                    "real_gui_pending_configuration_applies_only_after_inflight_completion",
                ):
                    argv += ["--skip", test]
            # 新 package 的同名 lib/bin/integration（jchocr）不能覆盖彼此的执行记录。
            kind = target.get("kind")
            suffix = "-lib" if kind == ["lib"] else "-bin" if kind == ["bin"] else ""
            shared_session = (
                kind == ["test"] and name in _ENGINE_SESSION_TEST_TARGETS
            ) or build.name == "snap-ocr-worker-test-build"
            stages.append(
                Stage(
                    f"{build.name}-{name}{suffix}",
                    argv,
                    exclusive_resource="engine-session" if shared_session else None,
                    watch_test_failures=True,
                )
            )
    return stages


def _real_test_stage(
    binaries: list[Stage],
    target: tuple[str, str],
    env: dict[str, str] | None = None,
) -> Stage:
    identifier, name = target
    binary = next((stage for stage in binaries if stage.name == f"root-test-build-{identifier}"), None)
    if binary is None:
        raise GateStoppedError([Result(name, "UNVERIFIED", "本轮编译未发布所需测试 artifact。")])
    argv = [binary.argv[0], "--ignored", "--nocapture", "--test-threads=1"]
    return Stage(name, argv, env=env, watch_test_failures=True)


def _compile_test_artifacts(run: BazelSession, plan: list[Stage], results: list[Result]) -> list[Stage]:
    # Producer 保持原相对顺序；发现只能在整个调用（含 main/worker 恢复）
    # 完成后开始。Doctest 不生产运行 artifact，留给最终真实 Bazel test 批次。
    completed = {result.name for result in results if result.status == "PASS"}
    selected = [
        stage
        for stage in plan
        if (stage.kind in {"compile", "compiler-observed"} or stage.name == "xberg-latest")
        and stage.name not in completed
        and not stage.name.endswith("-doctests")
    ]
    if selected:
        results.extend(run.run(selected, parallel=False))
    binaries: list[Stage] = []
    by_name = {result.name: result for result in results}
    for stage in plan:
        if not stage.name.endswith("-test-build"):
            continue
        result = by_name.get(stage.name)
        if result is None or result.status != "PASS":
            raise GateStoppedError([Result(f"{stage.name}-artifacts", "FAIL", "缺少本轮成功编译的 BEP 结果。")])
        found = _test_binary_stages(result)
        if not found:
            detail = "Cargo 未发布任何本轮测试 artifact，不能冒充已测试。"
            raise GateStoppedError([Result(f"{result.name}-artifacts", "FAIL", detail)])
        binaries.extend(found)
    return binaries


def _real_coverage(results: list[Result], binaries: list[Stage], root: Path) -> list[Stage]:
    stages: list[Stage] = []
    engine = os.environ.get("JCHTOOLS_TEST_7ZIP") or str(root / "resources/7zip/7z.exe")
    if Path(engine).is_file():
        # 每个 archive case 和库回归都用独占 tempdir/data/input/state；
        # 共用的只是只读 7z EXE，没有共享输出或引擎配置需要跨进程互斥。
        stages.extend(
            _real_test_stage(binaries, target, {"JCHTOOLS_TEST_7ZIP": engine})
            for target in (
                ("archive", "real-engine-tests"),
                ("jchtools_recursive_extract-lib", "real-engine-lib-tests"),
            )
        )
    else:
        raise GateStoppedError([Result("real-engine-tests", "UNVERIFIED", "缺少真实 7-Zip 引擎。")])
    if any(result.name == "xberg-latest" and result.status == "PASS" for result in results):
        stage = _real_test_stage(
            binaries,
            ("xberg_assets", "real-xberg-presence"),
            {"JCHTOOLS_REAL_XBERG_DIR": str(EXPECTED_TEST_ENGINE)},
        )
        stages.append(stage)
    else:
        raise GateStoppedError([Result("real-xberg-presence", "UNVERIFIED", "最新真实 Xberg 前置未通过。")])
    # 清单发现不运行媒体测试，也不读取媒体输入；夹具可与发现同批准备。
    media = _real_test_stage(binaries, ("markdown_media_e2e", "real-media-e2e"))
    # 两个用例各自保存隔离状态目录；逐个 Job 收尾后才能启动下一套共享代理。
    stages.extend(
        Stage(
            f"{media.name}-{case}",
            [*media.argv, "--exact", case],
            env=media.env,
            exclusive_resource="shared-xberg-media",
            watch_test_failures=True,
        )
        for case in (
            "real_component_trackless_media_reports_no_audio",
            "real_component_transcribe_returns_structured_markdown",
        )
    )
    return stages


def _real_media_stage(
    run: BazelSession, results: list[Result], binaries: list[Stage], root: Path, ps_env: dict[str, str] | None
) -> Stage:
    environment = _media_environment(run, results, root, ps_env)
    if environment is None:
        detail = "真实媒体组件、语音/无音轨合成输入及期望文本前置未齐；用例保留。"
        raise GateStoppedError([Result("real-media-e2e", "UNVERIFIED", detail)])
    broker = os.environ.get("JCHTOOLS_TEST_BROKER_EXE") or (ps_env or {}).get("JCHTOOLS_TEST_GUI_EXE")
    if broker is None or not Path(broker).is_absolute() or not Path(broker).is_file():
        raise GateStoppedError([Result("real-media-e2e", "UNVERIFIED", "真实媒体测试缺少本轮 JchTools 代理绝对 EXE。")])
    return _real_test_stage(
        binaries,
        ("markdown_media_e2e", "real-media-e2e"),
        environment | {"JCHTOOLS_TEST_BROKER_EXE": broker},
    )


def _media_fixture_stage(run: BazelSession, root: Path, ps_env: dict[str, str] | None) -> Stage:
    return Stage(
        "media-e2e-fixtures",
        [
            sys.executable,
            str(root / "scripts/media_e2e_fixture.py"),
            "--component-dir",
            str(EXPECTED_TEST_ENGINE),
            "--output",
            str(run.log_dir / "media-e2e-fixtures.json"),
        ],
        env=(ps_env or {}) | {"PYTHONIOENCODING": "utf-8"},
    )


def _media_environment(
    run: BazelSession, results: list[Result], root: Path, ps_env: dict[str, str] | None
) -> dict[str, str] | None:
    required = (
        "JCHTOOLS_MEDIA_E2E_COMPONENT_DIR",
        "JCHTOOLS_MEDIA_E2E_INPUT",
        "JCHTOOLS_MEDIA_E2E_TRACKLESS_INPUT",
        "JCHTOOLS_MEDIA_E2E_EXPECT_TEXT",
    )
    configured = {key: os.environ[key] for key in required if os.environ.get(key)}
    if configured:
        return configured if len(configured) == len(required) else None
    if not any(result.name == "xberg-latest" and result.status == "PASS" for result in results):
        return None
    output = run.log_dir / "media-e2e-fixtures.json"
    prepared = [result for result in results if result.name == "media-e2e-fixtures"]
    if not prepared:
        prepared = run.run([_media_fixture_stage(run, root, ps_env)])
        results.extend(prepared)
    if not prepared or prepared[0].status != "PASS":
        return None
    value = _parse_json(output.read_text(encoding="utf-8"))
    if not _mapping(value):
        message = "合成媒体前置输出不是环境对象。"
        raise ValueError(message)
    environment: dict[str, str] = {}
    for key in required:
        field = value.get(key)
        if not isinstance(field, str) or not field.strip():
            message = f"合成媒体前置缺少有效字段：{key}"
            raise ValueError(message)
        environment[key] = field
    return environment


def _discovery_text(result: Result) -> str:
    if result.status != "PASS" or result.log is None:
        message = "测试发现未成功或缺少原始清单日志。"
        raise ValueError(message)
    return result.log.read_text(encoding="utf-8-sig")


def _rust_case_list(text: str) -> dict[str, str]:
    """Libtest terse 清单没有汇总行；成功退出且空日志才是有效空集。."""
    cases: dict[str, str] = {}
    for line in text.splitlines():
        identifier, separator, kind = line.rpartition(": ")
        if (
            not separator
            or not identifier
            or identifier != identifier.strip()
            or not identifier.isprintable()
            or kind not in {"test", "benchmark"}
            or identifier in cases
        ):
            message = f"无效或重复的 libtest 清单项：{line!r}"
            raise ValueError(message)
        cases[identifier] = kind
    return cases


def _python_case_list(text: str) -> list[str]:
    prefix = "JCHTOOLS_TEST_CASES="
    lines = text.splitlines()
    protocol = [line for line in lines if line.startswith(prefix)]
    if len(protocol) != 1 or not lines or lines[-1] != protocol[0]:
        message = "Python 发现日志缺少唯一的末尾用例协议行。"
        raise ValueError(message)
    value = _parse_json(protocol[0][len(prefix) :])
    if not _object_array(value):
        message = "Python 用例清单不是数组。"
        raise TypeError(message)
    identifiers: list[str] = []
    seen: set[str] = set()
    for identifier in value:
        if (
            not isinstance(identifier, str)
            or not identifier
            or identifier != identifier.strip()
            or not identifier.isprintable()
        ):
            message = "Python 用例清单包含无效 ID。"
            raise ValueError(message)
        # 同 ID 的原始实例由 gate_unittest 一起执行，不创建同名 Bazel target。
        if identifier not in seen:
            seen.add(identifier)
            identifiers.append(identifier)
    return identifiers


def _listed_cases(stage: Stage, result: Result | None, ignored_result: Result | None, *, python: bool) -> list[str]:
    if result is None:
        message = "Bazel 未返回用例发现结果。"
        raise ValueError(message)
    if python:
        return _python_case_list(_discovery_text(result))
    all_cases = _rust_case_list(_discovery_text(result))
    if ignored_result is None:
        message = "Bazel 未返回 ignored 子集发现结果。"
        raise ValueError(message)
    ignored = _rust_case_list(_discovery_text(ignored_result))
    if any(all_cases.get(identifier) != kind for identifier, kind in ignored.items()):
        message = "ignored 清单不是完整清单的同类型子集。"
        raise ValueError(message)
    return [
        identifier
        for identifier, kind in all_cases.items()
        if kind == "test" and ((identifier in ignored) == ("--ignored" in stage.argv))
    ]


def _require_unique_stage_names(stages: list[Stage]) -> None:
    if len({stage.name for stage in stages}) != len(stages):
        raise GateStoppedError([Result("test-case-targets", "FAIL", "测试阶段名称碰撞，不能覆盖用例。")])


def _expand_test_cases(
    run: BazelSession,
    stages: list[Stage],
    results: list[Result],
    *,
    preparations: list[Stage] | None = None,
) -> list[Stage]:
    """发现与执行均经 Bazel；无依赖盘点与发现同批，不缓存手工测试清单。."""
    discoveries: list[Stage] = []
    _require_unique_stage_names(stages)
    python_stages: set[str] = set()
    for stage in stages:
        python = stage.argv[1:3] == ["-m", "scripts.unittest_report"]
        if python:
            python_stages.add(stage.name)
            argv = [stage.argv[0], "-m", "scripts.gate_unittest", "--list", "--", *stage.argv[3:]]
            discoveries.append(replace(stage, name=f"{stage.name}-list", argv=argv, watch_test_failures=False))
        else:
            # ignored 阶段的全集仅去掉选择开关；--exact/--skip 等原始过滤保持不变。
            argv = [argument for argument in stage.argv if argument != "--ignored"]
            for suffix, extra in (("list", []), ("ignored-list", ["--ignored"])):
                discoveries.append(
                    replace(
                        stage,
                        name=f"{stage.name}-{suffix}",
                        argv=[*argv, *extra, "--list", "--format=terse"],
                        watch_test_failures=False,
                    )
                )
    _require_unique_stage_names(discoveries)
    discovered = run.run([*(preparations or []), *discoveries])
    results.extend(discovered)
    by_name = {result.name: result for result in discovered}
    expanded: list[Stage] = []
    for stage in stages:
        result = by_name.get(f"{stage.name}-list")
        try:
            identifiers = _listed_cases(
                stage,
                result,
                by_name.get(f"{stage.name}-ignored-list"),
                python=stage.name in python_stages,
            )
            if result is not None:
                result.detail += f"；发现 {len(identifiers)} 个独立测试 ID（零用例保留此成功清单证据）。"
        except (OSError, UnicodeError, ValueError, TypeError) as error:
            raise GateStoppedError(
                [
                    Result(
                        f"{stage.name}-cases",
                        "FAIL",
                        f"测试发现清单损坏：{error}",
                        log=result.log if result else None,
                    )
                ]
            ) from error
        for identifier in identifiers:
            if stage.name in python_stages:
                argv = [
                    stage.argv[0],
                    "-m",
                    "scripts.gate_unittest",
                    "--case",
                    identifier,
                    "--",
                    *stage.argv[3:],
                ]
            else:
                # 已经精确选择的真实媒体阶段不能重复传递 libtest 单值开关。
                argv = [*stage.argv, *([] if "--exact" in stage.argv else ["--exact"]), identifier]
            # 只隔离已观察到启动竞争的短墙钟场景，不串行化整个测试模块：
            # ACP 屏障为 10s；PowerShell 后代必须在原有 5s 空闲超时前完成 PID 握手。
            resource = (
                "gate-wall-clock"
                if identifier.startswith("scripts.test_test_gate.GateFailFastTests.")
                or identifier
                in {
                    "real_service_discover_during_starting_is_minimal_stable_and_read_only",
                    "process::tests::streaming_terminal_paths_reap_owned_descendants",
                }
                else stage.exclusive_resource
            )
            expanded.append(replace(stage, name=f"{stage.name}-{identifier}", argv=argv, exclusive_resource=resource))
    _require_unique_stage_names(expanded)
    return expanded


def run_full_coverage(  # noqa: PLR0913 - 门级和测试环境必须独立于可选的打包阶段传入。
    run: BazelSession,
    plan: list[Stage],
    results: list[Result],
    snapshot: list[str],
    root: Path,
    *,
    level: str,
    ps_env: dict[str, str],
) -> None:
    """功能覆盖执行一次；只有 slow tier 在其后执行完整本地打包验证。."""
    binaries = _compile_test_artifacts(run, plan, results)
    pending = [*(stage for stage in plan if stage.name.endswith("-unit")), *binaries]
    results.append(
        Result(
            "real-opencode",
            "NOT_RUN_SEPARATE_USER_INSTRUCTION_REQUIRED",
            "真实模型验收保留独立 acp_opencode 入口；门授权或 JCHTOOLS_TEST_OPENCODE_EXE 配置不授权模型调用。",
        )
    )
    results.append(
        Result(
            "desktop-tests",
            "NOT_RUN_SEPARATE_USER_INSTRUCTION_REQUIRED",
            "gui_flow、ACP real_gui、桌面 driver 和 worker-root 保留独立入口：软件渲染不隔离焦点，热键也需隔离。",
        )
    )
    results.append(
        Result(
            "performance-benchmarks",
            "NOT_RUN_SEPARATE_USER_INSTRUCTION_REQUIRED",
            "perf_probe 的三个 ignored 性能基准保留独立 release 手工入口；未纳入本地门，不计为已验证。",
        )
    )
    pending.extend(_real_coverage(results, binaries, root))
    command = "from scripts.test_gate import snapshot_lines; print('\\n'.join(snapshot_lines()))"
    preparations = [
        Stage("source-snapshot", [sys.executable, "-c", command], env={"PYTHONIOENCODING": "utf-8"}),
        *(stage for stage in plan if stage.name in {"ocr-asset-manifest", "requirement-reference-inventory"}),
    ]
    if not any(
        os.environ.get(key)
        for key in (
            "JCHTOOLS_MEDIA_E2E_COMPONENT_DIR",
            "JCHTOOLS_MEDIA_E2E_INPUT",
            "JCHTOOLS_MEDIA_E2E_TRACKLESS_INPUT",
            "JCHTOOLS_MEDIA_E2E_EXPECT_TEXT",
        )
    ):
        preparations.append(_media_fixture_stage(run, root, ps_env))
    expanded = _expand_test_cases(run, pending, results, preparations=preparations)
    media = _real_media_stage(run, results, binaries, root, ps_env)
    expanded = [
        replace(stage, env=media.env) if stage.name.startswith("real-media-e2e-") else stage for stage in expanded
    ]
    source = next((result for result in results if result.name == "source-snapshot"), None)
    if source is not None and source.log is not None:
        snapshot.extend(source.log.read_text(encoding="utf-8", errors="replace").splitlines())
    # 审计与 doctest 不消费引擎状态；同真实用例并行。Doctest 仍使用原 Cargo
    # 命令，是否可复用由 Bazel 的完整源码/编译工具/运行材料输入声明决定。
    independent = [stage for stage in plan if stage.name == "pyquality-pip-audit" or stage.name.endswith("-doctests")]
    results.extend(run.run([*independent, *expanded]))
    if level != "slowtest":
        return
    package = next(stage for stage in plan if stage.name == "package")
    results.extend(run.run([package], parallel=False))
    if shutil.which("ISCC.exe") is None and not any(
        (Path(os.environ.get(key, "")) / "Inno Setup 6/ISCC.exe").is_file()
        for key in ("PROGRAMFILES", "PROGRAMFILES(X86)")
    ):
        raise GateStoppedError(
            [Result("installer-environment", "UNVERIFIED", "ISCC 不可用；便携 ZIP 不能代替安装包验证。")]
        )
