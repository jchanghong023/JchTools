"""复用既有检查和测试原语；不改判据，不触发桌面鼠标或远程流水线."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
from pathlib import Path
from typing import TYPE_CHECKING

from scripts.gate_runtime import Result, Stage
from scripts.native_test_tools import affected_environment
from scripts.quality_scope import production_python
from scripts.xberg_test_engine import EXPECTED_TEST_ENGINE

if TYPE_CHECKING:
    from collections.abc import Callable
    from typing import TypeIs

    from scripts.gate_runtime import GateRun

_parse_json: Callable[[str], object] = json.loads

TEST_WORKER_CAP = 28

# 大响应容量边界的真实 CLI 时限不适用于插桩；它们仍由普通 run 每轮执行。
COVERAGE_FILTER = (
    "not test(/^review_remediation_url_(content_length|chunked|without_length)_budget_plus_one_is_network_error$/)"
)


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
            [cargo, "test", "--doc", "--features", "test-hooks", "--no-fail-fast", "--", "--quiet"],
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
                [
                    cargo,
                    "test",
                    "--manifest-path",
                    manifest,
                    "--doc",
                    "--features",
                    "test-hooks",
                    "--no-fail-fast",
                    "--",
                    "--quiet",
                ],
                "compiler-observed",
            ),
        ]
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
        argv += ["--config-file", str(root / ".config/nextest.toml")]
    else:
        argv += ["affected", "run", "--report-json", str(incremental_report), "--"]
        env = (env or {}) | affected_environment()
    argv += [
        # cargo-affected 为选中的测试生成并合并根配置，不能再传第二个配置文件。
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
    argv += ["--all-targets", "--features", "test-hooks"]
    if ignored:
        argv += ["--run-ignored", "only"]
    argv += selection or []
    return Stage(name, argv, env=env)


def _coverage_selection_summary(payload: object, status: str) -> dict[str, object]:
    selection = payload.get("selection") if _mapping(payload) else None
    summary = selection.get("summary") if _mapping(selection) else None
    if not _mapping(summary):
        message = "cargo-affected 状态报告缺少有效 selection.summary。"
        raise TypeError(message)
    hit = status.startswith("hit-")
    expected_mode = "selection" if hit else "full-suite-no-listing"
    if summary.get("mode") != expected_mode:
        message = f"cargo-affected 状态报告 selection.summary.mode 与缓存状态不符：{status}"
        raise ValueError(message)
    counts = ("selected", "affected", "config", "new", "stranded", "skipped", "total_reachable_known")
    for name in counts:
        value = summary.get(name)
        if (
            name not in summary
            or (hit and (not isinstance(value, int) or isinstance(value, bool) or value < 0))
            or (not hit and value is not None)
        ):
            message = f"cargo-affected 状态报告 selection.summary.{name} 无效。"
            raise ValueError(message)
    return summary


def prepare_rust_baseline(run: GateRun, root: Path) -> None:
    """维护原生 coverage 基线；未收录或不宜插桩的用例仍由普通 run 执行."""
    cargo = shutil.which("cargo") or "cargo"
    report = run.log_dir / "rust-coverage-status.json"
    native_env = affected_environment()
    status_results = run.run(
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
                    "--config-file",
                    str(root / ".config/nextest.toml"),
                    "-E",
                    COVERAGE_FILTER,
                ],
                env=native_env,
            )
        ]
    )
    if any(result.status != "PASS" for result in status_results):
        return
    payload = _parse_json(report.read_text(encoding="utf-8"))
    cache = payload.get("cache") if _mapping(payload) else None
    status = cache.get("status") if _mapping(cache) else None
    if not isinstance(status, str):
        message = "cargo-affected 状态报告缺少有效 cache.status。"
        raise TypeError(message)
    if status not in {
        "hit-exact",
        "hit-with-divergence",
        "miss-fingerprint",
        "miss-no-coverage",
        "miss-no-reachable-sha",
    }:
        message = f"cargo-affected 返回未知缓存状态：{status}"
        raise ValueError(message)
    summary = _coverage_selection_summary(payload, status)
    hit = status.startswith("hit-")
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
        print("工作树未提交；保留原生选测或全量回退，不使用 --allow-dirty 采集基线。")
        return
    if hit and summary["selected"] == 0:
        return
    worker_build = run.run(
        [
            Stage(
                "rust-coverage-worker-build",
                [
                    cargo,
                    "build",
                    "--manifest-path",
                    str(root / "optional" / "snap-ocr-worker" / "Cargo.toml"),
                    "--bin",
                    "snap-ocr-worker",
                    "--features",
                    "test-hooks",
                    "--target-dir",
                    str(root / "target" / "affected" / "build"),
                ],
            )
        ]
    )
    if any(result.status != "PASS" for result in worker_build):
        return
    native = _nextest_stage(root, "rust-coverage-baseline")
    options = native.argv[3:]
    # collect 自带零测试选项及无覆盖结果校验，重复传参会被 Nextest 拒绝。
    empty_option = options.index("--no-tests")
    del options[empty_option : empty_option + 2]
    collect = [native.argv[0], "affected", "collect"]
    if status.startswith("hit-"):
        # diff collect 自行创建并合并根配置；重复指定会被 nextest 拒绝。
        config_option = options.index("--config-file")
        del options[config_option : config_option + 2]
        collect.append("--diff")
    collect += ["--", *options, "--workspace", "-E", COVERAGE_FILTER]
    _ = run.run([Stage("rust-coverage-baseline", collect, env=native_env)])


def _run_compilation(run: GateRun, plan: list[Stage], results: list[Result]) -> None:
    completed = {result.name for result in results}
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
    stages: list[Stage] = []
    engine = os.environ.get("JCHTOOLS_TEST_7ZIP") or str(root / "resources/7zip/7z.exe")
    if not Path(engine).is_file():
        results.append(Result("real-engine-tests", "UNVERIFIED", "缺少真实 7-Zip 引擎。"))
    else:
        stages.extend(
            [
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
            ]
        )
    if not any(result.name == "xberg-latest" and result.status == "PASS" for result in results):
        results.append(Result("real-xberg-presence", "UNVERIFIED", "最新真实 Xberg 前置未通过。"))
    else:
        stages.append(
            _nextest_stage(
                root,
                "real-xberg-presence",
                ignored=True,
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
    _ = run.run(preparations)
    source = next((result for result in results if result.name == "source-snapshot"), None)
    if source is not None and source.log is not None:
        snapshot.extend(source.log.read_text(encoding="utf-8", errors="replace").splitlines())

    # 串行运行 Rust，维持 nextest 跨用例资源组。
    incremental = not full
    if incremental:
        try:
            _ = affected_environment()
        except RuntimeError as error:
            results.append(Result("native-test-tools", "FAIL", str(error)))
            incremental = False

    def report(name: str) -> Path | None:
        return run.log_dir / f"{name}-affected.json" if incremental else None

    if incremental:
        try:
            prepare_rust_baseline(run, root)
        except (OSError, ValueError, TypeError, RuntimeError, subprocess.CalledProcessError) as error:
            results.append(Result("rust-coverage-preparation", "FAIL", str(error)))
    rust = [
        _nextest_stage(root, "root-nextest", incremental_report=report("root-nextest")),
        *(
            _nextest_stage(
                root, f"{component}-nextest", component=component, incremental_report=report(f"{component}-nextest")
            )
            for component in ("snap-ocr-core", "snap-ocr-worker")
        ),
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
