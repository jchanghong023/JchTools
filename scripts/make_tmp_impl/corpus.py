"""临时目录工厂语料构建器：testdata 全部 build_* 分区、git 基线与说明文档（自 make_tmp.py 原样搬移）."""

from __future__ import annotations

import bz2
import ctypes
import gzip
import lzma
import os
import shutil
import sys
import tarfile
import zipfile
from datetime import UTC, datetime
from io import BytesIO
from pathlib import Path

from scripts.make_tmp_impl.fsutil import (
    _add_tar_members,
    _line,
    _write_compressed_tar,
    fail,
    force_remove_tree,
    random_bytes,
    run,
    write,
    zip_members,
)

MAKECAB = Path(os.environ.get("SYSTEMROOT", r"C:\Windows")) / "System32" / "makecab.exe"
PAYLOAD = b"JchTools duplicate payload 2026-09-12\n" * 4
DEEP_ZIP_LAYERS = 18  # 10-压缩包-超深 的嵌套层数（默认 max_depth=16，第 17 层起应停止解压）

# 内联 git 身份（baseline 与 readme 两次提交共用）：不依赖机器上全局/系统
# git user.email / user.name 配置。
_GIT_COMMIT_IDENTITY = [
    "-c",
    "core.autocrlf=false",
    "-c",
    "user.email=test@local",
    "-c",
    "user.name=testdata",
]


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
    for name in ("dup.txt", "dup (1).txt", "dup - 副本.txt", "dup - copy.txt"):
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
            (
                "payload.txt 归类进「文档」后，单链/a/b/c 搬空、按空目录清理删除（C-06："
                "不设「消除单子项层级」开关，靠归类与空目录清理自然消除）|"
            ),
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
    force_remove_tree(buffer)
    docx = section / "真实文档.docx"
    with zipfile.ZipFile(docx, "w", zipfile.ZIP_DEFLATED) as archive:
        archive.writestr("[Content_Types].xml", "<Types/>")
        archive.writestr("word/document.xml", "<w:document/>")
    write(section / "没有扩展名的PDF", b"%PDF-1.7\n1 0 obj\n<<>>\nendobj\n")
    write(section / "未知格式.bin", random_bytes(256))
    log.append(
        _line(
            "| `06-格式识别/` | 扩展名与真实内容不一致 | ",
            (
                "开启「按内容签名修正扩展名」后：`.txt`->`.png`；`.dat` 只能识别到 ZIP 容器、"
                "无法确认内部类型，按 C-08 保持 `.dat` 不改名；"
            ),
            "`.docx` 是 ZIP 容器但**不应**被改成 zip；",
            "未知格式与无扩展名 PDF 按规则处理 |",
        )
    )


def build_archives(root: Path, seven: Path, log: list[str]) -> None:
    section = root / "07-压缩包-各格式"
    section.mkdir(parents=True, exist_ok=True)
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
    # 压缩流与 `.tar` 组合（X-01）：.lzma 引擎只能读不能写，用 Python 按 FORMAT_ALONE 生成。
    _ = (section / "single.txt.lzma").write_bytes(lzma.compress(single_payload, format=lzma.FORMAT_ALONE))
    buffer = BytesIO()
    with tarfile.open(fileobj=buffer, mode="w") as archive:
        _add_tar_members(archive, members, source)
    _ = (section / "bundle.tar.lzma").write_bytes(lzma.compress(buffer.getvalue(), format=lzma.FORMAT_ALONE))
    force_remove_tree(source)
    log.append(
        _line(
            "| `07-压缩包-各格式/` | 白名单内的 7z / zip / tar / tar.gz / tgz / tar.bz2 / tar.xz / gz / bz2 / xz /",
            "单文件流 .lzma 与 `.tar.lzma` 组合（本机 7-Zip 只支持解压 zst，没有构造 zst 用例） | ",
            "每个包都应解压成功后永久删除原包，同内容成员仍完整落盘，中间 tar 层不留在正式位置；",
            "只有另行执行目录整理才会去重；失败记录原因 |",
        )
    )


def build_containers(root: Path, _seven: Path, log: list[str]) -> None:
    """X-01 白名单反例：这些文件都能被 7-Zip 打开，但语义上不是用户要整理的归档文件."""
    section = root / "17-容器不自动解压"
    # 内容一律用合法 ZIP：证明「完全不碰」不是打不开，而是根本不在自动解压白名单内。
    containers = (
        "windows.iso",
        "boot.wim",
        "install.esd",
        "setup.msi",
        "setup.exe",
        "app.apk",
        "library.jar",
        "wheel.whl",
        "book.epub",
        "report.docx",
        "legacy.lzh",
        "archive.cpio",
        "addon.crx",
        "styles.xpi",
    )
    for index, name in enumerate(containers):
        payload = f"container payload {index}\n".encode()
        zip_members(section / name, {f"member{index:02}.bin": payload})
    if MAKECAB.is_file():
        source = root / ".build-17"
        write(source / "alpha.txt", b"alpha member\n")
        run(
            [
                str(MAKECAB),
                "/D",
                "CompressionType=LZX",
                "/D",
                "CompressionMemory=21",
                "alpha.txt",
                str(section / "visproww.cab"),
            ],
            cwd=source,
        )
        force_remove_tree(source)
    else:
        zip_members(section / "visproww.cab", {"alpha.txt": b"alpha member\n"})
    log.append(
        _line(
            "| `17-容器不自动解压/` | 内容合法可读的安装资源 `.cab`、镜像 `.iso/.wim/.esd`、",
            "安装包 `.msi/.exe`、程序包 `.apk/.jar/.whl`、浏览器扩展 `.crx/.xpi`、",
            "文档容器 `.docx/.epub`、旧式容器 `.lzh/.cpio` | ",
            "一律完全不碰：不解压、不删除、不移动、不进「解压失败」——",
            "7-Zip 能打开不等于应该解压，只有白名单格式及其分卷才自动解压 |",
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
    force_remove_tree(stage)
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
    force_remove_tree(stage)
    log.append("| `11-压缩包-损坏/` | 被截断的 zip / 7z | 应报错并移入「解压失败」，不删除原包、不落盘未校验结果 |")


def build_encrypted(root: Path, seven: Path, log: list[str]) -> None:
    section = root / "12-压缩包-加密"
    section.mkdir(parents=True, exist_ok=True)
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
    force_remove_tree(stage)
    log.append(
        _line(
            "| `12-压缩包-加密/` | 带密码的 zip（密码 123456） | ",
            "应用不提供密码输入，应移入「解压失败」并记录原因，不删除原包 |",
        )
    )


def build_multipart(root: Path, seven: Path, log: list[str]) -> None:
    section = root / "13-压缩包-分卷"
    section.mkdir(parents=True, exist_ok=True)
    stage = root / ".build-13"
    write(stage / "big.bin", random_bytes(2 * 1024 * 1024 + 12345))
    run(
        [str(seven), "a", "-t7z", "-mx=1", "-v1m", "-y", str(section / "multipart.7z"), str(stage / "big.bin")],
        cwd=stage,
    )
    force_remove_tree(stage)
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
    force_remove_tree(stage)
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
        if not ctypes.windll.kernel32.SetFileAttributesW(str(hidden), 0x02):
            fail(f"无法设置测试隐藏属性：{hidden}")
        if not ctypes.windll.kernel32.SetFileAttributesW(str(system), 0x04):
            fail(f"无法设置测试系统属性：{system}")
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
            "绝对路径/越界条目应判整包失败并隔离，不合入任何成员；含链接的包也应整体拒绝 |",
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
    # 内联身份提交：见模块头 _GIT_COMMIT_IDENTITY 注释。
    run(
        [
            "git",
            *_GIT_COMMIT_IDENTITY,
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
