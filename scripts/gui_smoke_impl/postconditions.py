"""产物与布局后置断言：整理/解压见证、转换产物、打包内容与两档窗口布局。.

由 gui_smoke.py 按域拆出；行为与拆分前一致，入口 scripts/gui_smoke.py 负责 re-export。
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import struct
import sys
import time
import zipfile
import zlib
from collections import Counter
from pathlib import Path
from typing import TYPE_CHECKING, cast

import win32con
import win32gui

if not __package__:
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

if TYPE_CHECKING:

    from pywinauto.application import WindowSpecification
    from pywinauto.base_wrapper import BaseWrapper

from scripts.gui_smoke_impl import config, convert_ui, driver


def tree_snapshot(root: Path) -> tuple[frozenset[str], dict[str, tuple[str, int]]]:
    """测试侧记录全部路径及字节/mtime，包含 Git 内部空目录，供整体平移与幂等判定。."""
    paths = frozenset(path.relative_to(root).as_posix() for path in root.rglob("*"))
    files = {
        path.relative_to(root).as_posix(): (file_digest(path), path.stat().st_mtime_ns)
        for path in root.rglob("*")
        if path.is_file()
    }
    return paths, files


def prepare_organize_witnesses(root: Path) -> dict[str, tuple[str, int]]:
    """覆盖 C-02/C-03/C-04/C-05/C-08/C-14：给工厂语料的一次性副本补充结果明确的见证。."""
    source = root / "__gui_witness"
    driver.require(not source.exists(), "测试语料已占用 GUI 见证目录")
    old_stamp, new_stamp = 1_700_000_000_000_000_000, 1_760_000_000_000_000_000
    # 期望由合同直接给定，不从计划库或产品归类函数反推结果。
    entries = (
        ("old/same.txt", "文档/same.txt", b"GUI same-name witness", old_stamp, False),
        ("new/same.txt", "文档/same.txt", b"GUI same-name witness", new_stamp, True),
        ("old/copy.txt", "文档/copy_2.txt", b"GUI copy-name witness", old_stamp, False),
        ("new/copy (2).txt", "文档/copy_2.txt", b"GUI copy-name witness", new_stamp, True),
        ("left/different-a.txt", "文档/different-a.txt", b"GUI different-name witness", old_stamp, True),
        ("right/different-b.txt", "文档/different-b.txt", b"GUI different-name witness", new_stamp, True),
        ("left/conflict.txt", "文档/left_conflict.txt", b"GUI conflict first", old_stamp, True),
        ("right/conflict.txt", "文档/right_conflict.txt", b"GUI conflict other", new_stamp, True),
        ("nested/witness.png", "图片/witness.png", b"GUI category image", old_stamp, True),
        ("nested/witness.mp4", "视频/witness.mp4", b"GUI category video", old_stamp, True),
        ("nested/witness.mp3", "音频/witness.mp3", b"GUI category audio", old_stamp, True),
        ("nested/witness.zip", "压缩包/witness.zip", b"GUI category archive", old_stamp, True),
        ("nested/witness.exe", "程序/witness.exe", b"GUI category program", old_stamp, True),
        ("nested/witness.unknown", "其他/witness.unknown", b"GUI category fallback", old_stamp, True),
    )
    expected: dict[str, tuple[str, int]] = {}
    for name, target, content, stamp, survives in entries:
        path = source / name
        path.parent.mkdir(parents=True, exist_ok=True)
        _ = path.write_bytes(content)
        os.utime(path, ns=(stamp, stamp))
        if survives:
            expected[target] = (file_digest(path), stamp)
    project = source / "project"
    for name, content in (
        (".git/marker", b"synthetic Git boundary"),
        ("Thumbs.db", b"Git internal junk must survive"),
        ("copy.txt", b"Git duplicate must survive"),
        ("copy (1).txt", b"Git duplicate must survive"),
    ):
        path = project / name
        path.parent.mkdir(parents=True, exist_ok=True)
        _ = path.write_bytes(content)
    (project / "empty").mkdir()
    return expected


def verify_organize_witnesses(
    root: Path,
    expected: dict[str, tuple[str, int]],
    git_before: tuple[frozenset[str], dict[str, tuple[str, int]]],
) -> None:
    """任务完成之外核对真实产物，拒绝未移动、错误去重、改写及 Git 内部清理。."""
    for name, (digest, stamp) in expected.items():
        path = root / name
        driver.require(path.is_file(), f"默认整理缺少合同期望产物：{name}")
        driver.require(file_digest(path) == digest, f"整理改变了文件内容：{name}")
        driver.require(path.stat().st_mtime_ns == stamp, f"去重保留者不是最新文件或归类改变了 mtime：{name}")
    expected_counts = Counter(digest for digest, _stamp in expected.values())
    actual_counts: Counter[str] = Counter()
    for path in root.rglob("*"):
        if path.is_file():
            digest = file_digest(path)
            if digest in expected_counts:
                actual_counts[digest] += 1
    driver.require(actual_counts == expected_counts, "默认去重的副本数量或不同名保留数量不符合合同")
    driver.require(not (root / "__gui_witness").exists(), "已搬空的 GUI 见证来源目录没有清理")
    driver.require(tree_snapshot(root / "Git项目集合" / "project") == git_before, "Git 项目未整体归类或内部被改动")


# 自动解压白名单（与 src/rules.rs::archive_name 同口径）：只有这些后缀会被解压。
# 其它格式（.cab/.iso/.wim/.lzh/.cpio/.docx/.msi 等）即使 7-Zip 能打开也不碰，
# 因此在核对里必须按「既有文件」处理——字节不变、不得消失、不得进隔离目录。
def file_digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def verify_original_disposition(root: Path, originals: dict[Path, str], quarantined: Counter[str]) -> None:
    """逐个原包核对：成功组的原包与分卷必须已删除且不得出现在「解压失败」，失败组必须完整隔离，既有文件必须原样保留."""
    successful_sections = {
        "07-压缩包-各格式",
        "08-压缩包-冲突",
        "09-压缩包-嵌套",
        "10-压缩包-超深",
        "13-压缩包-分卷",
        "14-大文件",
    }
    for relative, digest in originals.items():
        current = root / relative
        # X-09/X-10：数字尾卷必须属于白名单主包，编号为三位或自然扩展，
        # data.001、非白名单容器及多余前导零均是普通文件，不得要求删除或隔离。
        numeric_volume = (
            re.fullmatch(r"\.(?:[0-9]{3}|[1-9][0-9]{3,})", relative.suffix) is not None
            and relative.with_suffix("").suffix.lower() in config.ARCHIVE_SUFFIXES
        )
        is_archive = relative.suffix.lower() in config.ARCHIVE_SUFFIXES or numeric_volume
        if is_archive and relative.parts[0] in successful_sections:
            if current.exists():
                msg = f"完整成功后仍保留原包或分卷：{relative}"
                raise RuntimeError(msg)
            continue
        if current.is_file():
            if is_archive:
                msg = f"失败原包未隔离：{relative}"
                raise RuntimeError(msg)
            if file_digest(current) != digest:
                msg = f"解压修改了既有文件：{relative}"
                raise RuntimeError(msg)
            continue
        if not is_archive or quarantined[digest] == 0:
            msg = f"解压丢失失败原包、分卷或既有文件：{relative}"
            raise RuntimeError(msg)
        quarantined[digest] -= 1
    # X-05 与 X-06 是互斥的两种结果：「原包不在原位」不等于「按 X-05 永久删除」——
    # 被防护上限误伤而按 X-06 移入「解压失败」的原包，原位同样不存在。全部预期
    # 隔离项按 digest 配平后，「解压失败」不得再有剩余项：成功组的包若被误伤隔离，
    # 其 digest 不会出现在任何配平里，在此被拦下（否则 S4 对该类回归假绿，
    # 例如 max_ratio 上限被调小后 14-大文件 的全零包被整包推进「解压失败」）。
    leftovers = {digest: count for digest, count in quarantined.items() if count > 0}
    if leftovers:
        msg = f"「解压失败」存在无法对应到任何失败原包/分卷的多余隔离项（成功包可能被误伤隔离）：{leftovers}"
        raise RuntimeError(msg)


# 与 make_tmp.py 的 07 分区一致：同内容的单文件压缩流个数（gz / bz2 / xz / lzma）。
def verify_extracted_outputs(root: Path) -> None:
    """核对解压产物内容、分卷体积、复合压缩的中间层与嵌套原包清理."""
    expected = {
        "08-压缩包-冲突/说明 (1).txt": b"archive version B, different content and length\n",
        "08-压缩包-冲突/等长 (1).txt": b"fedcba9876543210",
        "09-压缩包-嵌套/level5.txt": b"innermost payload\n",
    }
    for relative, content in expected.items():
        if (root / relative).read_bytes() != content:
            msg = f"解压输出内容不符合测试语料：{relative}"
            raise RuntimeError(msg)
    # X-01：复合扩展名整体识别，中间 tar 层不得留在正式位置（`.tar.lzma` 这类
    # 引擎不自动拆 tar 的组合，靠把中间 tar 当嵌套包继续解开并清理）。
    leftovers = [str(path.relative_to(root)) for path in (root / "07-压缩包-各格式").rglob("*.tar")]
    if leftovers:
        msg = f"中间 tar 层不得留在正式位置：{leftovers}"
        raise RuntimeError(msg)
    # 同内容的单文件压缩流（gz/bz2/xz/lzma）都应解出；同名冲突自动改名，内容不变。
    payload = b"single member payload\n" * 8
    expected_copies = len(config.STREAM_PAYLOAD_CASES)
    copies = sum(
        1 for path in (root / "07-压缩包-各格式").rglob("*") if path.is_file() and path.read_bytes() == payload
    )
    if copies != expected_copies:
        msg = f"单文件压缩流应各解出一份内容（含 .lzma），实得 {copies} 份"
        raise RuntimeError(msg)
    if (root / "13-压缩包-分卷/big.bin").stat().st_size != 2 * 1024 * 1024 + 12345:
        msg = "分卷原包删除前必须完整解出 big.bin"
        raise RuntimeError(msg)
    for path in (root / "09-压缩包-嵌套").rglob("*"):
        if path.is_file() and path.suffix.lower() in config.ARCHIVE_SUFFIXES:
            msg = f"嵌套成功原包未清理：{path.relative_to(root)}"
            raise RuntimeError(msg)


def verify_extraction_results(root: Path, originals: dict[Path, str]) -> None:
    """核对成功源包删除、失败包完整隔离、既有文件不变与真实解压结果."""
    quarantined = Counter(file_digest(path) for path in (root / "解压失败").rglob("*") if path.is_file())
    # X-06：10-压缩包-超深 的第 17 层是按深度上限预期隔离的失败项。
    # 先严格核对这个有意保留的嵌套包，再从隔离计数中扣除一份；后续
    # verify_original_disposition 的 leftovers 检查仍会拒绝所有其他多余项。
    deep_failed = root / "解压失败" / "d17.zip"
    if not deep_failed.is_file():
        msg = "超深压缩包应按 X-06 隔离 d17.zip"
        raise RuntimeError(msg)
    try:
        with zipfile.ZipFile(deep_failed) as archive:
            members = archive.namelist()
    except (OSError, zipfile.BadZipFile) as exc:
        msg = "X-06 隔离的 d17.zip 不是有效压缩包"
        raise RuntimeError(msg) from exc
    if members != ["d18.zip"]:
        msg = f"X-06 隔离的 d17.zip 内容不符合预期：{members}"
        raise RuntimeError(msg)
    quarantined[file_digest(deep_failed)] -= 1
    verify_original_disposition(root, originals, quarantined)
    verify_extracted_outputs(root)


def snapshot_output_bytes(root: Path) -> dict[str, str]:
    """P-12：结果内容快照包含图片与临时文件，不能只数 Markdown。."""
    return {str(path.relative_to(root)): file_digest(path) for path in root.rglob("*") if path.is_file()}


def verify_cancelled_outputs(root: Path, completed: dict[str, str], in_flight_md: str) -> None:
    """覆盖 T-23/T-25：停止后保留已完成成品；当前文件至多保留停止落地前完成的那份.

    停止请求从发出到被引擎确认存在 UIA/调度时延：当前文件若在停止落地前已自然
    完成，其成品按 T-23「已完成产物保留」合法提交。因此允许集合为「已完成成品
    加至多一份当前文件成品」；后续文件、临时产物与未完成媒体目录仍然一律拒绝。
    """
    final = snapshot_output_bytes(root)
    extras = set(final) - set(completed)
    driver.require(not extras or extras == {in_flight_md}, f"停止后出现当前文件之外的产物或临时文件：{sorted(extras)}")
    lost = [name for name in completed if name not in final]
    driver.require(not lost, f"停止后已完成成品丢失：{sorted(lost)}")
    changed = [name for name in completed if name in final and final[name] != completed[name]]
    driver.require(not changed, f"停止后成品被修改：{changed}")
    driver.require(not any(path.is_dir() for path in root.rglob("*")), "取消后不得留下未完成媒体目录")


def optional_audio_member(relative: str) -> bool:
    path = relative.replace("\\", "/").lower()
    return path.startswith(("ffmpeg/", "models/vad/", "models/sense_voice_"))


def verify_bundled_package(root: Path) -> dict[str, str]:
    """核对真实基础包的收据及每个落盘成员；拒绝缺失/篡改与默认音频载荷."""
    receipt_path = root / "jchtools-xberg-release.json"
    receipt = cast("dict[str, object]", json.loads(receipt_path.read_text(encoding="utf-8")))
    driver.require(receipt.get("owner") == "JchTools-bundled-xberg-v1", "基础包缺少真实发布收据")
    driver.require(
        receipt.get("source") == "https://api.github.com/repos/jchanghong023/xberg/releases/latest",
        "基础包收据来源错误",
    )
    tag = receipt.get("tag")
    driver.require(isinstance(tag, str) and bool(tag), "基础包未锁定发布版本")
    components = cast("list[dict[str, object]]", receipt.get("components"))
    component_ids = {"document", "snapshot"}
    driver.require(
        isinstance(receipt.get("components"), list) and len(components) == len(component_ids),
        "基础包应只有 document/snapshot 两组件",
    )
    driver.require({component.get("id") for component in components} == component_ids, "基础包组件错误")
    expected: dict[str, tuple[int, str]] = {}
    for component in components:
        members = cast("list[dict[str, object]]", component.get("members"))
        driver.require(isinstance(component.get("members"), list) and bool(members), "基础包成员清单为空")
        for member in members:
            relative = cast("str", member.get("path"))
            driver.require(
                isinstance(member.get("path"), str)
                and "\\" not in relative
                and ":" not in relative
                and not relative.startswith("/")
                and all(part not in ("", ".", "..") for part in relative.split("/")),
                f"基础成员路径不安全：{relative!r}",
            )
            driver.require(not optional_audio_member(relative), f"基础收据混入音频成员：{relative}")
            identity = (cast("int", member.get("size_bytes")), cast("str", member.get("sha256")))
            driver.require(relative not in expected or expected[relative] == identity, f"共享成员身份冲突：{relative}")
            expected[relative] = identity
    driver.require("xberg.exe" in expected, "基础包缺少实际引擎")
    driver.require(any(path.startswith("sherpa-onnx/") for path in expected), "基础包缺少 sherpa 公共运行库")
    audio = cast("dict[str, object]", receipt.get("audio_assets"))
    driver.require(
        isinstance(receipt.get("audio_assets"), dict) and audio.get("id") == "audio-assets", "基础包未记录可选音频资产"
    )
    archive = "xberg-audio-assets-x86_64-pc-windows-msvc.zip"
    driver.require(
        audio.get("archive") == archive
        and audio.get("url") == f"https://github.com/jchanghong023/xberg/releases/download/{tag}/{archive}",
        "音频资产必须绑定基础包的精确发布",
    )
    for relative, (size, digest) in expected.items():
        member = root / relative
        driver.require(member.is_file() and not member.is_symlink(), f"基础成员缺失或被重定向：{relative}")
        driver.require(
            member.stat().st_size == size and file_digest(member) == digest,
            f"基础成员不符合收据：{relative}",
        )
    snapshot = snapshot_output_bytes(root)
    expected_names = {str(Path(relative)) for relative in expected} | {receipt_path.name}
    driver.require(
        set(snapshot) == expected_names,
        f"基础包文件集合不符合收据：{sorted(set(snapshot) ^ expected_names)}",
    )
    driver.require(
        not any(optional_audio_member(path.relative_to(root).as_posix() + "/") for path in root.rglob("*")),
        "默认基础包不得包含 FFmpeg / SenseVoice / VAD 目录或文件",
    )
    return snapshot


def _tiny_png(width: int = 16, height: int = 16) -> bytes:
    """构造最小合法灰度 PNG（无文字，转换成功即可，OCR 内容不作断言）."""
    raw = b"".join(b"\x00" + b"\xff" * width for _ in range(height))

    def chunk(kind: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 0, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw))
        + chunk(b"IEND", b"")
    )


LAYOUT_PAGES = (
    ("递归解压", ("开始解压",)),
    ("目录整理", ("开始分析",)),
    ("MD 整理", ("开始合并",)),
    ("转 Markdown", ("开始转换",)),
    ("Git 工具", ("开始",)),
    ("截图 OCR", ()),
    ("设置", (config.SETTINGS_DOWNLOAD_BUTTON, config.SETTINGS_CHECK_BUTTON)),
    ("关于", ()),
    ("模型服务", ("保存配置", "退出模型服务")),
    ("CodeAgent", ("保存间隔",)),
)


def verify_control_rectangles(bounds: tuple[int, int, int, int], rectangles: list[tuple[int, int, int, int]]) -> None:
    """覆盖 U-05/P-13：主要按钮应有正面积，处于窗口内且彼此不遮盖。."""
    left, top, right, bottom = bounds
    for index, (x1, y1, x2, y2) in enumerate(rectangles):
        driver.require(left <= x1 < x2 <= right and top <= y1 < y2 <= bottom, "主要操作控件被裁切或移出窗口")
        for a1, b1, a2, b2 in rectangles[:index]:
            driver.require(min(x2, a2) <= max(x1, a1) or min(y2, b2) <= max(y1, b1), "主要操作控件互相重叠")


def verify_layout_buttons(window: WindowSpecification, buttons: list[BaseWrapper]) -> None:
    """把真实 UIA 按钮矩形交给同一布局判据，页面和模态不另设放宽规则。."""
    bounds = window.rectangle()
    rectangles = [button.rectangle() for button in buttons]
    verify_control_rectangles(
        (bounds.left, bounds.top, bounds.right, bounds.bottom),
        [(rect.left, rect.top, rect.right, rect.bottom) for rect in rectangles],
    )


def _verify_layout_at_two_sizes(window: WindowSpecification) -> None:
    original = window.rectangle()
    sizes = (
        (original.right - original.left, original.bottom - original.top),
        (original.right - original.left + 200, original.bottom - original.top + 150),
    )
    optional_buttons = {"模型服务": ("应用并重启",), "CodeAgent": ("关闭提示",)}
    for width, height in sizes:
        win32gui.SetWindowPos(
            window.handle, 0, 0, 0, width, height, win32con.SWP_NOMOVE | win32con.SWP_NOZORDER | win32con.SWP_NOACTIVATE
        )
        time.sleep(0.5)
        for page, titles in LAYOUT_PAGES:
            driver.click(window, driver.find_button(window, page))
            if page == "截图 OCR":
                _ = convert_ui.wait_text_containing(window, "截图快捷键")
            elif page == "关于":
                _ = convert_ui.wait_text_containing(window, "关于 JchTools")
            buttons = [convert_ui.wait_button(window, title, config.TIMEOUT) for title in titles]
            for title in optional_buttons.get(page, ()):
                buttons.extend(convert_ui.unique_visible_buttons(window, title))
            verify_layout_buttons(window, buttons)
            if page == "模型服务":
                driver.click(window, convert_ui.wait_button(window, "退出模型服务", config.TIMEOUT, enabled=True))
                back = convert_ui.wait_button(window, "返回检查", config.TIMEOUT, enabled=True)
                confirm = convert_ui.wait_button(window, "确认退出模型服务", config.TIMEOUT, enabled=True)
                try:
                    verify_layout_buttons(window, [back, confirm])
                finally:
                    driver.click(window, back)
                _ = convert_ui.wait_button(window, "保存配置", config.TIMEOUT, enabled=True)
                _ = convert_ui.wait_button(window, "退出模型服务", config.TIMEOUT, enabled=True)


