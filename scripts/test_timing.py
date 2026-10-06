"""测试性能数据采集：套件级与用例级两轮计时，产出 JSON 与 Markdown 报告.

动机：整仓测试耗时偏长，需要每个测试都有性能数据才能定位可优化对象。本脚本
只测量与分析入口，不修改任何测试；测量通过直接运行 cargo 预编译的测试二进制
（与 cargo test 相同的执行形态），数据落在 .tmp/test-timing/（gitignore 范围）。

两轮测量：
1. 套件级：逐个测试二进制整跑，得到每个套件的墙钟时间（含套件内并行）；
2. 用例级：只对超过阈值的慢套件，逐用例 --exact 单跑计时（含单跑固定开销：
   进程启动、夹具与清场），按预算截停，未测用例如实标注。

用法（仓库根目录）：
  python scripts/test_timing.py                     # 构建后全流程
  python scripts/test_timing.py --skip-build        # 复用已构建产物
  python scripts/test_timing.py --suite-only        # 只做套件级
  python scripts/test_timing.py --per-test-budget 600
  python scripts/test_timing.py --target-dir target/test-timing

`--target-dir` 以 cargo 显式旗标传入（相对仓库根解析），适合在默认 target 被占用
（例如真实服务正从 build 目录运行）时用隔离树测量；隔离树可配
`CARGO_PROFILE_DEV_DEBUG=0`（环境变量，仅作用于该树，不影响项目配置）。
"""

from __future__ import annotations

import argparse
import contextlib
import io
import json
import os
import shutil
import signal
import subprocess
import sys
import time
from dataclasses import asdict, dataclass, field
from datetime import UTC, datetime
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT_DIR = ROOT / ".tmp" / "test-timing"
SLOW_SUITE_SECONDS = 5.0
SINGLE_TEST_TIMEOUT = 300.0
PROCESS_TREE_CLEANUP_SECONDS = 5.0
LIST_HEADER = "Running "
LIST_SUFFIX = ".exe)"


class _Arguments(argparse.Namespace):
    """带类型标注的解析结果；未提供的开关保留安全默认值."""

    skip_build: bool = False
    suite_only: bool = False
    per_test_budget: float = 600.0
    target_dir: Path | None = None


def _reconfigure_stdout() -> None:
    # 输出含中文；Windows 控制台默认代码页会把 print 变成 UnicodeEncodeError
    # （与 scripts/test_gate.py 同一处理）。
    with contextlib.suppress(AttributeError):
        stream = sys.stdout
        if isinstance(stream, io.TextIOWrapper):
            stream.reconfigure(encoding="utf-8", errors="replace")


@dataclass
class Suite:
    """一个测试二进制（套件）：描述、可执行文件路径与计时结果."""

    desc: str
    exe: Path
    seconds: float | None = None
    test_names: list[str] = field(default_factory=list)
    failed: bool = False


@dataclass
class PerTestRow:
    """用例级计时结果：seconds 为 None 表示预算截停未测."""

    suite: str
    name: str
    seconds: float | None
    note: str


def _kill_tree(proc: subprocess.Popen[str], *, timeout: float) -> bool:
    if sys.platform == "win32":
        taskkill = shutil.which("taskkill")
        if taskkill is None:
            return False
        try:
            result = subprocess.run(
                [taskkill, "/T", "/F", "/PID", str(proc.pid)],
                capture_output=True,
                check=False,
                timeout=timeout,
            )
        except subprocess.TimeoutExpired:
            return False
        return result.returncode == 0
    try:
        os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
    except ProcessLookupError:
        return True
    except PermissionError:
        return False
    return True


def _run_checked(argv: list[str], timeout: float) -> subprocess.CompletedProcess[str]:
    cleanup_budget = PROCESS_TREE_CLEANUP_SECONDS
    if timeout <= cleanup_budget:
        message = f"阶段预算 {timeout:.1f}s 不足以启动并清理进程树"
        raise ValueError(message)
    started = time.monotonic()
    deadline = started + timeout
    with subprocess.Popen(
        argv,
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        errors="replace",
        start_new_session=sys.platform != "win32",
    ) as proc:
        try:
            stdout, stderr = proc.communicate(timeout=timeout - cleanup_budget)
        except subprocess.TimeoutExpired as error:
            kill_timeout = min(2.0, max(0.0, deadline - time.monotonic()))
            tree_terminated = _kill_tree(proc, timeout=kill_timeout)
            if not tree_terminated and proc.poll() is None:
                proc.kill()
            try:
                _ = proc.wait(timeout=max(0.0, deadline - time.monotonic()))
            except subprocess.TimeoutExpired as wait_error:
                if proc.poll() is None:
                    proc.kill()
                message = f"命令超时后未能在预算内确认进程退出：{argv[0]}"
                raise RuntimeError(message) from wait_error
            if not tree_terminated:
                message = f"命令超时后无法确认进程树已终止：{argv[0]}"
                raise RuntimeError(message) from error
            raise subprocess.TimeoutExpired(argv, timeout) from error
        returncode = proc.wait()
    return subprocess.CompletedProcess(argv, returncode, stdout, stderr)


def _cargo_base(target_dir: Path | None) -> list[str]:
    """公共 cargo 前缀；--target-dir 以显式旗标传递（相对仓库根解析）."""
    prefix = ["cargo", "test", "--workspace", "--all-targets", "--locked"]
    if target_dir is not None:
        prefix += ["--target-dir", str(target_dir)]
    return prefix


def build_tests(target_dir: Path | None) -> None:
    """一次性预编译全部测试目标，后续直接运行测试二进制."""
    print("[1/4] 预编译测试目标（cargo test --no-run）...")
    done = _run_checked(
        [*_cargo_base(target_dir), "--no-run"],
        timeout=1200.0,
    )
    if done.returncode != 0:
        tail = "\n".join(done.stderr.splitlines()[-12:])
        print(f"构建失败（退出码 {done.returncode}）：\n{tail}")
        sys.exit(1)


def list_suites(target_dir: Path | None) -> list[Suite]:
    """枚举套件与其用例清单.

    cargo 把 `Running <目标> (<exe>)` 套件头写在 stderr、把 `--list` 的用例清单写在
    stdout，两者在合并输出里不可靠交错；因此只从 stderr 取有序套件头，再逐个
    测试二进制直接 `--list` 取各自的用例名。
    """
    print("[2/4] 枚举套件与用例清单...")
    done = _run_checked(
        [*_cargo_base(target_dir), "--", "--list", "--format", "terse"],
        timeout=600.0,
    )
    if done.returncode != 0:
        print(f"用例清单枚举失败（退出码 {done.returncode}）")
        sys.exit(1)
    suites: list[Suite] = []
    for line in done.stderr.splitlines():
        stripped = line.strip()
        if stripped.startswith(LIST_HEADER) and stripped.endswith(LIST_SUFFIX):
            head, _, exe_part = stripped.partition(" (")
            suites.append(
                Suite(
                    desc=head.removeprefix(LIST_HEADER),
                    exe=Path(exe_part.removesuffix(")")),
                )
            )
    if not suites:
        OUT_DIR.mkdir(parents=True, exist_ok=True)
        raw = (done.stdout + done.stderr).splitlines()[:40]
        _ = (OUT_DIR / "list-raw.log").write_text("\n".join(raw), encoding="utf-8")
        print(f"未解析到任何套件头；原始输出前 40 行已落盘 {OUT_DIR / 'list-raw.log'}")
    for suite in suites:
        listing = _run_checked([str(suite.exe), "--list", "--format", "terse"], timeout=120.0)
        if listing.returncode == 0:
            suite.test_names = [
                line.strip().removesuffix(": test")
                for line in listing.stdout.splitlines()
                if line.strip().endswith(": test")
            ]
    suites = [suite for suite in suites if suite.test_names]
    total = sum(len(suite.test_names) for suite in suites)
    print(f"共 {len(suites)} 个套件、{total} 个用例。")
    return suites


def time_suites(suites: list[Suite]) -> None:
    """套件级计时：逐个测试二进制整跑（套件内按 libtest 默认并行）."""
    print("[3/4] 套件级计时（逐二进制整跑）...")
    for index, suite in enumerate(suites, start=1):
        started = time.perf_counter()
        try:
            done = _run_checked([str(suite.exe)], timeout=1800.0)
            suite.failed = done.returncode != 0
        except subprocess.TimeoutExpired:
            suite.failed = True
        suite.seconds = time.perf_counter() - started
        mark = "FAIL" if suite.failed else "ok"
        print(f"  [{index:>2}/{len(suites)}] {mark} {suite.seconds:8.2f}s  {suite.desc}")


def time_individual_tests(suites: list[Suite], budget: float) -> list[PerTestRow]:
    """用例级计时：慢套件逐用例 --exact 单跑，累计预算截停."""
    print(f"[4/4] 用例级计时（套件阈值 {SLOW_SUITE_SECONDS}s，总预算 {budget:.0f}s）...")
    rows: list[PerTestRow] = []
    remaining = budget
    slow = sorted(
        (suite for suite in suites if (suite.seconds or 0.0) >= SLOW_SUITE_SECONDS),
        key=lambda suite: suite.seconds or 0.0,
        reverse=True,
    )
    for suite in slow:
        for name in suite.test_names:
            if remaining <= 0.0:
                rows.append(PerTestRow(suite.desc, name, None, "预算截停未测"))
                continue
            started = time.perf_counter()
            note = ""
            try:
                done = _run_checked(
                    [str(suite.exe), name, "--exact", "--format", "terse"],
                    timeout=SINGLE_TEST_TIMEOUT,
                )
                if done.returncode != 0:
                    note = "失败"
            except subprocess.TimeoutExpired:
                note = "单用例超时"
            elapsed = time.perf_counter() - started
            remaining -= elapsed
            rows.append(PerTestRow(suite.desc, name, elapsed, note))
            flag = " <-- " + note if note else ""
            print(f"  {elapsed:7.2f}s  {suite.desc} :: {name}{flag}")
    return rows


def _suite_seconds(suite: Suite) -> float:
    """报告用的套件秒数：未计时按 0 呈现（当前流程总会在计时后调用）."""
    return suite.seconds if suite.seconds is not None else 0.0


def write_report(suites: list[Suite], rows: list[PerTestRow], budget: float) -> None:
    """写出 timing.json 与 report.md，并打印摘要."""
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    ordered = sorted(suites, key=_suite_seconds, reverse=True)
    payload = {
        "generated_utc": datetime.now(UTC).isoformat(),
        "slow_suite_threshold_seconds": SLOW_SUITE_SECONDS,
        "per_test_budget_seconds": budget,
        "suites": [
            {
                "desc": suite.desc,
                "exe": suite.exe.name,
                "seconds": _suite_seconds(suite),
                "tests": len(suite.test_names),
                "failed": suite.failed,
            }
            for suite in ordered
        ],
        "per_test": [asdict(row) for row in rows],
    }
    _ = (OUT_DIR / "timing.json").write_text(
        json.dumps(payload, ensure_ascii=False, indent=2),
        encoding="utf-8",
    )
    lines = [
        "# 测试性能数据",
        "",
        f"- 生成时间（UTC）：{payload['generated_utc']}",
        f"- 套件阈值：{SLOW_SUITE_SECONDS}s；用例级总预算：{budget:.0f}s",
        "",
        "## 套件排行（整跑墙钟，含套件内并行）",
        "",
        "| 秒 | 套件 | 用例数 | 结果 |",
        "|---:|---|---:|---|",
    ]
    for suite in ordered:
        outcome = "FAIL" if suite.failed else "ok"
        lines.append(f"| {_suite_seconds(suite):.2f} | {suite.desc} | {len(suite.test_names)} | {outcome} |")
    lines += [
        "",
        "## 用例排行（单跑墙钟，含进程启动与夹具固定开销；仅慢套件）",
        "",
        "| 秒 | 套件 :: 用例 | 备注 |",
        "|---:|---|---|",
    ]
    measured = sorted(
        (row for row in rows if row.seconds is not None),
        key=lambda row: row.seconds or 0.0,
        reverse=True,
    )
    for row in measured:
        seconds = row.seconds or 0.0
        lines.append(f"| {seconds:.2f} | {row.suite} :: {row.name} | {row.note} |")
    _ = (OUT_DIR / "report.md").write_text("\n".join(lines) + "\n", encoding="utf-8")
    total_suite = sum(_suite_seconds(suite) for suite in suites)
    print(f"\n汇总：{len(suites)} 套件合计 {total_suite:.1f}s；用例级实测 {len(measured)} 条。")
    print(f"数据已写出：{OUT_DIR / 'timing.json'} 与 {OUT_DIR / 'report.md'}")
    for suite in ordered[:5]:
        print(f"  最慢套件 Top5：{_suite_seconds(suite):8.2f}s  {suite.desc}")


def main(argv: list[str] | None = None) -> int:
    if sys.platform != "win32":
        print("test_timing 按合同 P-07 仅支持 Windows；拒绝执行测试计时。")
        return 2
    _reconfigure_stdout()
    parser = argparse.ArgumentParser(description="采集全部测试的套件级与用例级耗时数据")
    _ = parser.add_argument("--skip-build", action="store_true", help="复用已构建的测试二进制")
    _ = parser.add_argument("--suite-only", action="store_true", help="只做套件级计时")
    _ = parser.add_argument("--per-test-budget", type=float, default=600.0, help="用例级测量总预算（秒）")
    _ = parser.add_argument(
        "--target-dir",
        type=Path,
        default=None,
        help="cargo --target-dir（相对仓库根解析；默认 target 被占用时用隔离树测量）",
    )
    args = parser.parse_args(argv, namespace=_Arguments())
    budget = args.per_test_budget
    target_dir = (ROOT / args.target_dir).resolve() if args.target_dir is not None else None
    if not args.skip_build:
        build_tests(target_dir)
    suites = list_suites(target_dir)
    if not suites:
        print("未发现任何测试二进制")
        return 1
    time_suites(suites)
    rows: list[PerTestRow] = []
    if not args.suite_only:
        rows = time_individual_tests(suites, budget)
    write_report(suites, rows, budget)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
