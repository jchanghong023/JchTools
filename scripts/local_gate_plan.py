"""复用既有检查和测试原语；不改判据，不触发桌面鼠标或远程流水线."""

from __future__ import annotations

import json
import os
import shutil
import sys
from pathlib import Path
from typing import TYPE_CHECKING

from scripts.gate_runtime import GateRun, Result, Stage
from scripts.xberg_test_engine import EXPECTED_TEST_ENGINE

if TYPE_CHECKING:
    from collections.abc import Callable
    from typing import TypeIs

_parse_json: Callable[[str], object] = json.loads


def _mapping(value: object) -> TypeIs[dict[str, object]]:
    return isinstance(value, dict)


def static_plan(root: Path) -> list[Stage]:
    """原有 Python / Rust 静态判据，不执行 unittest."""
    cargo = shutil.which("cargo") or "cargo"
    return [
        Stage("static-check", [sys.executable, str(root / "scripts/static_check.py")]),
        Stage("rustfmt-check", [cargo, "fmt", "--all", "--", "--check"]),
        Stage("pyquality-ruff-format", [sys.executable, "-m", "ruff", "format", "--check", "scripts", "typings"]),
        Stage("pyquality-ruff-lint", [sys.executable, "-m", "ruff", "check", "scripts", "typings"]),
        Stage("pyquality-basedpyright", [sys.executable, "-m", "basedpyright", "scripts", "typings"]),
        Stage("pyquality-vulture", [sys.executable, "-m", "vulture", "scripts", "typings", "--min-confidence", "100"]),
        Stage("pyquality-bandit", [sys.executable, "-m", "bandit", "-r", "scripts", "-s", "B404,B603", "-q"]),
    ]


def gate_plan(level: str, root: Path, log_dir: Path, ps_env: dict[str, str]) -> list[Stage]:
    """声明包含关系；运行时处理共享 Cargo 缓存和测试 artifact 的依赖."""
    cargo = shutil.which("cargo") or "cargo"
    plan = [
        *static_plan(root),
        Stage("production-build", [cargo, "build", "--bin", "JchTools"], "compile"),
        Stage("test-target-check", [cargo, "check", "--all-targets", "--features", "test-hooks"], "compile"),
        Stage(
            "clippy",
            [cargo, "clippy", "--all-targets", "--features", "test-hooks", "--", "-D", "warnings"],
            "compiler-observed",
        ),
    ]
    if level == "fastcheck":
        return plan
    plan += [
        Stage("pyquality-pip-audit", [sys.executable, "-m", "pip_audit", "-r", "scripts/requirements-dev.txt"]),
        Stage("markdown-acceptance-unit", [sys.executable, "-m", "unittest", "scripts.test_markdown_acceptance"]),
        Stage("gui-automation-unit", [sys.executable, "-m", "unittest", "scripts.test_gui_smoke"]),
        Stage("test-gate-unit", [sys.executable, "-m", "unittest", "scripts.test_test_gate"]),
        Stage("ocr-asset-manifest", [sys.executable, "tests/ocr_fixtures/check_asset_manifest.py"]),
        Stage("requirement-reference-inventory", [sys.executable, "scripts/requirement_coverage.py"]),
        Stage("xberg-latest", [sys.executable, "scripts/xberg_test_engine.py"]),
        Stage(
            "clippy-perf-tracing",
            [cargo, "clippy", "--all-targets", "--features", "perf-tracing,test-hooks", "--", "-D", "warnings"],
            "compiler-observed",
        ),
        Stage(
            "root-test-build",
            [cargo, "test", "--all-targets", "--features", "test-hooks", "--no-run", "--message-format=json"],
            "compile",
        ),
        Stage("root-doctests", [cargo, "test", "--doc", "--features", "test-hooks"], "compiler-observed"),
    ]
    for component in ("snap-ocr-core", "snap-ocr-worker"):
        manifest = f"optional/{component}/Cargo.toml"
        plan += [
            Stage(
                f"{component}-clippy",
                [
                    cargo,
                    "clippy",
                    "--manifest-path",
                    manifest,
                    "--all-targets",
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
                [cargo, "test", "--manifest-path", manifest, "--doc", "--features", "test-hooks"],
                "compiler-observed",
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
    for line in build.log.read_text(encoding="utf-8", errors="replace").splitlines():
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
            stages.append(Stage(f"{build.name}-{name}", argv))
    return stages


def _real_test_stage(
    results: list[Result],
    binaries: list[Stage],
    target: tuple[str, str],
    env: dict[str, str] | None = None,
) -> Stage | None:
    identifier, name = target
    binary = next((stage for stage in binaries if stage.name == f"root-test-build-{identifier}"), None)
    if binary is None:
        results.append(Result(name, "UNVERIFIED", "本轮编译未发布所需测试 artifact。"))
        return None
    argv = [binary.argv[0], "--ignored", "--nocapture", "--test-threads=1"]
    return Stage(name, argv, env=env)


def _compile_test_artifacts(run: GateRun, plan: list[Stage], results: list[Result]) -> list[Stage]:
    for stage in plan:
        if stage.name == "clippy-perf-tracing" or stage.name.endswith("-clippy"):
            results.extend(run.run([stage]))
    binaries: list[Stage] = []
    for stage in plan:
        if not stage.name.endswith("-test-build"):
            continue
        built = run.run([stage])
        results.extend(built)
        for result in built:
            if result.status != "PASS":
                continue
            found = _test_binary_stages(result)
            if not found:
                detail = "Cargo 未发布任何本轮测试 artifact，不能冒充已测试。"
                results.append(Result(f"{stage.name}-artifacts", "FAIL", detail))
            binaries.extend(found)
    return binaries


def _real_coverage(
    run: GateRun, results: list[Result], binaries: list[Stage], root: Path, ps_env: dict[str, str] | None
) -> list[Stage]:
    stages: list[Stage] = []
    engine = os.environ.get("JCHTOOLS_TEST_7ZIP") or str(root / "resources/7zip/7z.exe")
    if Path(engine).is_file():
        stage = _real_test_stage(results, binaries, ("archive", "real-engine-tests"), {"JCHTOOLS_TEST_7ZIP": engine})
        if stage is not None:
            if Path(engine).resolve() == (root / "resources/7zip/7z.exe").resolve():
                stage.exclusive_resource = "bundled-7zip"
            stages.append(stage)
    else:
        results.append(Result("real-engine-tests", "UNVERIFIED", "缺少真实 7-Zip 引擎。"))
    if any(result.name == "xberg-latest" and result.status == "PASS" for result in results):
        stage = _real_test_stage(
            results,
            binaries,
            ("xberg_assets", "real-xberg-presence"),
            {"JCHTOOLS_REAL_XBERG_DIR": str(EXPECTED_TEST_ENGINE)},
        )
        if stage is not None:
            stages.append(stage)
    else:
        results.append(Result("real-xberg-presence", "UNVERIFIED", "最新真实 Xberg 前置未通过。"))
    media = _real_media_stage(run, results, binaries, root, ps_env)
    if media is not None:
        # 两个用例各自保存隔离状态目录；逐个 Job 收尾后才能启动下一套共享代理。
        stages.extend(
            Stage(
                f"{media.name}-{case}",
                [*media.argv, "--exact", case],
                env=media.env,
                exclusive_resource="shared-xberg-media",
            )
            for case in (
                "real_component_trackless_media_reports_no_audio",
                "real_component_transcribe_returns_structured_markdown",
            )
        )
    return stages


def _real_media_stage(
    run: GateRun, results: list[Result], binaries: list[Stage], root: Path, ps_env: dict[str, str] | None
) -> Stage | None:
    environment = _media_environment(run, results, root, ps_env)
    if environment is None:
        detail = "真实媒体组件、语音/无音轨合成输入及期望文本前置未齐；用例保留。"
        results.append(Result("real-media-e2e", "UNVERIFIED", detail))
        return None
    broker = os.environ.get("JCHTOOLS_TEST_BROKER_EXE") or (ps_env or {}).get("JCHTOOLS_TEST_GUI_EXE")
    if broker is None or not Path(broker).is_absolute() or not Path(broker).is_file():
        results.append(Result("real-media-e2e", "UNVERIFIED", "真实媒体测试缺少本轮 JchTools 代理绝对 EXE。"))
        return None
    return _real_test_stage(
        results,
        binaries,
        ("markdown_media_e2e", "real-media-e2e"),
        environment | {"JCHTOOLS_TEST_BROKER_EXE": broker},
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
    prepared = run.run(
        [
            Stage(
                "media-e2e-fixtures",
                [
                    sys.executable,
                    str(root / "scripts/media_e2e_fixture.py"),
                    "--component-dir",
                    str(EXPECTED_TEST_ENGINE),
                    "--output",
                    str(output),
                ],
                env=(ps_env or {}) | {"PYTHONIOENCODING": "utf-8"},
            )
        ]
    )
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


def run_full_coverage(run: GateRun, plan: list[Stage], results: list[Result], snapshot: list[str], root: Path) -> None:
    """编译与测试运行分离；四个隔离 binary 持续补位，每个最多两个测试线程."""
    command = "from scripts.test_gate import snapshot_lines; print('\\n'.join(snapshot_lines()))"
    got = run.run([Stage("source-snapshot", [sys.executable, "-c", command], env={"PYTHONIOENCODING": "utf-8"})])
    results.extend(got)
    if got and got[0].log is not None:
        snapshot.extend(got[0].log.read_text(encoding="utf-8", errors="replace").splitlines())
    names = {"pyquality-pip-audit", "ocr-asset-manifest", "requirement-reference-inventory", "xberg-latest"}
    results.extend(run.run([stage for stage in plan if stage.name in names]))
    binaries = _compile_test_artifacts(run, plan, results)
    pending = [*(stage for stage in plan if stage.name.endswith("-unit")), *binaries]
    results.append(
        Result(
            "real-opencode",
            "NOT_RUN_SEPARATE_USER_INSTRUCTION_REQUIRED",
            "真实模型验收保留独立 acp_opencode 入口；门授权或 JCHTOOLS_TEST_OPENCODE_EXE 配置不授权模型调用。",
        )
    )
    results.extend(run.run(pending))
    results.append(
        Result(
            "desktop-tests",
            "NOT_RUN_SEPARATE_USER_INSTRUCTION_REQUIRED",
            "gui_flow、ACP real_gui、桌面 driver 和 worker-root 保留独立入口：软件渲染不隔离焦点，热键也需隔离。",
        )
    )
    for stage in plan:
        if stage.name.endswith("-doctests"):
            results.extend(run.run([stage]))
    package = next(stage for stage in plan if stage.name == "package")
    pending = _real_coverage(run, results, binaries, root, package.env)
    pending.append(package)
    results.extend(run.run(pending))
    if shutil.which("ISCC.exe") is None and not any(
        (Path(os.environ.get(key, "")) / "Inno Setup 6/ISCC.exe").is_file()
        for key in ("PROGRAMFILES", "PROGRAMFILES(X86)")
    ):
        results.append(Result("installer-environment", "UNVERIFIED", "ISCC 不可用；便携 ZIP 不能代替安装包验证。"))
