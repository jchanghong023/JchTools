"""既有测试结果的 JUnit 落盘；不调度测试，不改执行契约或构建缓存."""

from __future__ import annotations

import hashlib
import json
import math
import os
import platform
import re
import shutil
import subprocess
import sys
import time
import uuid
from dataclasses import dataclass
from datetime import UTC, datetime
from html import escape
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from collections.abc import Callable

ROOT = Path(__file__).resolve().parent.parent
_XML_INVALID = re.compile("[^\\x09\\x0a\\x0d\\x20-\\ud7ff\\ue000-\\ufffd\\U00010000-\\U0010ffff]")


def _xml_attribute(value: str) -> str:
    escaped = escape(_XML_INVALID.sub("", value), quote=True)
    # 先转义，再保留属性中的制表符与换行；否则 XML 解析器会将这些空白归一化。
    return escaped.replace("\t", "&#9;").replace("\n", "&#10;").replace("\r", "&#13;")


@dataclass(frozen=True)
class Case:
    name: str
    classname: str
    duration: float | None
    status: str = "passed"
    detail: str = ""
    file: str | None = None


def _git_bytes(arguments: list[str]) -> bytes | None:
    git = shutil.which("git")
    if git is None:
        return None
    try:
        result = subprocess.run([git, *arguments], cwd=ROOT, capture_output=True, check=False)
    except OSError as error:
        error.add_note(f"报告源标识操作：subprocess.run argv={[git, *arguments]!r}, cwd={str(ROOT)!r}")
        raise
    return result.stdout if result.returncode == 0 else None


def _source_identity() -> dict[str, object]:
    revision = _git_bytes(["rev-parse", "HEAD"])
    status = _git_bytes(["status", "--porcelain=v1", "-z"])
    diff = _git_bytes(["diff", "--binary", "HEAD"])
    untracked = _git_bytes(["ls-files", "--others", "--exclude-standard", "-z"])
    digest = hashlib.sha256()
    identity: str | None = None
    if status is not None and diff is not None and untracked is not None:
        digest.update(status)
        digest.update(diff)
        try:
            for name in sorted(untracked.split(b"\0")):
                if name:
                    digest.update(name)
                    digest.update(hashlib.sha256((ROOT / os.fsdecode(name)).read_bytes()).digest())
        except OSError:
            identity = None
        else:
            identity = digest.hexdigest()
    return {
        "revision": revision.decode("utf-8").strip() if revision is not None else None,
        "dirty": bool(status) if status is not None else None,
        "dirty_identity_sha256": identity,
        "identity_basis": "git porcelain + tracked HEAD diff + untracked file content hashes",
    }


def _replace_bytes(path: Path, content: bytes) -> None:
    temporary = path.with_suffix(path.suffix + ".writing")
    try:
        _ = temporary.write_bytes(content)
    except OSError as error:
        error.add_note(f"报告持久化操作：write_bytes path={str(temporary)!r}, destination={str(path)!r}")
        raise
    try:
        _ = temporary.replace(path)
    except OSError as error:
        error.add_note(f"报告发布操作：replace source={str(temporary)!r}, destination={str(path)!r}")
        raise


class TimingReport:
    """每次调用独占目录；逐项检查点保留被外部测试门终止前的真实结果."""

    def __init__(
        self,
        producer: str,
        command: list[str],
        *,
        scope: object,
        timing_basis: str,
        concurrency: object,
    ) -> None:
        self._started: float = time.monotonic()
        self._cases: list[Case] = []
        started_utc = datetime.now(UTC)
        self.run_id: str = started_utc.strftime("%Y%m%dT%H%M%S") + "-" + uuid.uuid4().hex
        self.directory: Path = ROOT / ".tmp" / "test-reports" / self.run_id
        # 统一经项目临时目录工厂创建；UUID 隔离重叠运行，不迁移 Cargo 缓存。
        try:
            _ = subprocess.run(
                [sys.executable, str(ROOT / "scripts/make_tmp.py"), "workspace", "--destination", str(self.directory)],
                cwd=ROOT,
                check=True,
            )
        except (OSError, subprocess.CalledProcessError) as error:
            error.add_note(f"报告目录初始化操作：make_tmp workspace destination={self.directory}")
            error.add_note(f"python={sys.executable!r}, cwd={ROOT}")
            raise
        self._metadata: dict[str, object] = {
            "run_id": self.run_id,
            "started_utc": started_utc.isoformat(),
            "producer": producer,
            "source": _source_identity(),
            "platform": {"os": platform.system(), "release": platform.release(), "architecture": platform.machine()},
            "versions": {
                "python": platform.python_version(),
                "runner": producer,
                "runner_version": platform.python_version() if producer == "unittest" else None,
            },
            "command": command,
            "selection_scope": scope,
            "concurrency": concurrency,
            "timing_basis": timing_basis,
            "observed_wall_seconds": None,
            "exit_code": None,
            "completion": "partial",
            "coverage": None,
            "report_paths": ["junit.xml"],
            "unavailable_values": "null means unavailable or not yet observed; no duration inferred from stage totals",
        }
        self._persist()
        print(f"测试报告：{self.directory}", flush=True)

    def add_case(self, case: Case) -> None:
        if case.status not in {"passed", "failure", "error", "skipped"}:
            message = f"未知测试结果状态：{case.status}"
            raise ValueError(message)
        if case.duration is None:
            if case.status != "skipped":
                message = f"已执行用例缺少真实耗时：{case.classname}::{case.name}"
                raise ValueError(message)
        elif not math.isfinite(case.duration) or case.duration < 0:
            message = f"用例耗时不是有限非负秒数：{case.classname}::{case.name}"
            raise ValueError(message)
        self._cases.append(case)
        self._persist()

    def _persist(self) -> None:
        self._metadata["observed_wall_seconds"] = time.monotonic() - self._started
        output = [
            '<?xml version="1.0" encoding="utf-8"?>\n',
            f'<testsuite name="{_xml_attribute(str(self._metadata["producer"]))}" ',
            f'tests="{len(self._cases)}" ',
            f'failures="{sum(case.status == "failure" for case in self._cases)}" ',
            f'errors="{sum(case.status == "error" for case in self._cases)}" ',
            f'skipped="{sum(case.status == "skipped" for case in self._cases)}">',
            "<properties>",
            f'<property name="run_id" value="{_xml_attribute(self.run_id)}"/>',
            f'<property name="timing_basis" value="{_xml_attribute(str(self._metadata["timing_basis"]))}"/>',
            "</properties>",
        ]
        for case in self._cases:
            output.append(f'<testcase name="{_xml_attribute(case.name)}" classname="{_xml_attribute(case.classname)}"')
            if case.duration is not None:
                output.append(f' time="{format(case.duration, ".9f")}"')
            if case.file is not None:
                output.append(f' file="{_xml_attribute(case.file)}"')
            output.append(">")
            if case.status != "passed":
                detail = escape(_XML_INVALID.sub("", case.detail), quote=False)
                output.append(f"<{case.status}>{detail}</{case.status}>")
            output.append("</testcase>")
        output.append("</testsuite>")
        _replace_bytes(self.directory / "junit.xml", "".join(output).encode("utf-8"))
        self._metadata["case_counts"] = {
            status: sum(case.status == status for case in self._cases)
            for status in ("passed", "failure", "error", "skipped")
        }
        _replace_bytes(
            self.directory / "run.json",
            (json.dumps(self._metadata, ensure_ascii=False, indent=2) + "\n").encode("utf-8"),
        )

    def finish(self, exit_code: int, *, completion: str = "complete", coverage: object = None) -> int:
        self._metadata.update(
            exit_code=exit_code,
            completion=completion,
            coverage=coverage,
            observed_wall_seconds=time.monotonic() - self._started,
            finished_utc=datetime.now(UTC).isoformat(),
        )
        try:
            self._persist()
        except (OSError, ValueError, TypeError) as error:
            print(f"测试报告写入失败：{error}", file=sys.stderr, flush=True)
            return exit_code or 1
        return exit_code


def run_atomic(producer: str, action: Callable[[], int], *, scope: object, error_exit_code: int = 1) -> int:
    """只用于确实没有内部用例协议的单项脚本；粒度明确为 command-level."""
    report = TimingReport(
        producer,
        [sys.executable, *sys.argv],
        scope=scope,
        timing_basis="atomic command body wall time, including its fixtures and cleanup; not per-assertion timing",
        concurrency=1,
    )
    started = time.monotonic()
    try:
        code = action()
    except BaseException as error:
        code = 130 if isinstance(error, KeyboardInterrupt) else error_exit_code
        try:
            report.add_case(Case(producer, producer, time.monotonic() - started, "error", str(error)))
        except (OSError, ValueError, TypeError) as report_error:
            print(f"测试报告写入失败：{report_error}", file=sys.stderr)
        _ = report.finish(code, completion="partial")
        raise
    try:
        report.add_case(Case(producer, producer, time.monotonic() - started, "passed" if code == 0 else "failure"))
    except (OSError, ValueError, TypeError) as error:
        print(f"测试报告写入失败：{error}", file=sys.stderr)
        return report.finish(code or 1, completion="partial", coverage={"reporting_error": str(error)})
    return report.finish(code)
