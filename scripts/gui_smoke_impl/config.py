"""共享常量：超时、阶段清单、确认文案与隔离环境键。.

由 gui_smoke.py 按域拆出；行为与拆分前一致，入口 scripts/gui_smoke.py 负责 re-export。
"""

from __future__ import annotations

import sqlite3
import sys
from pathlib import Path

if not __package__:
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))


TIMEOUT = 60
# 分析/执行完成等待用更长上限：真实数据、冷启动与杀软扫描都会让真实耗时远离秒级。
# 只放宽「等结果」的上限，元素出现仍按 TIMEOUT 快速失败，避免掩盖界面迟迟不响应的问题。
COMPLETION_TIMEOUT = 240
# 目录输入框与「选择目录…」按钮视为同一行的纵坐标容差（像素）。
EDIT_ROW_TOLERANCE_PX = 20
PHYSICAL_OVERLAP_THRESHOLD = 0.8
CONFIRM_CLOSED_OBSERVATIONS = 2
MD_SPLIT_LIMIT_BYTES = 1024
STOP_MEDIA_FILE_COUNT = 16
DEFAULT_EXE = "target/debug/JchTools.exe"
DEFAULT_DATA = ".tmp/gui-smoke/data"
# 冒烟阶段清单：S1-S4 无需转 Markdown 资产；S5 需要（默认序列不含 S5，须显式 --stages 请求）；
# S6-S8 需要真实预置的无音频基础包；S9 自建缺失包环境；S10-S14 复用验收环境的真实组件。
SUPPORTED_STAGES = (
    "S1",
    "S2",
    "S3",
    "S4",
    "S5",
    "S6",
    "S7",
    "S8",
    "S9",
    "S10",
    "S11",
    "S12",
    "S13",
    "S14",
    "S15",
    "S16",
    "S17",
    "S18",
)
DEFAULT_STAGES = "S1,S2,S3,S4,S15,S16,S18"
CONVERT_BUSY_TIMEOUT = 60  # 点击「开始转换」后等待「停止任务」出现的上限（秒）
S5_LAST_ATTEMPT = 2  # S5 重按「开始转换」的末次序号（共 3 次，0 起）
CONVERT_STOP_TIMEOUT = 15  # T-23：停止不得等待当前媒体自然结束。
# 转 Markdown 页「选择目录…」应有行数（输入 / 输出；组件下载统一在设置页）。
CONVERT_DIR_ROWS = 2
EXTRACT_ACK = "我已确认：成功原包及分卷永久删除（不可恢复）"
ORGANIZE_ACK = "我已确认目录、规则及可能的永久删除行为（不可恢复）"
# 设置/转换页状态文案锚点（与 ui/app.slint 的文案保持同步，改动必须两侧同改）。
SETTINGS_DOWNLOAD_BUTTON = "下载 / 补齐音频转写资产"
SETTINGS_RETRY_BUTTON = "重试音频资产下载"
SETTINGS_CHECK_BUTTON = "检查已安装组件"
SETTINGS_BASE_LABELS = ("文档转换与图片 OCR", "截图 OCR")
CONVERT_NEED_RUNTIME_TEXT = "请前往设置检查随包基础资产；音频转写资产需主动下载"
CONVERT_DONE_MARKER = "总耗时"
STOP_AND_CLOSE_TITLE = "停止任务并关闭"
STOP_AND_CLOSE_BUTTON = "停止并关闭"
# 统一隔离守卫：所有非自隔离阶段的 run_stage 不自建隔离环境（不同于 S6-S9 的
# isolated_state_env），独立调用会直接读写真实用户状态目录（config.sqlite3）并
# 可能连接/唤起常驻服务（已有配置可能触发 ensure_snap_supervisor）。因此要求经验收
# 编排布置的隔离环境（下述两个变量由 acceptance.ps1 的 gui-smoke / markdown 分支
# 统一布置）才可启动；--allow-isolated-run 不绕过公共启动入口。Xberg 目录可指向真实
# 引擎目录——状态隔离必须，组件目录可真实。
ISOLATION_GUARDED_STAGES = tuple(stage for stage in SUPPORTED_STAGES if stage not in ("S6", "S7", "S8", "S9"))
ISOLATION_REQUIRED_ENV_KEYS = ("JCHTOOLS_TEST_STATE_DIR", "JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT")

# pywinauto/pywin32 窗口操作在窗口建立/销毁竞态下抛出的瞬态错误族；
# 此元组是唯一放行集合，出现新瞬态类型须显式补充并说明：
# - pywintypes.error / pywintypes.com_error：pywin32 的 Win32 调用与 COM 调用基础错误；
# - comtypes.COMError：UIA 后端经 comtypes 访问 COM 元素，窗口消失时立即抛出；
# - ProcessNotFoundError：Application().connect() 在进程不存在/已退出时抛出；
# - timings.TimeoutError：window.wait() 等待条件超时抛出（继承 RuntimeError，与上无交集）；
# - findwindows.ElementNotFoundError / findbestmatch.MatchError /
#   controls.InvalidWindowHandle / controls.InvalidElement：pywinauto 解析控件规格失败
#   的四种错误（WindowSpecification.__getattribute__ 解析包装器时抛出，
#   与 window.exists()/wait() 内部捕获的是同一组）。
TASK_RECORD_ERRORS: tuple[type[Exception], ...] = (sqlite3.Error, ValueError)


ARCHIVE_SUFFIXES = (
    ".zip",
    ".7z",
    ".rar",
    ".tar",
    ".gz",
    ".bz2",
    ".xz",
    ".zst",
    ".lzma",
    ".z",
    ".tgz",
    ".tbz2",
    ".txz",
    ".tzst",
)


STREAM_PAYLOAD_CASES = ("single.txt.gz", "single.txt.bz2", "single.txt.xz", "single.txt.lzma")


ATTEMPT_TIMEOUT = 8  # 单次「点击→等文本」的观察窗（秒）；落空即重试


S11_SOURCE_FILES = ("one.txt", "two.txt")
