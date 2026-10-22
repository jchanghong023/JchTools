"""任务库/SQLite 读取、Xberg 设置读取与隔离状态环境。.

由 gui_smoke.py 按域拆出；行为与拆分前一致，入口 scripts/gui_smoke.py 负责 re-export。
"""

from __future__ import annotations

import contextlib
import json
import os
import sqlite3
import sys
import time
from pathlib import Path
from typing import cast

if not __package__:
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from scripts.gui_smoke_impl import config, driver
from scripts.make_tmp import guard_destination


def state_dir() -> Path:
    r"""任务库与 GUI 使用同一隔离状态根，缺覆盖时为 %LOCALAPPDATA%\JchTools\data."""
    isolated = os.environ.get("JCHTOOLS_TEST_STATE_DIR")
    if isolated is not None:
        path = Path(isolated)
        if not path.is_absolute():
            msg = "JCHTOOLS_TEST_STATE_DIR 必须为绝对路径，拒绝回退到真实任务目录"
            raise RuntimeError(msg)
        return path
    local = os.environ.get("LOCALAPPDATA")
    if not local:
        msg = "缺少 LOCALAPPDATA，无法定位任务目录"
        raise RuntimeError(msg)
    return Path(local) / "JchTools" / "data"


def task_records(data: str) -> list[tuple[str, object]]:
    """按目录名列出 data 的任务；转义 SQLite URI 路径，读取失败也关闭观察连接。."""
    tasks = state_dir() / "tasks"
    want = os.path.normcase(str(Path(data).resolve()))
    records: list[tuple[str, object]] = []
    for task in tasks.glob("*"):
        db = task / "task.sqlite3"
        if not db.is_file():
            continue
        with contextlib.suppress(*config.TASK_RECORD_ERRORS):
            with contextlib.closing(sqlite3.connect(db.as_uri() + "?mode=ro", uri=True)) as conn:
                rows: dict[str, object] = dict(conn.execute("SELECT key,value FROM metadata").fetchall())
            root_raw = rows.get("root", '""')
            status_raw = rows.get("status", '""')
            # json.loads 只接受 str/bytes/bytearray；其余类型原本也是 TypeError 后跳过，等价。
            # 解析出的 root 非字符串时原本 removeprefix 抛 AttributeError 后跳过，等价。
            if isinstance(root_raw, (str, bytes, bytearray)) and isinstance(status_raw, (str, bytes, bytearray)):
                # cast(object) 是诚实放宽：json.loads 的 Any 在此进入类型化世界，JSON 值本就属于 object；
                # basedpyright all 模式对「Any 赋给 object」也报 reportAny，cast 是唯一无 suppression 出口。
                root = cast("object", json.loads(root_raw))
                if isinstance(root, str):
                    if os.path.normcase(root.removeprefix("\\\\?\\")) != want:
                        continue
                    records.append((task.name, json.loads(status_raw)))
    # 任务目录名唯一，按首元素排序与按整个元组排序结果一致。
    records.sort(key=lambda record: record[0])
    return records


def task_ids(data: str) -> frozenset[str]:
    """记录点击前全部任务 ID；同秒随机 UUID 不能用于推断先后。."""
    return frozenset(name for name, _status in task_records(data))


def wait_task_status(
    data: str, expected: str, before: frozenset[str], timeout: int = config.COMPLETION_TIMEOUT
) -> None:
    """等待这次点击新建的任务跑到 expected 状态.

    界面文本经 UIA 桥接并不总是可见（动态状态行时有时无），
    而任务库状态是「GUI 流程真的跑完」的可靠判据，因此用它做断言；
    点击前的全部任务 ID 通过 before 排除，避免读到任意旧任务留下的状态。
    """
    deadline = time.time() + timeout
    while time.time() < deadline:
        for name, status in task_records(data):
            if name in before:
                continue
            if status == expected:
                return
            if status in ("failed", "cancelled"):
                msg = f"任务 {name} 状态为 {status}"
                raise RuntimeError(msg)
        time.sleep(0.5)
    msg = f"等待任务状态 {expected} 超时（目录 {data}）"
    raise RuntimeError(msg)


def read_xberg_settings(state: Path) -> dict[str, str]:
    """只读观察被测应用实际写入的隔离 SQLite，不向数据库伪造 GUI 保存结果."""
    database = state / "config.sqlite3"
    with contextlib.closing(sqlite3.connect(database.as_uri() + "?mode=ro", uri=True)) as connection:
        rows = cast(
            "list[tuple[str, str]]",
            connection.execute("SELECT key,value FROM app_settings WHERE key LIKE 'xberg_%'").fetchall(),
        )
    return dict(rows)


def active_xberg_settings(state: Path) -> dict[str, str]:
    """只读活动来源与所有目录记录；离线检查/取消/重启不得悄悄改动它们."""
    return read_xberg_settings(state)


def same_runtime_path(actual: str | None, expected: Path) -> bool:
    """SQLite 的 verbatim 路径与普通 Win32 路径表示同一个实际目录."""
    return actual is not None and Path(plain_windows_path(actual)).resolve() == expected.resolve()


def verify_bundled_settings(settings: dict[str, str], runtime: Path) -> None:
    """实际活动目录、来源和 bundled 来源记录必须一致，而不是仅显示一个路径."""
    driver.require(settings.get("xberg_source") == "bundled", f"活动来源不是 bundled：{settings}")
    for key in ("xberg_directory", "xberg_bundled_directory"):
        driver.require(same_runtime_path(settings.get(key), runtime), f"{key} 未指向随包目录：{settings.get(key)!r}")


def seed_legacy_runtime(state: Path, runtime: Path) -> str:
    """只播种旧文本路径；SQLite 的迁移/来源记录必须由应用真实生成."""
    directory = str(runtime.resolve())
    root = state / "markdown-assets"
    root.mkdir(parents=True, exist_ok=True)
    _ = (root / "xberg-runtime-path.txt").write_text(directory, encoding="utf-8")
    return directory


def isolated_state_env(scratch_state: Path, bundled: Path | None = None) -> dict[str, str]:
    """隔离配置、组件、bundle 发现及旧引擎覆盖，不读取宿主安装目录/注册表."""
    env = dict(os.environ)
    for key in ("JCHTOOLS_TEST_XBERG_DIR", "JCHTOOLS_SMOKE_XBERG_DIR", "JCHTOOLS_TEST_AUDIO_DOWNLOAD_BARRIER"):
        _ = env.pop(key, None)
    env["JCHTOOLS_TEST_STATE_DIR"] = str(scratch_state)
    env["JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT"] = str(scratch_state / "snap-assets")
    env["JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT"] = str(scratch_state / "markdown-assets")
    env["JCHTOOLS_TEST_ASSET_ROOT"] = str(scratch_state / "markdown-assets")
    env["JCHTOOLS_TEST_BUNDLED_XBERG_DIR"] = str(bundled if bundled is not None else scratch_state / "absent-bundle")
    return env


def settings_stage_workspace() -> Path:
    """新设置阶段只在 make_tmp 管理的仓库 .tmp 中建立一次性数据."""
    # 本模块位于 scripts/gui_smoke_impl/，仓库根需再上一级。
    root = Path(__file__).resolve().parents[2] / ".tmp" / "gui-settings"
    guard_destination(root)
    root.mkdir(parents=True, exist_ok=True)
    return root


def plain_windows_path(text: str) -> str:
    """剥除 Windows verbatim 前缀（与 src/platform.rs display_path_text 同口径）.

    应用保存目录经 fs::canonicalize 落盘为带 verbatim 前缀的路径，界面显示则剥前缀；
    断言两侧必须先归一，否则同一目录会因表示形式不同而误判。
    """
    if text.startswith("\\\\?\\UNC\\"):
        return "\\\\" + text[len("\\\\?\\UNC\\") :]
    if text.startswith("\\\\?\\"):
        return text[len("\\\\?\\") :]
    return text
