#!/usr/bin/env python3
"""Build the JchTools manual test corpus (nested archives, conflicts, junk, edge names).

The corpus is regenerated from scratch every run; point it at a dedicated directory
(default D:\\testzip) that contains nothing you want to keep. Usage:

    python scripts/make-testdata.py [--destination D:\\testzip]

Layout and expected behaviour of every case are written to `_测试说明.md` inside
the destination, so the person testing does not need to read this script.
"""
from __future__ import annotations

import argparse
import gzip
import os
import random
import shutil
import subprocess
import sys
import tarfile
import zipfile
from pathlib import Path

SEVEN_ZIP_CANDIDATES = [
    Path(r"C:\Program Files\7-Zip\7z.exe"),
    Path(r"C:\Program Files (x86)\7-Zip\7z.exe"),
]
MAKECAB = Path(os.environ.get("SystemRoot", r"C:\Windows")) / "System32" / "makecab.exe"
RANDOM = random.Random(20260912)
PAYLOAD = b"JchTools duplicate payload 2026-09-12\n" * 4


def force_remove_tree(path: Path) -> None:
    """删除目录树，先清掉只读属性（git 对象文件是只读的，Windows 上直接删会拒绝访问）。"""
    import stat

    def on_error(function, target, _error):
        try:
            os.chmod(target, stat.S_IWRITE)
            function(target)
        except OSError:
            pass

    shutil.rmtree(path, onerror=on_error)


def seven_zip() -> Path:
    for candidate in SEVEN_ZIP_CANDIDATES:
        if candidate.is_file():
            return candidate
    found = shutil.which("7z")
    if found:
        return Path(found)
    raise SystemExit("7-Zip not found; install the official full 7-Zip first.")


def run(command: list[str], cwd: Path | None = None) -> None:
    # 7-Zip 在本机按系统代码页输出，不能按 UTF-8 解码；出错时用替换字符展示。
    done = subprocess.run(command, cwd=cwd, capture_output=True)
    if done.returncode != 0:
        stdout = done.stdout.decode("utf-8", "replace") + done.stderr.decode("utf-8", "replace")
        raise SystemExit(f"command failed ({done.returncode}): {' '.join(command)}\n{stdout}")


def write(path: Path, data: bytes) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    return path


def random_bytes(size: int) -> bytes:
    return bytes(RANDOM.getrandbits(8) for _ in range(size))


def zip_members(target: Path, members: dict[str, bytes]) -> None:
    target.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(target, "w", zipfile.ZIP_DEFLATED) as archive:
        for name, data in members.items():
            archive.writestr(name, data)


def build_plain(root: Path, seven: Path, log: list[str]) -> None:
    section = root / "01-普通文件"
    write(section / "报告-2026.txt", "季度报告内容\n".encode())
    write(section / "我的 文档 (1).md", "# 标题\n\n带空格与括号的文件名\n".encode())
    write(section / "notes copy.txt", "季度报告内容\n".encode())          # 与 报告-2026.txt 同内容不同名
    write(section / "子目录" / "深层" / "leaf.bin", random_bytes(8 * 1024))
    log.append("| `01-普通文件/` | 基础扫描：中文名、空格、括号、同名不同内容 | 全部保留；`notes copy.txt` 与 `报告-2026.txt` 内容相同 -> 归入「不同名内容相同」去重 |")


def build_duplicates(root: Path, seven: Path, log: list[str]) -> None:
    section = root / "02-重复内容"
    write(section / "same-name.bin", PAYLOAD)
    write(section / "其它" / "same-name.bin", PAYLOAD)                     # 同名同内容
    for name in ("dup.txt", "dup (1).txt", "dup - 副本.txt", "dup copy.txt"):
        write(section / name, PAYLOAD)                                     # 副本命名
    write(section / "完全无关的名字.bin", PAYLOAD)                          # 不同名同内容
    log.append("| `02-重复内容/` | 三类去重规则各自命中（同名 / 副本命名 / 不同名） | 每组按「保留最新」留一个，其余进回收站；三类规则可在界面分别关掉验证差异 |")


def build_conflicts(root: Path, seven: Path, log: list[str]) -> None:
    section = root / "03-版本冲突"
    old = write(section / "config.ini", b"mode=fast\n")
    fresh = write(section / "归档" / "config.ini", b"mode=safe\nthreads=8\nverify=yes\nlog=verbose\n")
    os.utime(old, (1_700_000_000, 1_700_000_000))
    os.utime(fresh, (1_760_000_000, 1_760_000_000))
    same_size_a = write(section / "same-size-a.dat", random_bytes(64))
    same_size_b = write(section / "归档" / "same-size-a.dat", random_bytes(64))
    stamp = 1_750_000_000
    os.utime(same_size_a, (stamp, stamp))
    os.utime(same_size_b, (stamp, stamp))
    log.append("| `03-版本冲突/` | 同名不同大小（config.ini）与同名同大小不同内容（same-size-a.dat） | 默认保留较大 / 保留最新；两者开关与保留策略独立，可在「冲突」分区改 |")


def build_junk(root: Path, seven: Path, log: list[str]) -> None:
    section = root / "04-垃圾与临时"
    write(section / "Thumbs.db", b"thumbs")
    write(section / "desktop.ini", b"[.ShellClassInfo]\n")
    write(section / "~$季度报告.docx", b"office lock file")
    write(section / "scratch.tmp", b"temp")
    write(section / "backup.bak", b"backup")
    write(section / "empty.txt", b"")
    log.append("| `04-垃圾与临时/` | 系统附属文件、临时/备份文件、零字节文件 | 三组规则默认关闭；在「清理」分区打开后应分别删除（默认进回收站） |")


def build_empty_and_chain(root: Path, seven: Path, log: list[str]) -> None:
    section = root / "05-空目录与单链"
    (section / "空目录").mkdir(parents=True, exist_ok=True)
    write(section / "单链" / "a" / "b" / "c" / "payload.txt", b"chain payload\n")
    log.append("| `05-空目录与单链/` | 真空目录 + 只有一个子项的目录链 | 空目录清理应删除；开启「消除只有一个子项的目录层级」后 payload.txt 应被提升 |")


def build_formats(root: Path, seven: Path, log: list[str]) -> None:
    section = root / "06-格式识别"
    png = bytes.fromhex(
        "89504e470d0a1a0a0000000d494844520000000100000001080600000"
        "01f15c4890000000d4944415478da63f8ffff3f0300050001a5f645400000000049454e44ae426082"
    )
    write(section / "其实是PNG.txt", png)
    buffer = root / "06-格式识别" / ".tmp-zip-source"
    zip_members(buffer / "inner.txt", {"inner.txt": b"real zip container\n"})
    shutil.move(str(buffer / "inner.txt"), str(section / "其实是ZIP.dat"))
    shutil.rmtree(buffer, ignore_errors=True)
    docx = section / "真实文档.docx"
    with zipfile.ZipFile(docx, "w", zipfile.ZIP_DEFLATED) as archive:
        archive.writestr("[Content_Types].xml", "<Types/>")
        archive.writestr("word/document.xml", "<w:document/>")
    write(section / "没有扩展名的PDF", b"%PDF-1.7\n1 0 obj\n<<>>\nendobj\n")
    write(section / "未知格式.bin", random_bytes(256))
    log.append("| `06-格式识别/` | 扩展名与真实内容不一致 | 开启「检测真实类型 + 修正扩展名」后：`.txt`->`.png`、`.dat`->`.zip`；`.docx` 是 ZIP 容器但**不应**被改成 zip；未知格式与无扩展名 PDF 按规则处理 |")


def build_archives(root: Path, seven: Path, log: list[str]) -> None:
    section = root / "07-压缩包-各格式"
    source = root / ".build-07"
    write(source / "alpha.txt", b"alpha member\n")
    write(source / "beta.txt", b"beta member\n")
    write(source / "nested" / "gamma.txt", b"gamma member\n")
    members = sorted(p for p in source.rglob("*") if p.is_file())
    run([str(seven), "a", "-t7z", "-mx=7", "-y", str(section / "base.7z")] + [str(p) for p in members], cwd=source)
    zip_members(section / "base.zip", {"alpha.txt": b"alpha member\n", "beta.txt": b"beta member\n", "nested/gamma.txt": b"gamma member\n"})
    with tarfile.open(section / "base.tar", "w") as archive:
        for member in members:
            archive.add(member, arcname=member.relative_to(source).as_posix())
    for suffix, mode in ((".tar.gz", "w:gz"), (".tgz", "w:gz"), (".tar.bz2", "w:bz2"), (".tar.xz", "w:xz")):
        with tarfile.open(section / f"base{suffix}", mode) as archive:
            for member in members:
                archive.add(member, arcname=member.relative_to(source).as_posix())
    for suffix, opener in ((".gz", gzip.open), (".bz2", __import__("bz2").open), (".xz", __import__("lzma").open)):
        target = section / f"single.txt{suffix}"
        with opener(target, "wb") as stream:
            stream.write(b"single member payload\n" * 8)
    if MAKECAB.is_file():
        run([str(MAKECAB), "/D", "CompressionType=LZX", "/D", "CompressionMemory=21", "alpha.txt", str(section / "data.cab")], cwd=source)
    shutil.rmtree(source, ignore_errors=True)
    log.append("| `07-压缩包-各格式/` | 7z / zip / tar / tar.gz / tgz / tar.bz2 / tar.xz / gz / bz2 / xz / cab（本机 7-Zip 只支持解压 zst，没有构造 zst 用例） | 每个包都应解压成功并产生重复内容（同一段 payload），随后被去重；某个格式失败会记录到错误数 |")


def build_archive_conflicts(root: Path, seven: Path, log: list[str]) -> None:
    section = root / "08-压缩包-冲突"
    write(section / "说明.txt", b"existing file, version A\n")            # 现有 26 B
    zip_members(section / "pack.zip", {"说明.txt": b"archive version B, different content and length\n"})
    write(section / "等长.txt", b"0123456789abcdef")                        # 16 B
    zip_members(section / "等长冲突.zip", {"等长.txt": b"fedcba9876543210"})
    log.append("| `08-压缩包-冲突/` | 解压目标已存在同名文件（大小不同 / 大小相同） | 图形界面逐次询问，可「覆盖旧文件 / 跳过新文件 / 保留最新 / 保留较大 / 两个都保留」并应用到后续全部；命令行默认两个都保留 |")


def build_nested(root: Path, seven: Path, log: list[str]) -> None:
    section = root / "09-压缩包-嵌套"
    stage = root / ".build-09"
    write(stage / "level5.txt", b"innermost payload\n")
    with __import__("lzma").open(stage / "level5.txt.xz", "wb") as stream:
        stream.write((stage / "level5.txt").read_bytes())
    zip_members(stage / "level4.zip", {"level5.txt.xz": (stage / "level5.txt.xz").read_bytes()})
    with tarfile.open(stage / "level3.tar.gz", "w:gz") as archive:
        archive.add(stage / "level4.zip", arcname="level4.zip")
    run([str(seven), "a", "-t7z", "-y", str(stage / "level2.7z"), str(stage / "level3.tar.gz")], cwd=stage)
    zip_members(section / "level1.zip", {"level2.7z": (stage / "level2.7z").read_bytes()})
    shutil.rmtree(stage, ignore_errors=True)
    log.append("| `09-压缩包-嵌套/` | zip -> 7z -> tar.gz -> zip -> xz 共 5 层 | 开启「继续解压嵌套压缩包」后应逐层解压到最内层文本；关闭时只解第一层 |")


def build_deep(root: Path, seven: Path, log: list[str]) -> None:
    section = root / "10-压缩包-超深"
    payload = b"bottom of a very deep nest\n"
    for level in range(18, 0, -1):
        buffer = {}
        buffer[f"d{level + 1:02d}.zip" if level < 18 else "bottom.txt"] = payload
        archive = section / f"tmp{level:02d}.zip"
        zip_members(archive, buffer)
        payload = archive.read_bytes()
        archive.unlink()
    write(section / "d01.zip", payload)
    log.append("| `10-压缩包-超深/` | 18 层嵌套 zip（默认 `max_depth = 16`） | 第 17 层起应停止继续解压并记录，不会无限递归；把「最大嵌套层数」调大可继续 |")


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
    log.append("| `11-压缩包-损坏/` | 被截断的 zip / 7z | 应报错并**保留原包**，不产生半成品文件；错误数 +1 |")


def build_encrypted(root: Path, seven: Path, log: list[str]) -> None:
    section = root / "12-压缩包-加密"
    stage = root / ".build-12"
    write(stage / "secret.txt", b"encrypted payload\n")
    zip_members(section / "secret.zip", {"secret.txt": (stage / "secret.txt").read_bytes()})
    run([str(seven), "a", "-tzip", "-p123456", "-mem=ZipCrypto", "-y", str(section / "secret.zip"), str(stage / "secret.txt")], cwd=stage)
    shutil.rmtree(stage, ignore_errors=True)
    log.append("| `12-压缩包-加密/` | 带密码的 zip（密码 123456） | 应用不提供密码输入，应**跳过并记录**，不会尝试爆破，也不会误删原包 |")


def build_multipart(root: Path, seven: Path, log: list[str]) -> None:
    section = root / "13-压缩包-分卷"
    stage = root / ".build-13"
    write(stage / "big.bin", random_bytes(2 * 1024 * 1024 + 12345))
    run([str(seven), "a", "-t7z", "-mx=1", "-v1m", "-y", str(section / "multipart.7z"), str(stage / "big.bin")], cwd=stage)
    shutil.rmtree(stage, ignore_errors=True)
    volumes = sorted(section.glob("multipart.7z.*"))
    log.append(f"| `13-压缩包-分卷/` | 7z 分卷（{len(volumes)} 个卷，1 MiB/卷） | 应从识别到的首卷调用引擎解出完整文件；源卷保守保留，不自动删除整组 |")


def build_large(root: Path, seven: Path, log: list[str]) -> None:
    section = root / "14-大文件"
    write(section / "big-1.bin", random_bytes(32 * 1024 * 1024))
    write(section / "big-2.bin", random_bytes(32 * 1024 * 1024))
    stage = root / ".build-14"
    write(stage / "zeros.bin", bytes(64 * 1024 * 1024))
    run([str(seven), "a", "-t7z", "-mx=9", "-y", str(section / "zeros-64MiB.7z"), str(stage / "zeros.bin")], cwd=stage)
    shutil.rmtree(stage, ignore_errors=True)
    log.append("| `14-大文件/` | 两个同大小不同内容的 32 MiB 文件 + 一个 64 MiB 全零包 | 测 Hash 吞吐与「大文件单独归类」；全零包压缩比很高，用于确认展开比例上限不会误伤（默认上限 10000） |")


def build_hidden(root: Path, seven: Path, log: list[str]) -> None:
    section = root / "15-隐藏与系统"
    hidden = write(section / "hidden-file.txt", b"hidden\n")
    system = write(section / "system-file.txt", b"system\n")
    if sys.platform == "win32":
        import ctypes

        ctypes.windll.kernel32.SetFileAttributesW(str(hidden), 0x02)
        ctypes.windll.kernel32.SetFileAttributesW(str(system), 0x04)
    log.append("| `15-隐藏与系统/` | 带隐藏属性 / 系统属性的文件 | 默认两者都不扫描；在「安全与性能」分区打开后应出现 |")


def build_hostile(root: Path, seven: Path, log: list[str]) -> None:
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
        archive.addfile(benign, __import__("io").BytesIO(payload))
    log.append("| `16-恶意条目/` | 含 `../`、绝对路径与符号链接的压缩包 | 绝对路径/越界条目应被拒绝或跳过并记录；含链接的包应整体拒绝（不会写出目录之外的文件） |")


RESTORE_SCRIPT = """# 还原到生成时的基线：先恢复被删除/移动的文件，再清掉整理产生的新文件。
# 脚本内容保持 ASCII，避免 PowerShell 5.1 按 ANSI 读取时出错；路径都用通配符定位。
# 安全第一条：解析不到自身目录、或目录里没有 .git 时立刻退出，绝不把 git 命令打到别的仓库。
$here = if ($PSScriptRoot) { $PSScriptRoot } elseif ($PSCommandPath) { Split-Path -Parent $PSCommandPath } else { Split-Path -Parent $MyInvocation.MyCommand.Path }
if (-not $here) { Write-Error 'cannot resolve this script directory'; exit 1 }
# 分类整理可能把本脚本移进子目录，所以向上最多找 4 层；只认提交信息匹配本测试基线的仓库。
$root = $null
$probe = $here
for ($depth = 0; $depth -lt 5 -and $probe; $depth++) {
    if (Test-Path -LiteralPath (Join-Path $probe '.git')) {
        $message = (git -C $probe log -1 --pretty=%s 2>$null)
        if ($message -like '*test corpus baseline*') { $root = $probe; break }
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
    $empty = Get-ChildItem -LiteralPath $chainDir.FullName -Directory | Where-Object { -not (Get-ChildItem -LiteralPath $_.FullName -Recurse -File -ErrorAction SilentlyContinue) } | Select-Object -First 1
    if ($null -eq $empty) {
        # 空目录名与生成脚本保持一致（脚本带 BOM，中文可以安全写在这里）
        New-Item -ItemType Directory -Path (Join-Path $chainDir.FullName '空目录') | Out-Null
    }
}
Write-Host 'restored to baseline:'
git status --short
"""


def initialise_git(root: Path, log: list[str]) -> None:
    """建立可回滚的基线：字节稳定（* -text）、附还原脚本，并把两步都提交。"""
    (root / ".gitattributes").write_text(
        "# 测试数据需要字节可复现：不做换行转换，否则检出后哈希变化会影响去重测试。\n* -text\n",
        encoding="utf-8", newline="\n",
    )
    # 带 BOM 写入：PowerShell 5.1 否则会按 ANSI 解析中文注释，可能吞掉后续语句。
    (root / "恢复.ps1").write_text(RESTORE_SCRIPT, encoding="utf-8-sig", newline="\n")
    run(["git", "init"], cwd=root)
    run(["git", "add", "-A"], cwd=root)
    run(["git", "-c", "core.autocrlf=false", "commit", "-q", "-m",
         "test corpus baseline: 16 groups of generated JchTools test data"], cwd=root)
    log.append("| `.git` + `恢复.ps1` | 生成时建立的 git 基线（需 `--git`） | 整理跑完后执行 `恢复.ps1` 即可回到初始状态：`git checkout -- .` 恢复被删除/移动的文件、`git clean -fd` 清掉新文件，并补回隐藏/系统属性与空目录；`.git/**` 默认在排除规则里，不会被整理 |")


def build_readme(root: Path, log: list[str], seven: Path) -> None:
    text = [
        "# JchTools 手工测试数据集",
        "",
        f"生成时间：{__import__('datetime').date.today().isoformat()}",
        f"生成脚本：`scripts/make-testdata.py`（可重复执行，会先清空本目录）。",
        "所有内容都是脚本生成的假数据，可以随意删除、移动、回收。",
        "",
        "## 建议的测试顺序",
        "",
        "1. 用「处理规则」面板先只开基础项（解压、去重），点「开始解压与分析」，看计划里对每个压缩包、重复组的判定是否符合下表；",
        "2. 逐组勾选/取消计划项，确认「确认并执行整理」只执行勾选项；",
        "3. 再打开归类、清理、类型修正、隐藏/系统等开关，重新分析并执行；",
        "4. 关注底部进度条：分析阶段是往返光带 + 实时计数，执行阶段是百分比 + 已完成/总数。",
        "",
        "## 各目录的用途与预期行为",
        "",
        "| 目录 | 构造内容 | 预期 |",
        "| --- | --- | --- |",
    ]
    text.extend(log)
    text.extend([
        "",
        "## 注意",
        "",
        "- 全零压缩包解出来是 64 MiB 的 `zeros.bin`，分卷包解出来是 2 MiB 的 `big.bin`，执行整理前请确认磁盘空间；",
        "- 删除动作默认走回收站（可在规则里改成永久删除）；想验证「回收失败则永久删除」请在规则里调整；",
        "- `16-恶意条目/` 里的包是为安全测试准备的，请只在测试目录里使用；",
        "- 本说明文件本身也是被扫描的对象，不需要时可以直接删除。",
        "",
        "## 整理之后如何恢复",
        "",
        "本目录在生成时已用 git 建立基线（`--git`）。整理跑完后，在本目录执行：",
        "",
        "```powershell",
        ".\\恢复.ps1",
        "```",
        "",
        "等价的手工命令是 `git checkout -- .`（恢复被删除/移动的原始文件）+ `git clean -fd`（清掉解压与归类产生的新文件）。",
        "git 不保存 Windows 隐藏/系统属性和空目录，所以 `恢复.ps1` 会额外补这两类；目录里的 `.gitattributes`（`* -text`）保证检出后字节与初始一致，去重哈希才有可比性。",
        "想彻底重来，直接重跑生成脚本（会先清空目录）。",
    ])
    root.joinpath("_测试说明.md").write_text("\n".join(text) + "\n", encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser(description="Build the JchTools manual test corpus.")
    parser.add_argument("--destination", default=r"D:\testzip", help="target directory (wiped first)")
    parser.add_argument("--git", action="store_true", help="also create a git baseline plus a restore script")
    arguments = parser.parse_args()
    root = Path(arguments.destination)
    seven = seven_zip()

    if root.exists():
        print(f"clearing {root} ...")
        force_remove_tree(root)
    root.mkdir(parents=True)

    log: list[str] = []
    for builder in (
        build_plain, build_duplicates, build_conflicts, build_junk, build_empty_and_chain,
        build_formats, build_archives, build_archive_conflicts, build_nested, build_deep,
        build_broken, build_encrypted, build_multipart, build_large, build_hidden, build_hostile,
    ):
        builder(root, seven, log)
        print(f"  built {builder.__name__}")
    build_readme(root, log, seven)
    if arguments.git:
        initialise_git(root, log)

    files = [p for p in root.rglob("*") if p.is_file() and ".git" not in p.parts]
    total = sum(p.stat().st_size for p in files)
    print(f"corpus: {root}")
    print(f"files: {len(files)}  size: {total / 1048576:.1f} MiB")
    return 0


if __name__ == "__main__":
    sys.exit(main())
