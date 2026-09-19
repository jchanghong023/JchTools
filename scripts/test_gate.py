#!/usr/bin/env python3
"""JchTools 三级测试门：fastcheck / fulltest / slowtest.

层级语义（固定，不随项目需要改写；权限边界防止 AI 代理自行升级验证范围）：

  fastcheck   AI 可自主执行的高频快速反馈；总墙钟硬上限 60 秒，超时即失败并终止
              整个进程树，不得把超时报成成功。通过不代表完整验证。
              步骤：static_check → rustfmt → clippy（默认特性）→ cargo test（含
              binding loop 警告扫描）。
  fulltest    当前平台（Windows）全部适用本地检查：Python 质量门（与 CI 同命令）
              + rustfmt + clippy + make_tmp 测试数据集 + acceptance.ps1
              -WithEngine -WithGuiSmoke -WithPackage（复用可信基验收入口，内含
              static_check、全量测试、真实引擎用例、GUI 冒烟 S1-S4、打包自检）。
              不触发远程流水线；每次运行都需要人类明确授权（--authorized）。
  slowtest    fulltest 全部阶段 + 远程 CI（check.yml：gh 触发后轮询到最终
              状态，TRIGGERED 不等于 PASS）。平台范围按合同 P-07 仅
              Windows，不设跨平台/跨 WSL 阶段。同样需要人类本次明确授权。
              release.yml 是真实发布（自动打时间戳 tag 并发布产物），不属于
              slowtest，只能单独显式授权手动触发。

防递归：被触发的远程工作流各自运行固定步骤，不会回调本脚本，不存在
slowtest → CI → slowtest 循环。

用法：
    python scripts/test_gate.py fastcheck
    python scripts/test_gate.py fastcheck --deadline-seconds 3   # 仅允许调低，用于验证超时路径
    python scripts/test_gate.py fulltest --authorized
    python scripts/test_gate.py slowtest --authorized
"""

from __future__ import annotations

import argparse
import contextlib
import dataclasses
import importlib.util
import io
import json
import os
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from collections.abc import Callable
    from typing import TypeIs

ROOT = Path(__file__).resolve().parent.parent
LOG_DIR = ROOT / ".tmp" / "test-gate"
GUI_DATA_DIR = LOG_DIR / "gui-data"

FASTCHECK_DEADLINE_SECONDS = 60.0
STAGE_TIMEOUT_DEFAULT = 3600.0
# 远程工作流等待上限：check.yml 约 30-40 分钟。
REMOTE_CHECK_WATCH_SECONDS = 5400.0
REMOTE_POLL_INTERVAL_SECONDS = 30.0

# 常量名避开 pass 字样（S105 会把含 pass 的变量名当作疑似硬编码口令）。
STATUS_OK = "PASS"
STATUS_FAILED = "FAIL"
STATUS_TIMED_OUT = "TIMEOUT"
STATUS_UNVERIFIED = "UNVERIFIED"
STATUS_NOT_RUN = "NOT RUN"

# json.loads 的返回含 Any；经固定签名别名收口为 object，再用 TypeIs 守卫逐层收窄。
_parse_json: Callable[[str], object] = json.loads


def _is_str_obj_map(value: object) -> TypeIs[dict[str, object]]:
    return isinstance(value, dict)


def _is_obj_list(value: object) -> TypeIs[list[object]]:
    return isinstance(value, list)


@dataclasses.dataclass
class StageResult:
    """单个阶段的执行结果；log 为该阶段完整输出落盘位置（无则 None）."""

    name: str
    status: str
    detail: str
    log: Path | None = None


class _Arguments(argparse.Namespace):
    """带类型标注的解析结果；子命令未提供的开关保留安全默认值."""

    gate: str = ""
    deadline_seconds: float = FASTCHECK_DEADLINE_SECONDS
    authorized: bool = False


def _reconfigure_stdout() -> None:
    # 阶段明细含中文；Windows 控制台默认代码页会把 print 变成 UnicodeEncodeError。
    with contextlib.suppress(AttributeError):
        stream = sys.stdout
        if isinstance(stream, io.TextIOWrapper):
            stream.reconfigure(encoding="utf-8", errors="replace")


def _kill_tree(proc: subprocess.Popen[bytes]) -> None:
    # 超时必须终止整个进程树（cargo 会派生 rustc / 测试二进制子进程），
    # 只杀父进程会留下继续占用 target/ 锁的孤儿进程。
    if sys.platform == "win32":
        taskkill = shutil.which("taskkill")
        if taskkill is not None:
            _ = subprocess.run([taskkill, "/T", "/F", "/PID", str(proc.pid)], capture_output=True, check=False)
    else:
        # POSIX 侧 run_logged 以 start_new_session 启动，killpg 可达整组。
        with contextlib.suppress(ProcessLookupError, PermissionError):
            os.killpg(os.getpgid(proc.pid), signal.SIGKILL)


def _tail(log: Path, limit: int = 12) -> str:
    lines = log.read_text(encoding="utf-8", errors="replace").splitlines()
    return "\n".join(lines[-limit:])


def run_logged(name: str, argv: list[str], *, timeout: float, env_extra: dict[str, str] | None = None) -> StageResult:
    """运行单个命令阶段：完整输出落 .tmp/test-gate/<name>.log，凭退出码判定成败."""
    log = LOG_DIR / f"{name}.log"
    LOG_DIR.mkdir(parents=True, exist_ok=True)
    env = os.environ | (env_extra or {})
    started = time.monotonic()
    with log.open("wb") as sink:
        proc = subprocess.Popen(
            argv,
            cwd=ROOT,
            stdout=sink,
            stderr=subprocess.STDOUT,
            env=env,
            start_new_session=sys.platform != "win32",
        )
        timed_out = False
        try:
            _ = proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            _kill_tree(proc)
            _ = proc.wait(timeout=30)
    elapsed = time.monotonic() - started
    if timed_out:
        detail = f"超过 {timeout:.0f}s 限制，进程树已终止；完整日志：{log}"
        return StageResult(name, STATUS_TIMED_OUT, detail, log)
    if proc.returncode == 0:
        return StageResult(name, STATUS_OK, f"{elapsed:.1f}s；完整日志：{log}", log)
    detail = f"退出码 {proc.returncode}；日志尾部：\n{_tail(log)}"
    return StageResult(name, STATUS_FAILED, detail, log)


def _scan_binding_loop(result: StageResult) -> StageResult:
    # AGENTS.md 第 4 节禁止布局绑定环；acceptance.ps1 对 cargo test 输出做同样扫描。
    if result.status != STATUS_OK or result.log is None:
        return result
    text = result.log.read_text(encoding="utf-8", errors="replace")
    hits = [line.strip() for line in text.splitlines() if "binding loop" in line]
    if not hits:
        return result
    detail = "构建输出出现 binding loop 警告（AGENTS.md 第 4 节禁止）：\n" + "\n".join(hits[:6])
    return StageResult(result.name, STATUS_FAILED, detail, result.log)


def _has_blocking(results: list[StageResult]) -> bool:
    return any(result.status in (STATUS_FAILED, STATUS_TIMED_OUT) for result in results)


def _print_summary(title: str, results: list[StageResult], notes: list[str]) -> int:
    print(f"\n==== {title} ====")
    for result in results:
        print(f"{result.status:10} {result.name}")
        for line in result.detail.splitlines():
            print(f"           {line}")
    for note in notes:
        print(f"{STATUS_NOT_RUN:10} {note}")
    unverified = [result.name for result in results if result.status == STATUS_UNVERIFIED]
    if all(result.status == STATUS_OK for result in results):
        print(f"Overall: PASS（{len(results)} 个阶段全部通过）")
        return 0
    if unverified:
        names = ", ".join(unverified)
        print(f"Overall: NOT FULLY VERIFIED（存在 UNVERIFIED 阶段：{names}；不得报告为通过）")
    else:
        print("Overall: FAIL")
    return 1


def cmd_fastcheck(deadline_seconds: float) -> int:
    started = time.monotonic()
    deadline = started + deadline_seconds
    results: list[StageResult] = []
    cargo = shutil.which("cargo")
    if cargo is None:
        results.append(StageResult("toolchain", STATUS_UNVERIFIED, "PATH 上找不到 cargo"))
        return _print_summary("fastcheck", results, [])
    plan: list[tuple[str, list[str]]] = [
        ("static-check", [sys.executable, str(ROOT / "scripts" / "static_check.py")]),
        ("rustfmt-check", [cargo, "fmt", "--all", "--", "--check"]),
        ("clippy", [cargo, "clippy", "--all-targets", "--", "-D", "warnings"]),
        ("cargo-test", [cargo, "test", "--all-targets"]),
    ]
    over_budget = False
    for name, argv in plan:
        if over_budget:
            results.append(StageResult(name, STATUS_NOT_RUN, "时间预算已耗尽（前置阶段超时）"))
            continue
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            over_budget = True
            budget = f"时间预算 {deadline_seconds:.0f}s 已耗尽，本阶段未能在预算内完成"
            results.append(StageResult(name, STATUS_TIMED_OUT, budget))
            continue
        result = run_logged(name, argv, timeout=remaining)
        if name == "cargo-test":
            result = _scan_binding_loop(result)
        results.append(result)
        if result.status == STATUS_TIMED_OUT:
            over_budget = True
        elif result.status != STATUS_OK:
            break
    elapsed = time.monotonic() - started
    code = _print_summary("fastcheck", results, [])
    budget_note = f"墙钟：{elapsed:.1f}s / 预算 {deadline_seconds:.0f}s（60 秒为硬上限，超时即失败）"
    print(budget_note)
    return code


def _python_quality_stages(results: list[StageResult]) -> None:
    # 与 CI "Python quality gate" 完全同一组命令；工具缺失只能 UNVERIFIED，不得静默跳过。
    stages: list[tuple[str, str, list[str]]] = [
        ("pyquality-ruff-format", "ruff", [sys.executable, "-m", "ruff", "format", "--check", "scripts", "typings"]),
        ("pyquality-ruff-lint", "ruff", [sys.executable, "-m", "ruff", "check", "scripts", "typings"]),
        ("pyquality-basedpyright", "basedpyright", [sys.executable, "-m", "basedpyright", "scripts", "typings"]),
        (
            "pyquality-vulture",
            "vulture",
            [sys.executable, "-m", "vulture", "scripts", "typings", "--min-confidence", "100"],
        ),
        ("pyquality-bandit", "bandit", [sys.executable, "-m", "bandit", "-r", "scripts", "-s", "B404,B603", "-q"]),
        # pip-audit 的模块名是下划线形式 pip_audit（连字符只是 console script 名）。
        ("pyquality-pip-audit", "pip_audit", [sys.executable, "-m", "pip_audit", "-r", "scripts/requirements-dev.txt"]),
    ]
    for name, module, argv in stages:
        if importlib.util.find_spec(module) is None:
            hint = f"工具模块 {module} 不可用；pip install -r scripts/requirements-quality.txt 后重跑"
            results.append(StageResult(name, STATUS_UNVERIFIED, hint))
            continue
        results.append(run_logged(name, argv, timeout=600.0))


def _fulltest_stages(results: list[StageResult]) -> None:
    cargo = shutil.which("cargo")
    powershell = shutil.which("powershell")
    if cargo is None:
        results.append(StageResult("toolchain", STATUS_UNVERIFIED, "PATH 上找不到 cargo"))
        return
    if powershell is None:
        results.append(StageResult("powershell", STATUS_UNVERIFIED, "PATH 上找不到 powershell（acceptance.ps1 需要）"))
        return
    _python_quality_stages(results)
    if _has_blocking(results):
        return
    results.append(run_logged("rustfmt-check", [cargo, "fmt", "--all", "--", "--check"], timeout=300.0))
    results.append(run_logged("clippy", [cargo, "clippy", "--all-targets", "--", "-D", "warnings"], timeout=1800.0))
    if _has_blocking(results):
        return
    # GUI 冒烟数据集：一次性生成在 .tmp/ 下，跑完删除（不整清 .tmp/，保留各阶段日志）。
    argv = [
        sys.executable,
        str(ROOT / "scripts" / "make_tmp.py"),
        "testdata",
        "--destination",
        str(GUI_DATA_DIR),
        "--force",
    ]
    results.append(run_logged("gui-testdata", argv, timeout=600.0))
    if _has_blocking(results):
        return
    # 单命令复用可信基验收入口：static_check + 全量测试 + binding 扫描 + 真实引擎用例
    # + GUI 冒烟（内含 gui-build）+ 打包自检（package-windows.ps1，本地构建，不触发远程）。
    acceptance_argv = [
        powershell,
        "-NoProfile",
        "-File",
        str(ROOT / "scripts" / "acceptance.ps1"),
        "-WithEngine",
        "-WithGuiSmoke",
        "-GuiData",
        str(GUI_DATA_DIR),
        "-WithPackage",
    ]
    # 本机执行策略全作用域 Undefined（默认 Restricted）会拒绝任何 -File 运行 .ps1；
    # PSExecutionPolicyPreference 以 Process 作用域覆盖之，且随环境继承给
    # acceptance.ps1 内部再起的 powershell 子进程（package 阶段），只影响本进程树。
    acceptance_env = {"PSExecutionPolicyPreference": "Bypass"}
    results.append(run_logged("acceptance", acceptance_argv, timeout=STAGE_TIMEOUT_DEFAULT, env_extra=acceptance_env))
    shutil.rmtree(GUI_DATA_DIR, ignore_errors=True)
    results.append(StageResult("cleanup-gui-data", STATUS_OK, f"已删除一次性数据集 {GUI_DATA_DIR}"))


def cmd_fulltest() -> int:
    if sys.platform != "win32":
        message = "fulltest 按 Windows 当前平台设计（acceptance.ps1 为 Windows 专用）；"
        message += "平台范围按合同 P-07 仅支持 Windows（原 Linux 验证入口已移除）"
        print(message)
        return 2
    results: list[StageResult] = []
    _fulltest_stages(results)
    return _print_summary("fulltest（当前平台 Windows）", results, [])


def _str_field(entry: object, key: str) -> str | None:
    if _is_str_obj_map(entry):
        got = entry.get(key)
        if isinstance(got, str):
            return got
    return None


def _git_output(git: str, args: list[str]) -> str | None:
    done = subprocess.run([git, *args], cwd=ROOT, capture_output=True, text=True, check=False)
    if done.returncode != 0:
        return None
    return done.stdout.strip()


def _trigger_workflow(git: str, gh: str, workflow: str) -> tuple[str, str] | StageResult:
    """前置检查并触发远程工作流；成功返回 (head, branch)，失败返回对应阶段结果."""
    stage = f"remote-{workflow}"
    head = _git_output(git, ["rev-parse", "HEAD"])
    if head is None:
        return StageResult(stage, STATUS_UNVERIFIED, "git rev-parse HEAD 失败")
    pushed = _git_output(git, ["branch", "-r", "--contains", "HEAD"])
    if not pushed:
        detail = "当前 HEAD 未推送到远程，CI 无法验证该提交；请先 push 后重跑 slowtest"
        return StageResult(stage, STATUS_UNVERIFIED, detail)
    branch = _git_output(git, ["rev-parse", "--abbrev-ref", "HEAD"])
    if branch is None:
        return StageResult(stage, STATUS_UNVERIFIED, "无法解析当前分支名")
    triggered = subprocess.run(
        [gh, "workflow", "run", workflow, "--ref", branch], cwd=ROOT, capture_output=True, text=True, check=False
    )
    if triggered.returncode != 0:
        stderr = (triggered.stderr or "").strip()
        detail = f"gh workflow run 失败（退出码 {triggered.returncode}）：{stderr}"
        return StageResult(stage, STATUS_FAILED, detail)
    return head, branch


def _find_completed_run(gh: str, workflow: str, branch: str, head: str) -> StageResult | None:
    """查询该提交的 run；已完结返回最终结果，未完结或查询失败返回 None."""
    stage = f"remote-{workflow}"
    json_fields = "headSha,status,conclusion,url"
    listing = subprocess.run(
        [gh, "run", "list", "--workflow", workflow, "--branch", branch, "--limit", "10", "--json", json_fields],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    if listing.returncode != 0:
        return None
    entries = _parse_json(listing.stdout)
    if not _is_obj_list(entries):
        return None
    for entry in entries:
        if _str_field(entry, "headSha") != head:
            continue
        url = _str_field(entry, "url") or "（未取得 run 链接）"
        if _str_field(entry, "status") == "completed":
            conclusion = _str_field(entry, "conclusion")
            if conclusion == "success":
                return StageResult(stage, STATUS_OK, url)
            detail = f"流水线最终状态 {conclusion}；run：{url}"
            return StageResult(stage, STATUS_FAILED, detail)
    return None


def _stage_remote_workflow(git: str, gh: str, workflow: str, *, watch_seconds: float) -> StageResult:
    """触发远程验证流水线并轮询到最终状态；「已成功触发」不等于「流水线通过」."""
    context = _trigger_workflow(git, gh, workflow)
    if isinstance(context, StageResult):
        return context
    head, branch = context
    stage = f"remote-{workflow}"
    deadline = time.monotonic() + watch_seconds
    while time.monotonic() < deadline:
        time.sleep(REMOTE_POLL_INTERVAL_SECONDS)
        found = _find_completed_run(gh, workflow, branch, head)
        if found is not None:
            return found
    minutes = watch_seconds / 60
    detail = f"等待 {minutes:.0f} 分钟仍未完成（未验证完成，不得报告为通过）"
    return StageResult(stage, STATUS_UNVERIFIED, detail)


def cmd_slowtest() -> int:
    if sys.platform != "win32":
        message = (
            "slowtest 按 Windows 主机设计（Windows fulltest + 远程 CI）；请在 Windows 上运行（合同 P-07 仅 Windows）"
        )
        print(message)
        return 2
    results: list[StageResult] = []
    _fulltest_stages(results)
    # 前置本地验证已失败时停止后续远程阶段，避免在已知失败状态下消耗流水线资源。
    git = shutil.which("git")
    gh = shutil.which("gh")
    if git is None or gh is None:
        state = f"git={'有' if git else '缺'} gh={'有' if gh else '缺'}；远程阶段无法验证"
        results.append(StageResult("remote-ci", STATUS_UNVERIFIED, state))
    elif not _has_blocking(results):
        auth = subprocess.run([gh, "auth", "status"], capture_output=True, text=True, check=False)
        if auth.returncode != 0:
            results.append(StageResult("remote-ci", STATUS_UNVERIFIED, "gh 未登录（先 gh auth login）"))
        else:
            results.append(_stage_remote_workflow(git, gh, "check.yml", watch_seconds=REMOTE_CHECK_WATCH_SECONDS))
    release_note = (
        "release-workflow：真实发布（自动打时间戳 tag 并发布安装包与便携 ZIP），不属于 slowtest；"
        "如需发布请单独确认目标后手动运行 gh workflow run release.yml"
    )
    return _print_summary("slowtest（fulltest + 远程流水线）", results, [release_note])


def main(argv: list[str] | None = None) -> int:
    _reconfigure_stdout()
    description = "JchTools 三级测试门：fastcheck（AI 自主，≤60s）/ fulltest / slowtest（后两级需 --authorized）"
    parser = argparse.ArgumentParser(description=description)
    sub = parser.add_subparsers(dest="gate", required=True)
    fast = sub.add_parser("fastcheck", help="快速反馈：static_check + fmt + clippy + cargo test；总墙钟硬上限 60 秒")
    _ = fast.add_argument(
        "--deadline-seconds",
        type=float,
        default=FASTCHECK_DEADLINE_SECONDS,
        help="仅允许 (0,60]（用于验证超时路径），不得调高绕过硬上限",
    )
    full = sub.add_parser("fulltest", help="当前平台（Windows）全部本地检查；不触发远程流水线")
    _ = full.add_argument("--authorized", action="store_true", help="确认本次运行已由人类明确授权")
    slow = sub.add_parser("slowtest", help="fulltest + 远程 CI（check.yml）")
    _ = slow.add_argument("--authorized", action="store_true", help="确认本次运行已由人类明确授权")
    arguments = parser.parse_args(argv, namespace=_Arguments())
    if arguments.gate == "fastcheck":
        deadline = arguments.deadline_seconds
        if not 0 < deadline <= FASTCHECK_DEADLINE_SECONDS:
            message = f"--deadline-seconds 仅允许 (0, 60]，收到 {deadline}"
            print(message)
            return 2
        return cmd_fastcheck(deadline)
    if not arguments.authorized:
        message = f"{arguments.gate} 每次运行都需要人类明确授权：确认后加 --authorized 重新执行。"
        message += "历史授权、CI 建议或脚本注释都不构成本次授权。"
        print(message)
        return 2
    if arguments.gate == "fulltest":
        return cmd_fulltest()
    return cmd_slowtest()


if __name__ == "__main__":
    sys.exit(main())
