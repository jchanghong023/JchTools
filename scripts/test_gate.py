#!/usr/bin/env python3
"""JchTools 本地验证门：fastcheck、fulltest、slowtest."""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
import traceback
from pathlib import Path

if not __package__:
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from scripts.gate_runtime import GateRun, Result, Stage
from scripts.gate_summary import STATUS_FAILED, STATUS_UNVERIFIED, ps51_module_path, reconfigure_stdout, version_line
from scripts.local_gate_plan import gate_plan, run_full_coverage
from scripts.make_tmp_impl.guard import guard_destination

ROOT = Path(__file__).resolve().parent.parent
LOG_DIR = ROOT / ".tmp" / "test-gate"
FIXED_XBERG_TEST_DIR = Path(r"C:\Users\jiang\Documents\xberg-test\xberg-cli-x86_64-pc-windows-msvc")
SNAPSHOT_DIRTY_SAMPLE = 10


class _Arguments(argparse.Namespace):
    gate: str = ""
    authorized: bool = False
    full: bool = False


def _gate_plan(level: str, log_dir: Path) -> list[Stage]:
    return gate_plan(level, ROOT, log_dir, _powershell_env())


def _execute_plan(run: GateRun, level: str, plan: list[Stage], snapshot: list[str], *, full: bool = False) -> None:
    common = [
        stage
        for stage in plan
        if stage.name in {"production-build", "test-target-check", "clippy", "xberg-latest"}
        or (stage.kind in {"compile", "compiler-observed"} and not stage.name.endswith("-doctests"))
        or stage.name.startswith(("static-", "rustfmt-", "pyquality-"))
    ]
    static = [stage for stage in common if stage.name.startswith(("static-", "rustfmt-", "pyquality-"))]
    _ = run.run(static)
    _ = run.run([stage for stage in common if stage not in static])
    if level != "fastcheck":
        run_full_coverage(run, plan, run.results, snapshot, ROOT, level=level, ps_env=_powershell_env(), full=full)


def execute_gate(level: str, *, stages_override: list[Stage] | None = None, full: bool = False) -> int:
    run = GateRun(level, ROOT, LOG_DIR / level)
    snapshot: list[str] = []
    notes: list[str] = []
    try:
        if stages_override is not None:
            _ = run.run(stages_override)
        elif sys.platform != "win32":
            run.results.append(Result("platform", STATUS_UNVERIFIED, "合同 P-07 仅支持 Windows。"))
        else:
            guard_destination(run.log_dir)
            run.prepare_temporary()
            plan = _gate_plan(level, run.log_dir)
            _execute_plan(run, level, plan, snapshot, full=full)
            notes.append("NOT_RUN_SEPARATE_USER_INSTRUCTION_REQUIRED：gui_smoke、Markdown 真实桌面验收、OCR 桌面探针。")
            notes.append("CI / 远程流水线 / 发布 / Computer Use：在三个门之外。")
    except KeyboardInterrupt:
        run.results.append(Result("interruption", STATUS_FAILED, "用户中断；不报告为通过。"))
    except Exception as error:  # noqa: BLE001 - 入口须对启动或编排错误非零退出。
        run.results.append(Result("orchestration", STATUS_FAILED, f"{type(error).__name__}: {error}"))
        traceback.print_exc()
    return run.finish(snapshot=snapshot, notes=notes)


def _powershell_env() -> dict[str, str]:
    """只为子进程配置 PowerShell 5.1 和测试程序路径."""
    env = {"PSExecutionPolicyPreference": "Bypass"}
    module_path = ps51_module_path()
    if module_path is not None:
        env["PSModulePath"] = module_path
    explicit_xberg = os.environ.get("JCHTOOLS_TEST_XBERG_DIR")
    if explicit_xberg:
        env["JCHTOOLS_TEST_XBERG_DIR"] = explicit_xberg
    elif (FIXED_XBERG_TEST_DIR / "xberg.exe").is_file():
        env["JCHTOOLS_TEST_XBERG_DIR"] = str(FIXED_XBERG_TEST_DIR)
    explicit_gui = os.environ.get("JCHTOOLS_TEST_GUI_EXE")
    if explicit_gui:
        env["JCHTOOLS_TEST_GUI_EXE"] = explicit_gui
    else:
        target_raw = os.environ.get("CARGO_TARGET_DIR")
        target_dir = Path(target_raw) if target_raw else ROOT / "target"
        if not target_dir.is_absolute():
            target_dir = ROOT / target_dir
        env["JCHTOOLS_TEST_GUI_EXE"] = str(target_dir.resolve() / "debug" / "JchTools.exe")
    return env


def _git_output(git: str, args: list[str]) -> str | None:
    done = subprocess.run([git, *args], cwd=ROOT, capture_output=True, text=True, encoding="utf-8", check=False)
    if done.returncode != 0:
        return None
    return done.stdout.strip()


def snapshot_lines() -> list[str]:
    """记录源码状态及工具版本，作为本次结果来源证据."""
    git = shutil.which("git")
    head = _git_output(git, ["rev-parse", "HEAD"]) if git is not None else None
    lines = [f"HEAD: {head or '未知（git 不可用）'}"]
    if git is not None:
        porcelain = _git_output(git, ["status", "--porcelain"])
        if porcelain is None:
            lines.append("工作区：状态查询失败")
        elif not porcelain:
            lines.append("工作区：clean（无未提交改动）")
        else:
            changed = porcelain.splitlines()
            sample = "；".join(changed[:SNAPSHOT_DIRTY_SAMPLE])
            rest = len(changed) - SNAPSHOT_DIRTY_SAMPLE
            more = f"；另有 {rest} 项" if rest > 0 else ""
            lines.append(f"工作区：dirty（{len(changed)} 项未提交改动）：{sample}{more}")
    lines.append(version_line("cargo"))
    lines.append(version_line("rustc"))
    lines.append(f"python: {sys.version.split()[0]}（{sys.executable}）")
    for key in ("CARGO_TARGET_DIR", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"):
        value = os.environ.get(key)
        if value:
            lines.append(f"{key}: {value}")
    return lines


def main(argv: list[str] | None = None) -> int:
    reconfigure_stdout()
    parser = argparse.ArgumentParser(description="JchTools 本地三级门；无 CI、鼠标或发布")
    sub = parser.add_subparsers(dest="gate", required=True)
    for name in ("fastcheck", "fulltest", "slowtest"):
        command = sub.add_parser(name)
        _ = command.add_argument("--full", action="store_true", help="禁用增量选测，运行全量覆盖")
        if name != "fastcheck":
            _ = command.add_argument("--authorized", action="store_true", help="代表当次明确授权")
    arguments = parser.parse_args(argv, namespace=_Arguments())
    if arguments.gate != "fastcheck" and not arguments.authorized:
        run = GateRun(arguments.gate, ROOT, LOG_DIR / arguments.gate)
        run.results.append(Result("authorization", STATUS_UNVERIFIED, "须当次明确授权，并使用 --authorized。"))
        return run.finish()
    return execute_gate(arguments.gate, full=arguments.full)


if __name__ == "__main__":
    sys.exit(main())
