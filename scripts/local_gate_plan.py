"""复用既有检查和测试原语；不改判据，不触发桌面鼠标或远程流水线."""

from __future__ import annotations

import json
import os
import shutil
import sys
from pathlib import Path
from typing import TYPE_CHECKING

from scripts.gate_runtime import Result, Stage
from scripts.quality_scope import production_python
from scripts.xberg_test_engine import EXPECTED_TEST_ENGINE

if TYPE_CHECKING:
    from collections.abc import Callable
    from typing import TypeIs

    from scripts.cargo_delta import ImpactSelection
    from scripts.gate_runtime import GateRun

_parse_json: Callable[[str], object] = json.loads


TEST_WORKER_CAP = 28


def test_worker_count() -> int:
    """Cap test concurrency at the CPUs available to this process."""
    return max(1, min(TEST_WORKER_CAP, os.process_cpu_count() or 1))


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


def _package_args(packages: frozenset[str] | set[str]) -> list[str]:
    return [arg for package in sorted(packages) for arg in ("-p", package)]


def _feature_args(scope: ImpactSelection, packages: frozenset[str] | set[str], *features: str) -> list[str]:
    enabled = [
        f"{package}/{feature}"
        for package in sorted(packages)
        for feature in features
        if feature in scope.features.get(package, frozenset())
    ]
    return ["--features", ",".join(enabled)] if enabled else []


def _optional_plan(level: str, cargo: str, scope: ImpactSelection, plan: list[Stage]) -> None:
    """追加选中的 optional 组件阶段."""
    for component in ("snap-ocr-core", "snap-ocr-worker"):
        if component not in scope.packages:
            continue
        manifest = f"optional/{component}/Cargo.toml"
        if component == "snap-ocr-worker" and level != "fastcheck":
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
        component_hooks = _feature_args(scope, {component}, "test-hooks")
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
                    *component_hooks,
                    "--",
                    "-D",
                    "warnings",
                ],
                "compiler-observed",
            ),
        ]
        if level == "fastcheck":
            plan.append(
                Stage(
                    f"{component}-test-target-check",
                    [cargo, "check", "--manifest-path", manifest, "--all-targets", *component_hooks],
                    "compile",
                )
            )
            continue
        plan += [
            Stage(
                f"{component}-test-build",
                [
                    cargo,
                    "test",
                    "--manifest-path",
                    manifest,
                    "--all-targets",
                    *component_hooks,
                    "--no-run",
                    "--message-format=json",
                ],
                "compile",
            ),
            Stage(
                f"{component}-doctests",
                [
                    cargo,
                    "test",
                    "--manifest-path",
                    manifest,
                    "--doc",
                    *component_hooks,
                    "--no-fail-fast",
                    "--",
                    "--quiet",
                ],
                "compiler-observed",
            ),
        ]


def gate_plan(level: str, root: Path, log_dir: Path, ps_env: dict[str, str], scope: ImpactSelection) -> list[Stage]:
    """按包选取 Cargo 阶段；全局静态检查和完整发布验证保持不变."""
    cargo = shutil.which("cargo") or "cargo"
    default = scope.packages & scope.default_packages
    packages = _package_args(default)
    hooks = _feature_args(scope, default, "test-hooks")
    plan = static_plan(root)
    if "jchtools" in default:
        plan.append(Stage("production-build", [cargo, "build", "-p", "jchtools", "--bin", "JchTools"], "compile"))
    if default:
        bins = [
            arg
            for name, package in (("JchTools", "jchtools"), ("jchocr", "jchocr"))
            if package in default
            for arg in ("--bin", name)
        ]
        plan += [
            Stage("test-target-check", [cargo, "check", *packages, "--all-targets", *hooks], "compile"),
            Stage(
                "clippy",
                [cargo, "clippy", *packages, "--lib", *bins, *hooks, "--", "-D", "warnings"],
                "compiler-observed",
            ),
        ]
        if level != "fastcheck":
            perf = _feature_args(scope, default, "perf-tracing", "test-hooks")
            plan += [
                Stage(
                    "clippy-perf-tracing",
                    [cargo, "clippy", *packages, "--lib", *bins, *perf, "--", "-D", "warnings"],
                    "compiler-observed",
                ),
                Stage(
                    "root-test-build",
                    [cargo, "test", *packages, "--all-targets", *hooks, "--no-run", "--message-format=json"],
                    "compile",
                ),
                Stage(
                    "root-doctests",
                    [cargo, "test", *packages, "--doc", *hooks, "--no-fail-fast", "--", "--quiet"],
                    "compiler-observed",
                ),
            ]
    if level != "fastcheck":
        plan.append(Stage("requirement-reference-inventory", [sys.executable, "scripts/requirement_coverage.py"]))
        if "jchtools" in default or "snap-ocr-core" in scope.packages or "snap-ocr-worker" in scope.packages:
            plan.append(Stage("ocr-asset-manifest", [sys.executable, "tests/ocr_fixtures/check_asset_manifest.py"]))
        if "jchtools" in default:
            plan.append(
                Stage(
                    "xberg-engine",
                    [sys.executable, "scripts/xberg_test_engine.py", *(["--offline"] if level == "fulltest" else [])],
                )
            )
        if default & {"jchtools", "jchtools-acp-api"}:
            # ACP 库测试直接启动根包的 fixture，不能依赖测试 artifact。
            plan.append(
                Stage(
                    "test-program-build",
                    [cargo, "build", "-p", "jchtools", "--bins", "--features", "test-hooks"],
                    "compile",
                )
            )
    _optional_plan(level, cargo, scope, plan)
    if level != "slowtest":
        return plan
    plan += [
        # 全量在线供应链审计依赖外部索引响应，不属于快速功能回归。
        Stage("pyquality-pip-audit", [sys.executable, "-m", "pip_audit", "-r", "scripts/requirements-dev.txt"]),
        Stage("release-target-build", [cargo, "build", "--all-targets", "--release"], "compile"),
    ]
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
            ],
            "mixed",
            ps_env,
        )
    )
    return plan


def _nextest_stage(  # noqa: PLR0913 - 组件、筛选、忽略测试及环境分别控制原生测试参数。
    root: Path,
    name: str,
    *,
    component: str | None = None,
    selection: list[str] | None = None,
    ignored: bool = False,
    env: dict[str, str] | None = None,
    cargo_args: list[str] | None = None,
) -> Stage:
    cargo = shutil.which("cargo") or "cargo"
    argv = [
        cargo,
        "nextest",
        "run",
        "--config-file",
        str(root / ".config/nextest.toml"),
        "--test-threads",
        str(test_worker_count()),
        "--retries",
        "0",
        "--no-fail-fast",
        "--color",
        "never",
        "--no-tests",
        "fail",
        "--status-level",
        "fail",
        "--failure-output",
        "immediate",
        "--show-progress",
        "none",
    ]
    if component:
        argv += ["--manifest-path", str(root / "optional" / component / "Cargo.toml")]
    argv += ["--all-targets", *(cargo_args if cargo_args is not None else ["--features", "test-hooks"])]
    if ignored:
        argv += ["--run-ignored", "only"]
    argv += selection or []
    return Stage(name, argv, env=env)


def _run_compilation(run: GateRun, plan: list[Stage], results: list[Result]) -> None:
    completed = {result.name for result in results}
    selected = [
        stage
        for stage in plan
        if (stage.kind in {"compile", "compiler-observed"} or stage.name == "xberg-engine")
        and stage.name not in completed
        and not stage.name.endswith("-doctests")
    ]
    if selected:
        _ = run.run(selected)


def _real_coverage(results: list[Result], root: Path, scope: ImpactSelection) -> list[Stage]:
    stages: list[Stage] = []
    selected = scope.packages & scope.default_packages
    engine_packages = selected & {"jchtools", "jchtools-recursive-extract"}
    if engine_packages:
        engine = os.environ.get("JCHTOOLS_TEST_7ZIP") or str(root / "resources/7zip/7z.exe")
        if not Path(engine).is_file():
            results.append(Result("real-engine-tests", "UNVERIFIED", "缺少真实 7-Zip 引擎。"))
        else:
            for package, name, expression in (
                ("jchtools", "real-engine-tests", "binary(=archive)"),
                (
                    "jchtools-recursive-extract",
                    "real-engine-lib-tests",
                    "package(=jchtools-recursive-extract) and kind(lib)",
                ),
            ):
                if package in engine_packages:
                    one = {package}
                    stages.append(
                        _nextest_stage(
                            root,
                            name,
                            ignored=True,
                            selection=["-E", expression],
                            cargo_args=[*_package_args(one), *_feature_args(scope, one, "test-hooks")],
                            env={"JCHTOOLS_TEST_7ZIP": engine},
                        )
                    )
    if "jchtools" not in selected:
        return stages
    if not any(result.name == "xberg-engine" and result.status == "PASS" for result in results):
        results.append(Result("real-xberg-presence", "UNVERIFIED", "真实 Xberg 引擎前置未通过。"))
    else:
        stages.append(
            _nextest_stage(
                root,
                "real-xberg-presence",
                ignored=True,
                cargo_args=["-p", "jchtools", *_feature_args(scope, {"jchtools"}, "test-hooks")],
                selection=["-E", "binary(=xberg_assets) and test(=real_xberg_dir_passes_presence_checks)"],
                env={"JCHTOOLS_REAL_XBERG_DIR": str(EXPECTED_TEST_ENGINE)},
            )
        )
    return stages


def _real_media_stages(run: GateRun, results: list[Result], root: Path, ps_env: dict[str, str] | None) -> list[Stage]:
    try:
        environment = _media_environment(run, results, root, ps_env)
    except (OSError, ValueError) as error:
        results.append(Result("real-media-e2e", "UNVERIFIED", f"真实媒体前置读取失败：{error}"))
        return []
    if environment is None:
        detail = "真实媒体组件、语音/无音轨合成输入及期望文本前置未齐；用例保留。"
        results.append(Result("real-media-e2e", "UNVERIFIED", detail))
        return []
    broker = os.environ.get("JCHTOOLS_TEST_BROKER_EXE") or (ps_env or {}).get("JCHTOOLS_TEST_GUI_EXE")
    if broker is None or not Path(broker).is_absolute() or not Path(broker).is_file():
        detail = "真实媒体测试缺少本轮 JchTools 代理绝对 EXE。"
        results.append(Result("real-media-e2e", "UNVERIFIED", detail))
        return []
    return [
        _nextest_stage(
            root,
            f"real-media-e2e-{case}",
            cargo_args=["-p", "jchtools", "--features", "test-hooks"],
            ignored=True,
            selection=["-E", f"binary(=markdown_media_e2e) and test(={case})"],
            env=environment | {"JCHTOOLS_TEST_BROKER_EXE": broker},
        )
        for case in (
            "real_component_trackless_media_reports_no_audio",
            "real_component_transcribe_returns_structured_markdown",
        )
    ]


def _media_fixture_stage(run: GateRun, root: Path, ps_env: dict[str, str] | None) -> Stage:
    return Stage(
        "media-e2e-fixtures",
        [
            sys.executable,
            str(root / "scripts/media_e2e_fixture.py"),
            "--component-dir",
            str(EXPECTED_TEST_ENGINE),
            "--output",
            str(run.log_dir / "media-e2e-fixtures.json"),
            *(["--offline"] if run.level == "fulltest" else []),
        ],
        env=(ps_env or {}) | {"PYTHONIOENCODING": "utf-8"},
    )


def _media_environment(
    run: GateRun, results: list[Result], root: Path, ps_env: dict[str, str] | None
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
    if not any(result.name == "xberg-engine" and result.status == "PASS" for result in results):
        return None
    output = run.log_dir / "media-e2e-fixtures.json"
    prepared = [result for result in results if result.name == "media-e2e-fixtures"]
    if not prepared:
        prepared = run.run([_media_fixture_stage(run, root, ps_env)])
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


def run_full_coverage(  # noqa: PLR0913 - 门级和测试环境必须独立于可选的打包阶段传入。
    run: GateRun,
    plan: list[Stage],
    results: list[Result],
    snapshot: list[str],
    root: Path,
    *,
    level: str,
    ps_env: dict[str, str],
    scope: ImpactSelection,
) -> None:
    """功能覆盖执行一次；只有 slow tier 在其后执行完整本地打包验证。."""
    _run_compilation(run, plan, results)
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
    command = "from scripts.test_gate import snapshot_lines; print('\\n'.join(snapshot_lines()))"
    preparations = [
        Stage("source-snapshot", [sys.executable, "-c", command], env={"PYTHONIOENCODING": "utf-8"}),
        *(stage for stage in plan if stage.name in {"ocr-asset-manifest", "requirement-reference-inventory"}),
    ]
    if "jchtools" in scope.packages and not any(
        os.environ.get(key)
        for key in (
            "JCHTOOLS_MEDIA_E2E_COMPONENT_DIR",
            "JCHTOOLS_MEDIA_E2E_INPUT",
            "JCHTOOLS_MEDIA_E2E_TRACKLESS_INPUT",
            "JCHTOOLS_MEDIA_E2E_EXPECT_TEXT",
        )
    ):
        preparations.append(_media_fixture_stage(run, root, ps_env))
    _ = run.run(preparations)
    source = next((result for result in results if result.name == "source-snapshot"), None)
    if source is not None and source.log is not None:
        snapshot.extend(source.log.read_text(encoding="utf-8", errors="replace").splitlines())

    # 串行运行 Rust，维持 nextest 跨用例资源组。
    default = scope.packages & scope.default_packages
    rust = [
        *(
            [
                _nextest_stage(
                    root,
                    "root-nextest",
                    cargo_args=[*_package_args(default), *_feature_args(scope, default, "test-hooks")],
                )
            ]
            if default
            else []
        ),
        *(
            _nextest_stage(
                root,
                f"{component}-nextest",
                component=component,
                cargo_args=_feature_args(scope, {component}, "test-hooks"),
            )
            for component in ("snap-ocr-core", "snap-ocr-worker")
            if component in scope.packages
        ),
        *_real_coverage(results, root, scope),
        *(_real_media_stages(run, results, root, ps_env) if "jchtools" in default else []),
        *(stage for stage in plan if stage.name.endswith("-doctests")),
    ]
    _ = run.run(rust)
    if level != "slowtest":
        return
    package = next(stage for stage in plan if stage.name == "package")
    _ = run.run([package])
    if shutil.which("ISCC.exe") is None and not any(
        (Path(os.environ.get(key, "")) / "Inno Setup 6/ISCC.exe").is_file()
        for key in ("PROGRAMFILES", "PROGRAMFILES(X86)")
    ):
        detail = "ISCC 不可用；便携 ZIP 不能代替安装包验证。"
        results.append(Result("installer-environment", "UNVERIFIED", detail))
