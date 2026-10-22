"""临时目录工厂底层助手：文件系统清理、reparse 安全、压缩与外部命令（自 make_tmp.py 原样搬移）."""

from __future__ import annotations

import contextlib
import hashlib
import itertools
import os
import shutil
import stat
import subprocess
import sys
import tarfile
import time
import zipfile
from pathlib import Path
from typing import TYPE_CHECKING, Literal, NoReturn

if TYPE_CHECKING:
    from collections.abc import Callable

SEVEN_ZIP_CANDIDATES = [
    Path(r"C:\Program Files\7-Zip\7z.exe"),
    Path(r"C:\Program Files (x86)\7-Zip\7z.exe"),
]
# random_bytes 的流编号：每次调用递增，保证同尺寸多次调用内容不同；起点固定，全程可复现。
_STREAM_IDS = itertools.count()


def fail(message: str) -> NoReturn:
    """以给定的中文错误信息立即退出脚本（等价于 raise SystemExit(message)，供各检查点复用）."""
    raise SystemExit(message)


def line(*parts: str) -> str:
    """把多段长句拼成一行（行宽限制下拆行书写；生成内容与单行长串逐字符相同）."""
    return "".join(parts)


def force_remove_tree(path: Path) -> None:
    """删除目录树；遇到根或内部 reparse point 时只移除链接本身，不遍历其目标."""
    root_reparse = reparse_point_status(path)
    if root_reparse is not None:
        remove_reparse_point(path, root_reparse)
        return

    failed: list[str] = []

    def on_error(function: Callable[..., object], target: str, _error: BaseException) -> None:
        try:
            Path(target).chmod(stat.S_IWRITE)
            _ = function(target)
        except OSError as exc:
            failed.append(f"{target}（{exc}）")

    _remove_reparse_points(path, failed)
    if failed:
        print(f"清理失败：以下 {len(failed)} 个路径无法安全移除：", file=sys.stderr)
        for item in failed:
            print(f"  {item}", file=sys.stderr)
        fail(f"清理未完成；为避免越过 reparse point，已中止递归删除：{path}")
    # onexc 是 3.12+ 的 rmtree 回调参数；项目按 py313 目标运行，不再保留旧 onerror 分支。
    shutil.rmtree(path, onexc=on_error)
    if failed:
        print(f"清理失败：以下 {len(failed)} 个路径未能删除：", file=sys.stderr)
        for item in failed:
            print(f"  {item}", file=sys.stderr)
        fail(f"清理未完成，已中止以免在残留目录上重建测试数据：{path}")
    if path.exists():
        fail(f"清理后目录仍存在（可能有残留）：{path}")


def reparse_point_status(path: Path) -> os.stat_result | None:
    """Windows 下返回 reparse point 的 lstat；不解析链接目标."""
    if os.name != "nt":
        return None
    st = os.lstat(path)
    file_attributes = getattr(st, "st_file_attributes", 0)
    reparse_flag = getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0x400)
    return st if stat.S_ISLNK(st.st_mode) or file_attributes & reparse_flag else None


def remove_reparse_point(path: Path, st: os.stat_result) -> None:
    """只解除 reparse point 本身，不递归删除或解析其目标."""
    file_attributes = getattr(st, "st_file_attributes", 0)
    directory_flag = getattr(stat, "FILE_ATTRIBUTE_DIRECTORY", 0x10)
    if stat.S_ISDIR(st.st_mode) or file_attributes & directory_flag:
        path.rmdir()
    else:
        path.unlink()


def _remove_reparse_points(path: Path, failed: list[str]) -> None:
    """Windows 上移除目录型 reparse point 并剪枝，避免遍历或删除其目标."""
    if os.name != "nt":
        return
    for dirpath, dirnames, _filenames in os.walk(path, topdown=True):
        for name in list(dirnames):
            p = Path(dirpath) / name
            try:
                st = reparse_point_status(p)
            except FileNotFoundError:
                dirnames.remove(name)
                continue
            except OSError as exc:
                dirnames.remove(name)
                failed.append(f"{p}（无法检查 reparse point：{exc}）")
                continue
            if st is not None:
                dirnames.remove(name)
                try:
                    remove_reparse_point(p, st)
                except OSError as exc:
                    failed.append(f"{p}（无法移除 reparse point：{exc}）")


def measure_tree(root: Path) -> tuple[int, int]:
    """统计目录内文件数量与总字节数，不跟随 reparse point 或符号链接."""
    count = 0
    size = 0
    for dirpath, dirnames, filenames in os.walk(root, topdown=True):
        if os.name == "nt":
            for name in list(dirnames):
                try:
                    reparse = reparse_point_status(Path(dirpath) / name)
                except OSError:
                    dirnames.remove(name)
                    continue
                if reparse is not None:
                    dirnames.remove(name)
        for name in filenames:
            count += 1
            with contextlib.suppress(OSError):
                size += (Path(dirpath) / name).lstat().st_size
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


def add_tar_members(archive: tarfile.TarFile, members: list[Path], source: Path) -> None:
    for member in members:
        archive.add(member, arcname=member.relative_to(source).as_posix())


def write_compressed_tar(
    tar_path: Path, mode: Literal["w:gz", "w:bz2", "w:xz"], members: list[Path], source: Path
) -> None:
    with tarfile.open(tar_path, mode) as archive:
        add_tar_members(archive, members, source)


def robust_rmtree(path: Path) -> None:
    r"""Windows 健壮删除；根或内部 reparse point 只移除链接本身，不触碰目标."""

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

    if os.name == "nt":
        root_reparse = reparse_point_status(path)
        if root_reparse is not None:
            remove_reparse_point(path, root_reparse)
            return
        failed: list[str] = []
        _remove_reparse_points(path, failed)
        if failed:
            raise OSError("无法安全移除 reparse point：" + "；".join(failed))

    # 长路径前缀交给系统按扩展长度路径处理。使用 abspath 而非 resolve，
    # 避免解析根路径中的 junction 后递归删除链接目标。
    target: str | Path = path
    if os.name == "nt":
        target = r"\\?\\" + os.path.abspath(path)  # noqa: PTH100  # 有意不 resolve：解析 junction 会把删除引导到链接目标
    shutil.rmtree(target, onexc=on_error)
