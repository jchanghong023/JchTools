"""临时目录工厂目标守卫：拒绝危险清空目标（盘符根/UNC/系统目录/别名盘符等）（自 make_tmp.py 原样搬移）."""

from __future__ import annotations

import ctypes
import os
import sys
from pathlib import Path

from scripts.make_tmp_impl.fsutil import _reparse_point_status, fail

UNC_SHARE_PARTS = 2  # \\server\share 去掉尾部反斜杠后恰好只剩两级即共享根
DRIVE_ANCHOR_MIN_LEN = 2  # 盘符锚点最短为「盘符+冒号」（如 C:\）


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
        return False  # UNC（\\server\...）与无盘符路径不走此检查
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
    if raw.startswith(("\\\\?\\", "\\\\.\\",)):
        fail(f"不支持扩展路径前缀（\\\\?\\ / \\\\.\\）：{root}")
    # 在 resolve 之前检查原始路径及祖先，否则仓库 .tmp 内的 junction
    # 会被解析成普通目录，让后续清空目标的 reparse 检查失效。
    original = root.expanduser().absolute()
    for part in (original, *original.parents):
        try:
            reparse = _reparse_point_status(part)
        except FileNotFoundError:
            continue
        except OSError as error:
            fail(f"无法安全检查目标路径的 reparse point：{part}（{error}）")
        if reparse is not None:
            fail(f"拒绝经过 reparse point 的目标路径：{part}")
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
    # 实现包比入口脚本深一层：仓库根取 parents[2]（与入口 make_tmp.py 的 parent.parent 同值）。
    repo = Path(__file__).resolve().parents[2]
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
