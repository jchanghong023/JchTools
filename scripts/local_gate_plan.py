"""复用既有检查和测试原语；不改判据，不触发桌面鼠标或远程流水线."""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path
from typing import TYPE_CHECKING

from scripts.gate_runtime import Result, Stage
from scripts.quality_scope import production_python
from scripts.xberg_test_engine import EXPECTED_TEST_ENGINE

if TYPE_CHECKING:
    from collections.abc import Callable
    from typing import TypeIs

    from scripts.gate_runtime import GateRun

_parse_json: Callable[[str], object] = json.loads

TEST_WORKER_CAP = 15


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


def gate_plan(level: str, root: Path, log_dir: Path, ps_env: dict[str, str], *, full: bool = False) -> list[Stage]:
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
        _python_stage(root, log_dir, "markdown-acceptance-unit", "scripts/test_markdown_acceptance.py", full=True),
        _python_stage(root, log_dir, "gui-automation-unit", "scripts/test_gui_smoke.py", full=True),
        _python_stage(
            root,
            log_dir,
            "test-gate-unit",
            "scripts/test_test_gate.py",
            full=True,
        ),
        _python_stage(
            root,
            log_dir,
            "test-gate-pytest-unit",
            "scripts/test_pytest_gate.py",
            full=True,
        ),
        _python_stage(root, log_dir, "trusted-review-unit", "scripts/test_review_trusted.py", full=True),
        _python_stage(root, log_dir, "worker-root-isolation-unit", "tests/ocr_fixtures", full=full),
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
            ),
        ]
    if level != "slowtest":
        return plan
    plan += [
        # 全量在线供应链审计依赖外部索引响应，不属于快速功能回归。
        Stage("pyquality-pip-audit", [sys.executable, "-m", "pip_audit", "-r", "scripts/requirements-dev.txt"]),
        Stage("release-target-build", [cargo, "build", "--all-targets", "--release"], "compile"),
        _python_stage(
            root,
            log_dir,
            "package-windows-unit",
            "scripts/test_package_windows.py",
            full=True,
        ),
        _python_stage(root, log_dir, "fetch-7zip-unit", "scripts/test_fetch_7zip.py", full=True),
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


def _python_stage(
    root: Path,
    log_dir: Path,
    name: str,
    suite: str,
    *,
    full: bool,
) -> Stage:
    env: dict[str, str] = {}
    mode = ["--no-testmon"]
    if not full:
        # 此套件仅有一个未模拟的外部输入：共享 SQL schema。
        schema = root / "crates/foundation/src/app_settings.sql"
        digest = hashlib.sha256(schema.read_bytes()).hexdigest() if schema.is_file() else "missing"
        env["TESTMON_DATAFILE"] = str(root / ".tmp/testmon" / f"{name}.testmondata")
        mode = ["--testmon", "--testmon-env", f"{name}-{digest}"]
    return Stage(
        name,
        [
            sys.executable,
            "-m",
            "pytest",
            "-p",
            "scripts.pytest_gate",
            "-x",
            "-n",
            str(test_worker_count()),
            "--dist",
            "loadscope",
            "--max-worker-restart",
            "0",
            *mode,
            "--jchtools-report-dir",
            str(log_dir / "python-reports" / name),
            suite,
        ],
        env=env,
    )


def _nextest_stage(  # noqa: PLR0913 - 组件、筛选、忽略测试、环境及增量报告分别控制原生测试参数。
    root: Path,
    name: str,
    *,
    component: str | None = None,
    selection: list[str] | None = None,
    ignored: bool = False,
    env: dict[str, str] | None = None,
    incremental_report: Path | None = None,
) -> Stage:
    cargo = shutil.which("cargo") or "cargo"
    argv = [cargo]
    if incremental_report is None:
        argv += ["nextest", "run"]
    else:
        argv += ["affected", "run", "--report-json", str(incremental_report), "--"]
    argv += [
        "--config-file",
        str(root / ".config/nextest.toml"),
        "--test-threads",
        str(test_worker_count()),
        "--retries",
        "0",
        "--max-fail=1:immediate",
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
    argv += ["--all-targets", "--features", "test-hooks"]
    if ignored:
        argv += ["--run-ignored", "only"]
    argv += selection or []
    return Stage(name, argv, env=env)


def prepare_rust_baseline(run: GateRun, root: Path) -> bool:
    """在干净工作树上由原生工具建立缺失基线；返回本轮是否已执行普通 Rust 测试."""
    cargo = shutil.which("cargo") or "cargo"
    report = run.log_dir / "rust-coverage-status.json"
    _ = run.run(
        [
            Stage(
                "rust-coverage-status",
                [
                    cargo,
                    "affected",
                    "status",
                    "--report-json",
                    str(report),
                    "--",
                    "--workspace",
                    "--all-targets",
                    "--features",
                    "test-hooks",
                ],
            )
        ]
    )
    payload = _parse_json(report.read_text(encoding="utf-8"))
    cache = payload.get("cache") if _mapping(payload) else None
    status = cache.get("status") if _mapping(cache) else None
    if not isinstance(status, str):
        message = "cargo-affected 状态报告缺少有效 cache.status。"
        raise TypeError(message)
    if not status.startswith("miss-"):
        return False
    git = shutil.which("git") or "git"
    working_tree = subprocess.run(
        [git, "status", "--porcelain", "--untracked-files=normal"],
        cwd=root,
        capture_output=True,
        text=True,
        encoding="utf-8",
        check=True,
    )
    if working_tree.stdout:
        print("Rust coverage 基线不可用且工作树未提交；保守全量运行，不使用 --allow-dirty。")
        return False
    native = _nextest_stage(root, "rust-coverage-baseline")
    options = native.argv[3:]
    # collect 自带零测试选项及无覆盖结果校验，重复传参会被 Nextest 拒绝。
    empty_option = options.index("--no-tests")
    del options[empty_option : empty_option + 2]
    collect = [native.argv[0], "affected", "collect", "--", *options, "--workspace"]
    _ = run.run([Stage("rust-coverage-baseline", collect)])
    return True


def _run_compilation(run: GateRun, plan: list[Stage], results: list[Result]) -> None:
    completed = {result.name for result in results if result.status == "PASS"}
    selected = [
        stage
        for stage in plan
        if (stage.kind in {"compile", "compiler-observed"} or stage.name == "xberg-latest")
        and stage.name not in completed
        and not stage.name.endswith("-doctests")
    ]
    if selected:
        _ = run.run(selected)


def _real_coverage(results: list[Result], root: Path) -> list[Stage]:
    engine = os.environ.get("JCHTOOLS_TEST_7ZIP") or str(root / "resources/7zip/7z.exe")
    if not Path(engine).is_file():
        detail = "缺少真实 7-Zip 引擎。"
        results.append(Result("real-engine-tests", "UNVERIFIED", detail))
        raise FileNotFoundError(detail)
    if not any(result.name == "xberg-latest" and result.status == "PASS" for result in results):
        detail = "最新真实 Xberg 前置未通过。"
        results.append(Result("real-xberg-presence", "UNVERIFIED", detail))
        raise RuntimeError(detail)
    return [
        _nextest_stage(
            root,
            "real-engine-tests",
            ignored=True,
            selection=["-E", "binary(=archive)"],
            env={"JCHTOOLS_TEST_7ZIP": engine},
        ),
        _nextest_stage(
            root,
            "real-engine-lib-tests",
            ignored=True,
            selection=["-E", "package(=jchtools-recursive-extract) and kind(lib)"],
            env={"JCHTOOLS_TEST_7ZIP": engine},
        ),
        _nextest_stage(
            root,
            "real-xberg-presence",
            ignored=True,
            selection=["-E", "binary(=xberg_assets) and test(=real_xberg_dir_passes_presence_checks)"],
            env={"JCHTOOLS_REAL_XBERG_DIR": str(EXPECTED_TEST_ENGINE)},
        ),
    ]


def _real_media_stages(run: GateRun, results: list[Result], root: Path, ps_env: dict[str, str] | None) -> list[Stage]:
    environment = _media_environment(run, results, root, ps_env)
    if environment is None:
        detail = "真实媒体组件、语音/无音轨合成输入及期望文本前置未齐；用例保留。"
        results.append(Result("real-media-e2e", "UNVERIFIED", detail))
        raise RuntimeError(detail)
    broker = os.environ.get("JCHTOOLS_TEST_BROKER_EXE") or (ps_env or {}).get("JCHTOOLS_TEST_GUI_EXE")
    if broker is None or not Path(broker).is_absolute() or not Path(broker).is_file():
        detail = "真实媒体测试缺少本轮 JchTools 代理绝对 EXE。"
        results.append(Result("real-media-e2e", "UNVERIFIED", detail))
        raise FileNotFoundError(detail)
    return [
        _nextest_stage(
            root,
            f"real-media-e2e-{case}",
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
    if not any(result.name == "xberg-latest" and result.status == "PASS" for result in results):
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


def run_priority_units(run: GateRun, plan: list[Stage]) -> None:
    """先运行高风险单元测试，成功后才进入普通覆盖。."""
    priority = [
        stage for stage in plan if stage.name.endswith("-unit") and stage.name.startswith(("trusted-", "test-gate-"))
    ]
    if priority:
        _ = run.run(priority)


def run_full_coverage(  # noqa: PLR0913 - 门级和测试环境必须独立于可选的打包阶段传入。
    run: GateRun,
    plan: list[Stage],
    results: list[Result],
    snapshot: list[str],
    root: Path,
    *,
    level: str,
    ps_env: dict[str, str],
    full: bool = False,
) -> None:
    """功能覆盖执行一次；只有 slow tier 在其后执行完整本地打包验证。."""
    _run_compilation(run, plan, results)
    completed = {result.name for result in results if result.status == "PASS"}
    pending = [stage for stage in plan if stage.name.endswith("-unit") and stage.name not in completed]
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
    _ = run.run([*preparations, *pending])
    source = next((result for result in results if result.name == "source-snapshot"), None)
    if source is not None and source.log is not None:
        snapshot.extend(source.log.read_text(encoding="utf-8", errors="replace").splitlines())

    # Python 全部结束后再串行运行 Rust，维持 nextest 跨用例资源组。
    def report(name: str) -> Path | None:
        return None if full else run.log_dir / f"{name}-affected.json"

    ordinary: list[Stage] = []
    if full or not prepare_rust_baseline(run, root):
        ordinary = [
            _nextest_stage(root, "root-nextest", incremental_report=report("root-nextest")),
            *(
                _nextest_stage(
                    root, f"{component}-nextest", component=component, incremental_report=report(f"{component}-nextest")
                )
                for component in ("snap-ocr-core", "snap-ocr-worker")
            ),
        ]
    rust = [
        *ordinary,
        *_real_coverage(results, root),
        *_real_media_stages(run, results, root, ps_env),
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
        raise FileNotFoundError(detail)
