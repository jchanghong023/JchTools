#!/usr/bin/env python3
"""JchTools 临时目录工厂：统一生成与清理本项目需要的所有 `.tmp/` 内容（自包含，不依赖仓库外目录）.

种类（后续新增临时数据需求时在此扩展）：
    testdata  手工测试数据集（嵌套包、冲突、垃圾文件、边界名称等），默认生成到
              <repo>/.tmp/testdata/，每次重建前先清空目标；`_测试说明.md` 逐条
              说明每个用例与预期行为。

用法：
    python scripts/make_tmp.py testdata [--git] [--destination <专门测试目录>] [--force]
    python scripts/make_tmp.py clean    # 清空整个 .tmp/ 释放磁盘；测试完成后执行，防止无限增长

clean 只删除仓库内 `.tmp/` 的内容，绝不触碰仓库其他位置与仓库外目录。
"""

from __future__ import annotations

import argparse
import bz2
import contextlib
import ctypes
import gzip
import hashlib
import itertools
import lzma
import os
import shutil
import stat
import subprocess
import sys
import tarfile
import time
import zipfile
from datetime import UTC, datetime
from io import BytesIO
from pathlib import Path
from typing import TYPE_CHECKING, Literal, NoReturn

if TYPE_CHECKING:
    from collections.abc import Callable

SEVEN_ZIP_CANDIDATES = [
    Path(r"C:\Program Files\7-Zip\7z.exe"),
    Path(r"C:\Program Files (x86)\7-Zip\7z.exe"),
]
MAKECAB = Path(os.environ.get("SYSTEMROOT", r"C:\Windows")) / "System32" / "makecab.exe"
# random_bytes 的流编号：每次调用递增，保证同尺寸多次调用内容不同；起点固定，全程可复现。
_STREAM_IDS = itertools.count()
PAYLOAD = b"JchTools duplicate payload 2026-09-12\n" * 4
DEEP_ZIP_LAYERS = 18  # 10-压缩包-超深 的嵌套层数（默认 max_depth=16，第 17 层起应停止解压）
UNC_SHARE_PARTS = 2  # \\server\share 去掉尾部反斜杠后恰好只剩两级即共享根
DRIVE_ANCHOR_MIN_LEN = 2  # 盘符锚点最短为「盘符+冒号」（如 C:\）


def fail(message: str) -> NoReturn:
    """以给定的中文错误信息立即退出脚本（等价于 raise SystemExit(message)，供各检查点复用）."""
    raise SystemExit(message)


def _line(*parts: str) -> str:
    """把多段长句拼成一行（行宽限制下拆行书写；生成内容与单行长串逐字符相同）."""
    return "".join(parts)


def force_remove_tree(path: Path) -> None:
    """删除目录树，先清掉只读属性（git 对象文件是只读的，Windows 上直接删会拒绝访问）.

    删除失败的路径会被收集并打印；清理后目录若仍存在也视为失败，避免在残留内容上重建。
    Windows 上先移除目录型 reparse point，避免 Python<3.12 的 rmtree 穿透 junction 删到目标外。
    """
    failed: list[str] = []

    def on_error(function: Callable[..., object], target: str, _error: BaseException) -> None:
        try:
            Path(target).chmod(stat.S_IWRITE)
            _ = function(target)
        except OSError as exc:
            failed.append(f"{target}（{exc}）")

    _remove_reparse_points(path, failed)
    # onexc 是 3.12+ 的 rmtree 回调参数；项目按 py313 目标运行，不再保留旧 onerror 分支。
    shutil.rmtree(path, onexc=on_error)
    if failed:
        print(f"清理失败：以下 {len(failed)} 个路径未能删除：", file=sys.stderr)
        for item in failed:
            print(f"  {item}", file=sys.stderr)
        fail(f"清理未完成，已中止以免在残留目录上重建测试数据：{path}")
    if path.exists():
        fail(f"清理后目录仍存在（可能有残留）：{path}")


def _remove_reparse_points(path: Path, failed: list[str]) -> None:
    """Windows 上先移除目录型 reparse point（junction/符号链接），避免 rmtree 穿透到目标外."""
    if os.name != "nt":
        return
    for dirpath, dirnames, _filenames in os.walk(path, topdown=False):
        for name in dirnames:
            p = Path(dirpath) / name
            try:
                st = os.lstat(p)
            except OSError:
                continue
            file_attributes = getattr(st, "st_file_attributes", 0)
            reparse_flag = getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0x400)
            reparse = bool(file_attributes & reparse_flag)
            if stat.S_ISLNK(st.st_mode) or reparse:
                try:
                    p.rmdir()
                except OSError as exc:
                    failed.append(f"{p}（无法移除 reparse point：{exc}）")


def measure_tree(root: Path) -> tuple[int, int]:
    """统计目录内文件数量与总字节数，用于清空前摘要."""
    count = 0
    size = 0
    for dirpath, _dirnames, filenames in os.walk(root):
        for name in filenames:
            count += 1
            with contextlib.suppress(OSError):
                size += (Path(dirpath) / name).stat().st_size
    return count, size


def seven_zip() -> Path:
    for candidate in SEVEN_ZIP_CANDIDATES:
        if candidate.is_file():
            return candidate
    found = shutil.which("7z")
    if found:
        return Path(found)
    fail("7-Zip not found; install the official full 7-Zip first.")


def run(command: list[str], cwd: Path | None = None) -> None:
    # 7-Zip 在本机按系统代码页输出，不能按 UTF-8 解码；出错时用替换字符展示。
    done = subprocess.run(command, cwd=cwd, capture_output=True, check=False)
    if done.returncode != 0:
        stdout = done.stdout.decode("utf-8", "replace") + done.stderr.decode("utf-8", "replace")
        fail(f"command failed ({done.returncode}): {' '.join(command)}\n{stdout}")


def write(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    _ = path.write_bytes(data)


def random_bytes(size: int) -> bytes:
    """生成 size 字节确定性伪随机数据（SHA-256 计数器流：可复现，且每次调用内容不同）."""
    stream = next(_STREAM_IDS)
    out = bytearray()
    position = 0
    while len(out) < size:
        block_input = stream.to_bytes(8, "little") + position.to_bytes(8, "little")
        out += hashlib.sha256(block_input).digest()
        position += 1
    return bytes(out[:size])


def zip_members(target: Path, members: dict[str, bytes]) -> None:
    target.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(target, "w", zipfile.ZIP_DEFLATED) as archive:
        for name, data in members.items():
            _ = archive.writestr(name, data)


def _add_tar_members(archive: tarfile.TarFile, members: list[Path], source: Path) -> None:
    for member in members:
        archive.add(member, arcname=member.relative_to(source).as_posix())


def _write_compressed_tar(
    tar_path: Path, mode: Literal["w:gz", "w:bz2", "w:xz"], members: list[Path], source: Path
) -> None:
    with tarfile.open(tar_path, mode) as archive:
        _add_tar_members(archive, members, source)


def build_plain(root: Path, _seven: Path, log: list[str]) -> None:
    section = root / "01-普通文件"
    write(section / "报告-2026.txt", "季度报告内容\n".encode())
    write(section / "我的 文档 (1).md", "# 标题\n\n带空格与括号的文件名\n".encode())
    write(section / "notes copy.txt", "季度报告内容\n".encode())  # 与 报告-2026.txt 同内容不同名
    write(section / "子目录" / "深层" / "leaf.bin", random_bytes(8 * 1024))
    log.append(
        _line(
            "| `01-普通文件/` | 基础扫描：中文名、空格、括号、同名不同内容 | 全部保留；",
            "`notes copy.txt` 与 `报告-2026.txt` 内容相同；不同名去重默认关闭，手动开启后才参与 |",
        )
    )


def build_duplicates(root: Path, _seven: Path, log: list[str]) -> None:
    section = root / "02-重复内容"
    write(section / "same-name.bin", PAYLOAD)
    write(section / "其它" / "same-name.bin", PAYLOAD)  # 同名同内容
    for name in ("dup.txt", "dup (1).txt", "dup - 副本.txt", "dup copy.txt"):
        write(section / name, PAYLOAD)  # 副本命名
    write(section / "完全无关的名字.bin", PAYLOAD)  # 不同名同内容
    log.append(
        _line(
            "| `02-重复内容/` | 三类去重规则各自命中（同名 / 副本命名 / 不同名） | ",
            "同名与副本名默认去重，不同名需手动开启；每组按「保留最新」留一个，其余默认永久删除 |",
        )
    )


def build_conflicts(root: Path, _seven: Path, log: list[str]) -> None:
    section = root / "03-版本冲突"
    old = section / "config.ini"
    write(old, b"mode=fast\n")
    fresh = section / "归档" / "config.ini"
    write(fresh, b"mode=safe\nthreads=8\nverify=yes\nlog=verbose\n")
    os.utime(old, (1_700_000_000, 1_700_000_000))
    os.utime(fresh, (1_760_000_000, 1_760_000_000))
    same_size_a = section / "same-size-a.dat"
    write(same_size_a, random_bytes(64))
    same_size_b = section / "归档" / "same-size-a.dat"
    write(same_size_b, random_bytes(64))
    stamp = 1_750_000_000
    os.utime(same_size_a, (stamp, stamp))
    os.utime(same_size_b, (stamp, stamp))
    log.append(
        _line(
            "| `03-版本冲突/` | 同名不同大小（config.ini）与同名同大小不同内容（same-size-a.dat） | ",
            "按 C-02 保留两份不同内容，不提供版本取舍开关；归类可以移动它们，但不得按同名自动淘汰 |",
        )
    )


def build_junk(root: Path, _seven: Path, log: list[str]) -> None:
    section = root / "04-垃圾与临时"
    write(section / "Thumbs.db", b"thumbs")
    write(section / "desktop.ini", b"[.ShellClassInfo]\n")
    write(section / "~$季度报告.docx", b"office lock file")
    write(section / "scratch.tmp", b"temp")
    write(section / "backup.bak", b"backup")
    write(section / "empty.txt", b"")
    log.append(
        _line(
            "| `04-垃圾与临时/` | 系统附属文件、临时/备份文件、零字节文件 | ",
            "系统附属文件规则默认开启，临时/备份与零字节规则默认关闭；各项独立设置删除方式，默认跟随全局永久删除 |",
        )
    )


def build_empty_and_chain(root: Path, _seven: Path, log: list[str]) -> None:
    section = root / "05-空目录与单链"
    (section / "空目录").mkdir(parents=True, exist_ok=True)
    write(section / "单链" / "a" / "b" / "c" / "payload.txt", b"chain payload\n")
    log.append(
        _line(
            "| `05-空目录与单链/` | 真空目录 + 只有一个子项的目录链 | 空目录清理应删除；",
            "开启「消除只有一个子项的目录层级」后 payload.txt 应被提升 |",
        )
    )


def build_formats(root: Path, _seven: Path, log: list[str]) -> None:
    section = root / "06-格式识别"
    png = bytes.fromhex(
        _line(
            "89504e470d0a1a0a0000000d494844520000000100000001080600000",
            "01f15c4890000000d4944415478da63f8ffff3f0300050001a5f645400000000049454e44ae426082",
        )
    )
    write(section / "其实是PNG.txt", png)
    buffer = root / "06-格式识别" / ".tmp-zip-source"
    zip_members(buffer / "inner.txt", {"inner.txt": b"real zip container\n"})
    _ = shutil.move(str(buffer / "inner.txt"), str(section / "其实是ZIP.dat"))
    shutil.rmtree(buffer, ignore_errors=True)
    docx = section / "真实文档.docx"
    with zipfile.ZipFile(docx, "w", zipfile.ZIP_DEFLATED) as archive:
        archive.writestr("[Content_Types].xml", "<Types/>")
        archive.writestr("word/document.xml", "<w:document/>")
    write(section / "没有扩展名的PDF", b"%PDF-1.7\n1 0 obj\n<<>>\nendobj\n")
    write(section / "未知格式.bin", random_bytes(256))
    log.append(
        _line(
            "| `06-格式识别/` | 扩展名与真实内容不一致 | ",
            "开启「检测真实类型 + 修正扩展名」后：`.txt`->`.png`、`.dat`->`.zip`；",
            "`.docx` 是 ZIP 容器但**不应**被改成 zip；",
            "未知格式与无扩展名 PDF 按规则处理 |",
        )
    )


def build_archives(root: Path, seven: Path, log: list[str]) -> None:
    section = root / "07-压缩包-各格式"
    source = root / ".build-07"
    write(source / "alpha.txt", b"alpha member\n")
    write(source / "beta.txt", b"beta member\n")
    write(source / "nested" / "gamma.txt", b"gamma member\n")
    members = sorted(p for p in source.rglob("*") if p.is_file())
    run([str(seven), "a", "-t7z", "-mx=7", "-y", str(section / "base.7z")] + [str(p) for p in members], cwd=source)
    zip_members(
        section / "base.zip",
        {"alpha.txt": b"alpha member\n", "beta.txt": b"beta member\n", "nested/gamma.txt": b"gamma member\n"},
    )
    with tarfile.open(section / "base.tar", "w") as archive:
        _add_tar_members(archive, members, source)
    _write_compressed_tar(section / "base.tar.gz", "w:gz", members, source)
    _write_compressed_tar(section / "base.tgz", "w:gz", members, source)
    _write_compressed_tar(section / "base.tar.bz2", "w:bz2", members, source)
    _write_compressed_tar(section / "base.tar.xz", "w:xz", members, source)
    single_payload = b"single member payload\n" * 8
    with gzip.open(section / "single.txt.gz", "wb") as stream:
        _ = stream.write(single_payload)
    with bz2.open(section / "single.txt.bz2", "wb") as stream:
        _ = stream.write(single_payload)
    with lzma.open(section / "single.txt.xz", "wb") as stream:
        _ = stream.write(single_payload)
    if MAKECAB.is_file():
        run(
            [
                str(MAKECAB),
                "/D",
                "CompressionType=LZX",
                "/D",
                "CompressionMemory=21",
                "alpha.txt",
                str(section / "data.cab"),
            ],
            cwd=source,
        )
    shutil.rmtree(source, ignore_errors=True)
    log.append(
        _line(
            "| `07-压缩包-各格式/` | 7z / zip / tar / tar.gz / tgz / tar.bz2 / tar.xz / gz / bz2 / xz / cab",
            "（本机 7-Zip 只支持解压 zst，没有构造 zst 用例） | ",
            "每个包都应解压成功后永久删除原包，同内容成员仍完整落盘；只有另行执行目录整理才会去重；失败记录原因 |",
        )
    )


def build_archive_conflicts(root: Path, _seven: Path, log: list[str]) -> None:
    section = root / "08-压缩包-冲突"
    write(section / "说明.txt", b"existing file, version A\n")  # 现有 26 B
    zip_members(section / "pack.zip", {"说明.txt": b"archive version B, different content and length\n"})
    write(section / "等长.txt", b"0123456789abcdef")  # 16 B
    zip_members(section / "等长冲突.zip", {"等长.txt": b"fedcba9876543210"})
    log.append(
        _line(
            "| `08-压缩包-冲突/` | 解压目标已存在同名文件（大小不同 / 大小相同） | ",
            "已有文件不变，新文件自动改名为「文件名 (1).原扩展名」，同内容也保留两份；",
            "不弹冲突策略，成功后永久删除原包 |",
        )
    )


def build_nested(root: Path, seven: Path, log: list[str]) -> None:
    section = root / "09-压缩包-嵌套"
    stage = root / ".build-09"
    write(stage / "level5.txt", b"innermost payload\n")
    with lzma.open(stage / "level5.txt.xz", "wb") as stream:
        _ = stream.write((stage / "level5.txt").read_bytes())
    zip_members(stage / "level4.zip", {"level5.txt.xz": (stage / "level5.txt.xz").read_bytes()})
    with tarfile.open(stage / "level3.tar.gz", "w:gz") as archive:
        archive.add(stage / "level4.zip", arcname="level4.zip")
    run([str(seven), "a", "-t7z", "-y", str(stage / "level2.7z"), str(stage / "level3.tar.gz")], cwd=stage)
    zip_members(section / "level1.zip", {"level2.7z": (stage / "level2.7z").read_bytes()})
    shutil.rmtree(stage, ignore_errors=True)
    log.append(
        _line(
            "| `09-压缩包-嵌套/` | zip -> 7z -> tar.gz -> zip -> xz 共 5 层 | ",
            "默认逐层解压到最内层文本，每层完整成功后永久删除该层原包；不提供关闭嵌套解压的开关 |",
        )
    )


def build_deep(root: Path, _seven: Path, log: list[str]) -> None:
    section = root / "10-压缩包-超深"
    payload = b"bottom of a very deep nest\n"
    for level in range(DEEP_ZIP_LAYERS, 0, -1):
        buffer: dict[str, bytes] = {}
        buffer[f"d{level + 1:02d}.zip" if level < DEEP_ZIP_LAYERS else "bottom.txt"] = payload
        archive = section / f"tmp{level:02d}.zip"
        zip_members(archive, buffer)
        payload = archive.read_bytes()
        archive.unlink()
    write(section / "d01.zip", payload)
    log.append(
        _line(
            "| `10-压缩包-超深/` | 18 层嵌套 zip（默认 `max_depth = 16`） | ",
            "第 17 层起应停止继续解压并记录，不会无限递归；把「最大嵌套层数」调大可继续 |",
        )
    )


def build_broken(root: Path, seven: Path, log: list[str]) -> None:
    section = root / "11-压缩包-损坏"
    stage = root / ".build-11"
    write(stage / "member.txt", b"member that will be lost\n" * 20)
    zip_members(stage / "good.zip", {"member.txt": (stage / "member.txt").read_bytes()})
    run([str(seven), "a", "-t7z", "-y", str(stage / "good.7z"), str(stage / "member.txt")], cwd=stage)
    for name in ("good.zip", "good.7z"):
        data = (stage / name).read_bytes()
        write(section / f"truncated{Path(name).suffix}", data[: int(len(data) * 0.6)])
    shutil.rmtree(stage, ignore_errors=True)
    log.append("| `11-压缩包-损坏/` | 被截断的 zip / 7z | 应报错并移入「解压失败」，不删除原包、不落盘未校验结果 |")


def build_encrypted(root: Path, seven: Path, log: list[str]) -> None:
    section = root / "12-压缩包-加密"
    stage = root / ".build-12"
    write(stage / "secret.txt", b"encrypted payload\n")
    # 直接用 7z 从临时目录创建加密 zip，不先写明文容器（避免中途留下未加密的 secret.zip）。
    run(
        [
            str(seven),
            "a",
            "-tzip",
            "-p123456",
            "-mem=ZipCrypto",
            "-y",
            str(section / "secret.zip"),
            str(stage / "secret.txt"),
        ],
        cwd=stage,
    )
    shutil.rmtree(stage, ignore_errors=True)
    log.append(
        _line(
            "| `12-压缩包-加密/` | 带密码的 zip（密码 123456） | ",
            "应用不提供密码输入，应移入「解压失败」并记录原因，不删除原包 |",
        )
    )


def build_multipart(root: Path, seven: Path, log: list[str]) -> None:
    section = root / "13-压缩包-分卷"
    stage = root / ".build-13"
    write(stage / "big.bin", random_bytes(2 * 1024 * 1024 + 12345))
    run(
        [str(seven), "a", "-t7z", "-mx=1", "-v1m", "-y", str(section / "multipart.7z"), str(stage / "big.bin")],
        cwd=stage,
    )
    shutil.rmtree(stage, ignore_errors=True)
    volumes = sorted(section.glob("multipart.7z.*"))
    log.append(
        _line(
            f"| `13-压缩包-分卷/` | 7z 分卷（{len(volumes)} 个卷，1 MiB/卷） | ",
            "应从识别到的首卷解出完整文件；完整成功后永久删除全部实际源卷 |",
        )
    )


def build_large(root: Path, seven: Path, log: list[str]) -> None:
    section = root / "14-大文件"
    write(section / "big-1.bin", random_bytes(32 * 1024 * 1024))
    write(section / "big-2.bin", random_bytes(32 * 1024 * 1024))
    stage = root / ".build-14"
    write(stage / "zeros.bin", bytes(64 * 1024 * 1024))
    run([str(seven), "a", "-t7z", "-mx=9", "-y", str(section / "zeros-64MiB.7z"), str(stage / "zeros.bin")], cwd=stage)
    shutil.rmtree(stage, ignore_errors=True)
    log.append(
        _line(
            "| `14-大文件/` | 两个同大小不同内容的 32 MiB 文件 + 一个 64 MiB 全零包 | ",
            "测 Hash 吞吐与「大文件单独归类」；全零包压缩比很高，用于确认展开比例上限不会误伤（默认上限 10000） |",
        )
    )


def build_hidden(root: Path, _seven: Path, log: list[str]) -> None:
    section = root / "15-隐藏与系统"
    hidden = section / "hidden-file.txt"
    write(hidden, b"hidden\n")
    system = section / "system-file.txt"
    write(system, b"system\n")
    if sys.platform == "win32":
        ctypes.windll.kernel32.SetFileAttributesW(str(hidden), 0x02)
        ctypes.windll.kernel32.SetFileAttributesW(str(system), 0x04)
    log.append(
        _line(
            "| `15-隐藏与系统/` | 带隐藏属性 / 系统属性的文件 | 默认两者都扫描；",
            "用户主动关闭相应开关时跳过，并明确提示范围缩小 |",
        )
    )


def build_hostile(root: Path, _seven: Path, log: list[str]) -> None:
    section = root / "16-恶意条目"
    section.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(section / "逃逸路径.zip", "w", zipfile.ZIP_DEFLATED) as archive:
        archive.writestr("../逃逸.txt", b"path traversal attempt\n")
        archive.writestr("/绝对路径.txt", b"absolute path attempt\n")
        archive.writestr("正常成员.txt", b"benign member\n")
    with tarfile.open(section / "符号链接.tar", "w") as archive:
        info = tarfile.TarInfo("link-to-etc")
        info.type = tarfile.SYMTYPE
        info.linkname = "/etc/passwd"
        info.size = 0
        archive.addfile(info)
        benign = tarfile.TarInfo("正常成员.txt")
        payload = b"benign member\n"
        benign.size = len(payload)
        archive.addfile(benign, BytesIO(payload))
    log.append(
        _line(
            "| `16-恶意条目/` | 含 `../`、绝对路径与符号链接的压缩包 | ",
            "绝对路径/越界条目应被拒绝或跳过并记录；含链接的包应整体拒绝（不会写出目录之外的文件） |",
        )
    )


RESTORE_SCRIPT = _line(
    """# 还原到生成时的基线：先恢复被删除/移动的文件，再清掉整理产生的新文件。
# 脚本内容保持 ASCII，避免 PowerShell 5.1 按 ANSI 读取时出错；路径都用通配符定位。
# 安全第一条：解析不到自身目录、或目录里没有 .git 时立刻退出，绝不把 git 命令打到别的仓库。
""",
    "$here = if ($PSScriptRoot) { $PSScriptRoot } elseif ($PSCommandPath) { Split-Path -Parent $PSCommandPath }",
    " else { Split-Path -Parent $MyInvocation.MyCommand.Path }\n",
    """if (-not $here) { Write-Error 'cannot resolve this script directory'; exit 1 }
# 分类整理可能把本脚本移进子目录，所以向上最多找 4 层；只认提交信息匹配本测试基线的仓库。
$root = $null
$probe = $here
for ($depth = 0; $depth -lt 5 -and $probe; $depth++) {
    if (Test-Path -LiteralPath (Join-Path $probe '.git')) {
        # 语料有两次提交（baseline + README），HEAD 永远是后者：必须全历史匹配，
        # 只看 log -1 会让一键还原必然失败。
        $subjects = (git -C $probe log --pretty=%s 2>$null)
        if ($subjects -like '*test corpus baseline*') { $root = $probe; break }
    }
    $probe = Split-Path -Parent $probe
}
if (-not $root) { Write-Error ('no JchTools test-corpus git baseline found above ' + $here); exit 1 }
Set-Location -LiteralPath $root
Write-Host ('restoring ' + $root)
git checkout -- .
git clean -fd
# git 不保存 Windows 属性和空目录，这里补回来。
$attrDir = Get-ChildItem -LiteralPath $root -Directory | Where-Object { $_.Name -like '15-*' } | Select-Object -First 1
if ($null -ne $attrDir) {
    Get-ChildItem -LiteralPath $attrDir.FullName -File | ForEach-Object {
        if ($_.Name -like 'hidden-*') { attrib +h $_.FullName | Out-Null }
        if ($_.Name -like 'system-*') { attrib +s $_.FullName | Out-Null }
    }
}
$chainDir = Get-ChildItem -LiteralPath $root -Directory | Where-Object { $_.Name -like '05-*' } | Select-Object -First 1
if ($null -ne $chainDir) {
    $empty = Get-ChildItem -LiteralPath $chainDir.FullName -Directory""",
    " | Where-Object { -not (Get-ChildItem -LiteralPath $_.FullName -Recurse -File -ErrorAction SilentlyContinue) }",
    " | Select-Object -First 1\n",
    """    if ($null -eq $empty) {
        # 空目录名与生成脚本保持一致（脚本带 BOM，中文可以安全写在这里）
        New-Item -ItemType Directory -Path (Join-Path $chainDir.FullName '空目录') | Out-Null
    }
}
Write-Host 'restored to baseline:'
git status --short
""",
)


def initialise_git(root: Path, log: list[str]) -> None:
    """建立可回滚的基线：字节稳定（* -text）、附还原脚本，并把两步都提交."""
    _ = (root / ".gitattributes").write_text(
        "# 测试数据需要字节可复现：不做换行转换，否则检出后哈希变化会影响去重测试。\n* -text\n",
        encoding="utf-8",
        newline="\n",
    )
    # 带 BOM 写入：PowerShell 5.1 否则会按 ANSI 解析中文注释，可能吞掉后续语句。
    _ = (root / "恢复.ps1").write_text(RESTORE_SCRIPT, encoding="utf-8-sig", newline="\n")
    run(["git", "init"], cwd=root)
    run(["git", "add", "-A"], cwd=root)
    # 内联身份提交：不依赖机器上全局/系统 git user.email / user.name 配置。
    run(
        [
            "git",
            "-c",
            "core.autocrlf=false",
            "-c",
            "user.email=test@local",
            "-c",
            "user.name=testdata",
            "commit",
            "-q",
            "-m",
            "test corpus baseline: 16 groups of generated JchTools test data",
        ],
        cwd=root,
    )
    log.append(
        _line(
            "| `.git` + `恢复.ps1` | 生成时建立的 git 基线（需 `--git`） | ",
            "根目录含 `.git` 时两个工具均拒绝整次处理；该模式仅用于验证 Git 保护，不用于普通处理验收 |",
        )
    )


def build_readme(root: Path, log: list[str], *, git: bool) -> None:
    text = [
        "# JchTools 手工测试数据集",
        "",
        f"生成时间：{datetime.now(UTC).astimezone().date().isoformat()}",
        _line(
            "生成脚本：`python scripts/make_tmp.py testdata`（可重复执行，会先清空本目录；",
            "默认生成到仓库 `.tmp/testdata/`）。",
        ),
        "所有内容都是脚本生成的假数据，可以用于移动和永久删除测试；不要指向真实资料。",
        "",
        "## 建议的测试顺序",
        "",
        _line(
            "1. 先用「递归解压」工具：选本目录 → 点「开始解压」→ 一段确认后跑完；",
            "完整成功后永久删除原包及分卷，冲突自动改文件名且后缀不变；未完全解开的包移入「解压失败」并记录原因；",
        ),
        "2. 再用「目录整理」工具：点「开始分析」（只读，不改文件），看计划里对每个重复组的判定是否符合下表；",
        "3. 逐组勾选/取消计划项，确认「确认并执行整理」只执行勾选项；",
        "4. 按需调整归类、临时/零字节清理及类型修正规则，重新分析并执行；默认已包含隐藏和系统属性资料；",
        "5. 关注底部进度条：解压/分析阶段是往返光带 + 实时计数，执行阶段是百分比 + 已完成/总数。",
        "",
        "## 各目录的用途与预期行为",
        "",
        "| 目录 | 构造内容 | 预期 |",
        "| --- | --- | --- |",
    ]
    text.extend(log)
    text.extend(
        [
            "",
            "## 注意",
            "",
            "- 全零压缩包解出来是 64 MiB 的 `zeros.bin`，分卷包解出来是 2 MiB 的 `big.bin`，执行整理前请确认磁盘空间；",
            "- 两工具的文件删除均为永久删除、不可恢复；解压仅删除完整成功的原包及分卷，不删除已有文件或普通解压结果；",
            "- `16-恶意条目/` 里的包是为安全测试准备的，请只在测试目录里使用；",
            "- 本说明文件本身也是被扫描的对象，不需要时可以直接删除。",
            "",
        ]
    )
    if git:
        text.extend(
            [
                "## 整理之后如何恢复",
                "",
                "本目录使用 `--git` 建立了根 Git 边界，应被两个工具整体拒绝。仅用于验证 Git 保护；恢复基线可执行：",
                "",
                "```powershell",
                ".\\恢复.ps1",
                "```",
                "",
                _line(
                    "等价的手工命令是 `git checkout -- .`（恢复被删除/移动的原始文件）",
                    "+ `git clean -fd`（清掉解压与归类产生的新文件）。",
                ),
                _line(
                    "git 不保存 Windows 隐藏/系统属性和空目录，所以 `恢复.ps1` 会额外补这两类；",
                    "目录里的 `.gitattributes`（`* -text`）保证检出后字节与初始一致，去重哈希才有可比性。",
                ),
                "想彻底重来，直接重跑生成脚本（会先清空目录）。",
            ]
        )
    else:
        text.extend(
            [
                "## 整理之后如何恢复",
                "",
                "本次生成没有使用 `--git`，目录里没有 git 基线和 `恢复.ps1`，整理后的删除/移动无法一键撤销。",
                _line(
                    "普通处理验收保持不带 `--git`；需要还原时重新执行 `python scripts/make_tmp.py testdata --force`",
                    "（默认 `.tmp/testdata/`，会先清空目录重建）。",
                ),
            ]
        )
    _ = root.joinpath("_测试说明.md").write_text("\n".join(text) + "\n", encoding="utf-8")


def _is_drive_alias(resolved: Path) -> bool:
    r"""Windows 下检测盘符是否为 subst/网络映射别名；真实本地卷返回 False，非 Windows 恒 False.

    resolve() 不会把映射盘符解析回真实目标（subst X: C:\Users\... 会原样保留 X:\），
    主目录检查对这类路径全部失效，只能用 QueryDosDeviceW 看真实设备名：
    真实本地卷是 \Device\HarddiskVolumeN，subst 映射是 \??\C:\...，
    网络映射是 \Device\LanmanRedirector\...；只有真实本地卷放行。
    """
    # os.name 与 sys.platform 在 Windows 上同真；带上 sys.platform 便于类型收窄到 windll。
    if os.name != "nt" or sys.platform != "win32":
        return False
    anchor = resolved.anchor
    if len(anchor) < DRIVE_ANCHOR_MIN_LEN or anchor[1] != ":":
        return False  # UNC（\\\\server\\...）与无盘符路径不走此检查
    try:
        buffer = ctypes.create_unicode_buffer(1024)
        # 查询失败（含盘符不存在）按可疑处理，宁可拒绝。
        if ctypes.windll.kernel32.QueryDosDeviceW(ctypes.c_wchar_p(anchor[:2]), buffer, 1024) == 0:
            return True
        # Array.value 在类型存根中是 Any；改为按 UTF-16-LE 解码整块缓冲区并截到首个 NUL，
        # 与 .value 的取值语义逐字符相同。
        device_name = bytes(buffer).decode("utf-16-le").split(chr(0), 1)[0]
        return not device_name.startswith("\\Device\\HarddiskVolume")
    except (OSError, AttributeError, ctypes.ArgumentError):
        return True


def guard_destination(root: Path) -> None:
    """清空不可逆：拒绝盘符根、UNC 共享根、用户主目录、系统目录，以及会波及本仓库（.tmp 之外）的目标."""
    # 扩展路径前缀（\\?\ 与 \\.\）不会被 resolve() 规范化，会让下面的全部检查失效，直接拒绝。
    raw = str(root.expanduser())
    if raw.startswith(("\\\\?\\", "\\\\.\\")):
        fail(f"不支持扩展路径前缀（\\\\?\\ / \\\\.\\）：{root}")
    resolved = root.expanduser().resolve()
    if _is_drive_alias(resolved):
        fail(f"拒绝清空映射/别名盘符（subst 或网络映射）：{resolved}")
    _reject_unc_share_root(resolved)
    # 盘符根/POSIX 根：C:\ 的 parts 形如 ('C:\\',)，parent 等于自身。
    # 新版 pathlib 对 UNC 共享根也把 parent 视为自身，此检查一并覆盖。
    if resolved == resolved.parent:
        fail(f"拒绝清空盘符根/文件系统根：{resolved}")
    _reject_repo_and_home(resolved)
    _reject_system_directories(resolved)
    _reject_other_user_dirs(resolved)
    _reject_system_names(resolved)


def _reject_unc_share_root(resolved: Path) -> None:
    """拒绝 UNC 共享根与 Windows 管理共享（resolve 后按字符串判定，兼容 pathlib 版本差异）."""
    # UNC 共享根（\\server\share）：pathlib 版本间对 UNC 根的 parts 表示不一致，
    # 直接按字符串判定——去掉尾部反斜杠后恰好只剩 server\share 两级则拒绝；
    # \\server\share\sub 多一层，可放行。此检查先于盘符根判断，确保报错信息准确。
    norm = str(resolved).replace("/", "\\").rstrip("\\")
    if not norm.startswith("\\\\"):
        return
    comps = [c for c in norm.split("\\") if c]
    if len(comps) == UNC_SHARE_PARTS:
        fail(f"拒绝清空 UNC 共享根：{resolved}")
    # 管理共享（\\localhost\c$、\\127.0.0.1\d$、\\机器名\admin$ 等）不 resolve 成
    # 本地盘形态，会绕过 home/Users/Public 检查。直接拒绝，避免清空用户数据区。
    # 以 `$` 结尾的共享名覆盖 c$/print$/fax$/admin$ 等全部管理共享。
    share = comps[1].lower() if len(comps) >= UNC_SHARE_PARTS else ""
    if share.endswith("$"):
        fail(f"拒绝清空 Windows 管理共享（admin share）：{resolved}")


def _reject_repo_and_home(resolved: Path) -> None:
    """拒绝仓库及其上级（仓库内只放行 .tmp/ 下），以及用户主目录及其上级/下级."""
    home = Path.home().resolve()
    repo = Path(__file__).resolve().parent.parent
    if resolved == repo or resolved in repo.parents:
        fail(f"拒绝清空仓库或其上级目录：{resolved}")
    # 仓库内测试数据只允许放在 .tmp/（AGENTS.md 第2节）。命中 .tmp 后显式放行，
    # 仓库若位于 home 下也不得被下面的 home 规则误杀。
    under_repo_tmp = False
    if repo in resolved.parents:
        if repo / ".tmp" not in (resolved, *resolved.parents):
            fail(f"仓库内只允许把测试数据放到 .tmp/ 下：{resolved}")
        under_repo_tmp = True
    # home 本身、home 的上级（如 C:\Users）与 home 的下级（如桌面/文档）都在清空波及
    # 用户真实数据的范围内，一并拒绝；测试集只应放在专门的测试目录。
    if not under_repo_tmp and (resolved == home or resolved in home.parents or home in resolved.parents):
        fail(f"拒绝清空用户主目录及其上级/下级目录：{resolved}")


def _reject_system_directories(resolved: Path) -> None:
    r"""拒绝系统目录黑名单：SystemRoot、ProgramData、Program Files 系列、Users\Public."""
    system_root = Path(os.environ.get("SYSTEMROOT", r"C:\Windows")).resolve()
    if resolved == system_root or system_root in resolved.parents:
        fail(f"拒绝清空 Windows 系统目录（SystemRoot）：{resolved}")
    public = Path(os.environ.get("PUBLIC", r"C:\Users\Public")).resolve()
    if resolved == public or public in resolved.parents:
        fail(f"拒绝清空公共用户目录（Users\\Public）：{resolved}")
    program_data = Path(os.environ.get("PROGRAMDATA", r"C:\ProgramData")).resolve()
    if resolved == program_data or program_data in resolved.parents:
        fail(f"拒绝清空 ProgramData 目录：{resolved}")


def _reject_other_user_dirs(resolved: Path) -> None:
    """拒绝其他用户主目录：Users 下除当前用户外的任何账户目录."""
    home = Path.home().resolve()
    users_root = home.parent
    if resolved != home and users_root in resolved.parents:
        other_user = resolved.relative_to(users_root).parts[0]
        if other_user and other_user != home.name:
            fail(f"拒绝清空其他用户目录（{other_user}）：{resolved}")


def _reject_system_names(resolved: Path) -> None:
    """拒绝路径中任何名为 Windows（含 Windows.old）或以 Program Files 开头的目录."""
    for part in (resolved, *resolved.parents):
        # resolve() 会按磁盘实际大小写规范化目录名，比较必须忽略大小写。
        name = part.name.casefold()
        # startswith("windows") 覆盖 Windows 与 Windows.old 等变体。
        if name.startswith(("windows", "program files")):
            fail(f"拒绝清空系统/程序目录（{part.name}）：{resolved}")


def robust_rmtree(path: Path) -> None:
    r"""Windows 健壮删除：超长路径加 \\?\ 前缀，只读文件先清只读位，目录删除竞态短暂重试.

    细节：git 对象等只读文件删除报 WinError 5；目录删除竞态报 WinError 145；
    深层嵌套路径总长可超 MAX_PATH，删除中途会“找不到路径”而留下深层尾巴.
    """

    def on_error(func: Callable[..., object], target: str, _exc_info: BaseException) -> None:
        last: BaseException | None = None
        for attempt in range(3):
            try:
                Path(target).chmod(stat.S_IWRITE)
                _ = func(target)
            except OSError as error:
                last = error
                time.sleep(0.1 * (attempt + 1))
            else:
                return
        if last is None:
            detail = "last is not None"
            raise AssertionError(detail)
        raise last

    # 长路径前缀交给系统按扩展长度路径处理。仅内部清理使用；
    # 用户输入的扩展前缀仍由 guard_destination 直接拒绝。
    # 前缀必须加在根上：rmtree 的子路径由根拼接而来，中途遇到超长子路径再补就晚了。
    target: str | Path = path
    if os.name == "nt":
        target = r"\\?\\" + str(path.resolve())
    shutil.rmtree(target, onexc=on_error)


def clean_tmp(repo_root: Path) -> int:
    """清空仓库内 .tmp/ 全部内容（一次性中间产物，测试完成后清理，防止磁盘无限增长）."""
    tmp = (repo_root / ".tmp").resolve()
    if tmp.name != ".tmp" or tmp.parent != repo_root:
        fail(f"安全检查失败：目标不是仓库内 .tmp/（{tmp}）")
    if not tmp.is_dir():
        print(f".tmp 不存在，无需清理：{tmp}")
        return 0
    count, total = measure_tree(tmp)
    failures: list[tuple[Path, str]] = []
    for entry in tmp.iterdir():
        try:
            if entry.is_dir() and not entry.is_symlink():
                robust_rmtree(entry)
            else:
                entry.chmod(stat.S_IWRITE)
                entry.unlink()
        except FileNotFoundError:
            pass
        except OSError as error:
            failures.append((entry, str(error)))
    if failures:
        for path, error in failures:
            print(f"删除失败：{path}：{error}")
        fail(f"共 {len(failures)} 个条目删除失败（见上方原因）；请处理后重跑 clean。")
    print(f"已清空 {tmp}：删除 {count} 个文件，释放约 {total / 1048576:.1f} MiB。")
    return 0


class _Arguments(argparse.Namespace):
    """显式声明各参数类型的命令行容器（argparse 会把解析结果 setattr 到该实例上）.

    类属性默认值与 argparse 的默认行为一一对应（位置参数必有值、
    --destination 默认 None、store_true 默认 False），因此不影响运行结果。
    """

    kind: str = ""
    destination: str | None = None
    git: bool = False
    force: bool = False


def _clear_existing_target(root: Path, *, force: bool) -> None:
    """目标已存在时：非目录直接拒绝；非空时统计并（按需确认）清空."""
    if not root.exists():
        return
    if not root.is_dir():
        fail(f"目标不是目录：{root}；请改用专门的测试目录。")
    entries = list(root.iterdir())
    if entries:
        count, total = measure_tree(root)
        print(f"目标目录非空：{root}")
        print(f"  将删除 {count} 个文件，合计 {total / 1048576:.1f} MiB（顶层 {len(entries)} 个条目）")
        if not force:
            if not sys.stdin.isatty():
                fail("目标目录非空；非交互环境请加 --force 确认清空。")
            try:
                answer = input("确认清空以上内容？输入 yes 继续：")
            except (EOFError, KeyboardInterrupt):
                fail("已取消：未清空目标目录。")
            if answer.strip().lower() != "yes":
                fail("已取消：未清空目标目录。")
    print(f"clearing {root} ...")
    force_remove_tree(root)


def main() -> int:
    repo_root = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description="JchTools 临时目录工厂：生成与清理 .tmp/ 内容。")
    _ = parser.add_argument(
        "kind",
        choices=["testdata", "clean"],
        help="testdata=生成手工测试数据集；clean=清空整个 .tmp/ 释放磁盘",
    )
    _ = parser.add_argument(
        "--destination",
        default=None,
        help="testdata 专用：另指定测试目录（默认 <repo>/.tmp/testdata，会先清空）",
    )
    _ = parser.add_argument("--git", action="store_true", help="testdata 专用：建立 git 基线并生成 恢复.ps1")
    _ = parser.add_argument(
        "--force",
        action="store_true",
        help="testdata 专用：目标目录非空时免交互确认清空",
    )
    arguments = parser.parse_args(namespace=_Arguments())
    kind = arguments.kind
    force = arguments.force
    use_git = arguments.git
    destination = arguments.destination
    if kind == "clean":
        return clean_tmp(repo_root)
    root = Path(destination) if destination else repo_root / ".tmp" / "testdata"
    guard_destination(root)
    # guard 内部用 resolve() 检查，但构建全程用的是原始路径；相对路径在脚本切换
    # 工作目录后会指向不存在的位置（已清空目标却构建失败），这里统一转成绝对路径。
    root = root.expanduser().resolve()
    seven = seven_zip()

    _clear_existing_target(root, force=force)
    root.mkdir(parents=True)

    log: list[str] = []
    for builder in (
        build_plain,
        build_duplicates,
        build_conflicts,
        build_junk,
        build_empty_and_chain,
        build_formats,
        build_archives,
        build_archive_conflicts,
        build_nested,
        build_deep,
        build_broken,
        build_encrypted,
        build_multipart,
        build_large,
        build_hidden,
        build_hostile,
    ):
        builder(root, seven, log)
        print(f"  built {builder.__name__}")
    if use_git:
        initialise_git(root, log)
    build_readme(root, log, git=use_git)
    # README 在 baseline 之后生成：必须再提交一次，否则 恢复.ps1 的 git clean -fd 会删掉说明。
    if use_git:
        # 与 baseline 提交同款内联身份：不依赖机器上全局/系统 git user.email / user.name 配置。
        run(["git", "-C", str(root), "add", "-A"])
        run(
            [
                "git",
                "-C",
                str(root),
                "-c",
                "core.autocrlf=false",
                "-c",
                "user.email=test@local",
                "-c",
                "user.name=testdata",
                "commit",
                "-m",
                "test corpus readme",
            ]
        )

    files = [p for p in root.rglob("*") if p.is_file() and ".git" not in p.parts]
    total = sum(p.stat().st_size for p in files)
    print(f"corpus: {root}")
    print(f"files: {len(files)}  size: {total / 1048576:.1f} MiB")
    return 0


if __name__ == "__main__":
    sys.exit(main())
