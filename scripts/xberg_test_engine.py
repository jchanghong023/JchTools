"""为本地真实引擎测试核验并准备官方最新 Xberg；源码仓库从不参与构建。."""

from __future__ import annotations

import hashlib
import io
import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import zipfile
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from collections.abc import Callable
    from typing import TypeIs

ROOT = Path(__file__).resolve().parent.parent
EXPECTED_TEST_ENGINE = Path(r"C:\Users\jiang\Documents\xberg-test\xberg-cli-x86_64-pc-windows-msvc")
TEST_ENGINE = EXPECTED_TEST_ENGINE
REPOSITORY = "jchanghong023/xberg"
ARCHIVE_NAME = "xberg-cli-x86_64-pc-windows-msvc.zip"
RECEIPT_NAME = ".jchtools-test-release.json"
WORKSPACE = ROOT / ".tmp" / "xberg-test-latest"
_parse_json: Callable[[str], object] = json.loads


def object_map(value: object) -> TypeIs[dict[str, object]]:
    return isinstance(value, dict)


def object_list(value: object) -> TypeIs[list[object]]:
    return isinstance(value, list)


def text_field(value: dict[str, object], key: str) -> str:
    field = value.get(key)
    if not isinstance(field, str) or not field:
        message = f"发布元数据缺少字符串字段：{key}"
        raise ValueError(message)
    return field


@dataclass(frozen=True)
class Release:
    tag: str
    archive_sha256: str
    archive_size: int


def release_from_metadata(value: object) -> Release:
    if not object_map(value) or value.get("draft") or value.get("prerelease"):
        message = "latest 元数据不是正式发布"
        raise ValueError(message)
    tag = text_field(value, "tag_name")
    if any(character not in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-" for character in tag):
        message = "latest 发布标签非法"
        raise ValueError(message)
    assets = value.get("assets")
    if not object_list(assets):
        message = "latest 发布没有资产清单"
        raise ValueError(message)
    matching = [asset for asset in assets if object_map(asset) and asset.get("name") == ARCHIVE_NAME]
    if len(matching) != 1 or not object_map(matching[0]):
        message = "latest Windows x64 归档缺失或重复"
        raise ValueError(message)
    asset = matching[0]
    expected_url = f"https://github.com/{REPOSITORY}/releases/download/{tag}/{ARCHIVE_NAME}"
    if text_field(asset, "browser_download_url") != expected_url:
        message = "latest 归档来源不是指定官方仓库"
        raise ValueError(message)
    digest = text_field(asset, "digest")
    prefix, separator, sha256 = digest.partition(":")
    size = asset.get("size")
    if (
        prefix != "sha256"
        or not separator
        or len(sha256) != hashlib.sha256().digest_size * 2
        or any(character not in "0123456789abcdefABCDEF" for character in sha256)
        or not isinstance(size, int)
        or isinstance(size, bool)
        or size <= 0
    ):
        message = "latest 归档没有可靠的 SHA-256 或大小"
        raise ValueError(message)
    return Release(tag, sha256.lower(), size)


def digest_file(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def current_engine_matches(release: Release) -> bool:
    executable = TEST_ENGINE / "xberg.exe"
    if not executable.is_file():
        return False
    manifest = _parse_json((ROOT / "resources" / "markdown-assets.json").read_text(encoding="utf-8"))
    if not object_map(manifest):
        return False
    pack = manifest.get("xberg")
    if object_map(pack) and pack.get("tag") == release.tag and pack.get("archive_sha256") == release.archive_sha256:
        members = pack.get("members")
        if object_list(members):
            for member in members:
                if object_map(member) and member.get("path") == "xberg.exe":
                    return digest_file(executable) == member.get("sha256")
    receipt_path = TEST_ENGINE / RECEIPT_NAME
    if not receipt_path.is_file():
        return False
    receipt: object = _parse_json(receipt_path.read_text(encoding="utf-8"))
    return (
        object_map(receipt)
        and receipt.get("tag") == release.tag
        and receipt.get("archive_sha256") == release.archive_sha256
        and digest_file(executable) == receipt.get("engine_sha256")
    )


def unpack_verified_archive(archive: Path, destination: Path) -> None:
    """已校验官方归档仍拒绝路径逃逸、链接和重复成员，产物仅在 .tmp 内。."""
    destination.mkdir()
    names: set[str] = set()
    with zipfile.ZipFile(archive) as package:
        for member in package.infolist():
            relative = Path(member.filename)
            if relative.parts and relative.parts[0] == TEST_ENGINE.name:
                relative = Path(*relative.parts[1:])
            target = (destination / relative).resolve()
            if destination.resolve() not in target.parents:
                if member.is_dir() and target == destination.resolve():
                    continue
                message = f"归档路径越界：{member.filename}"
                raise ValueError(message)
            key = str(relative).casefold()
            if key in names or stat.S_ISLNK(member.external_attr >> 16):
                message = f"归档含重复成员或链接：{member.filename}"
                raise ValueError(message)
            names.add(key)
            if member.is_dir():
                target.mkdir(parents=True, exist_ok=True)
            else:
                target.parent.mkdir(parents=True, exist_ok=True)
                with package.open(member) as source, target.open("xb") as output:
                    shutil.copyfileobj(source, output)
    if not (destination / "xberg.exe").is_file():
        message = "latest 归档中没有 xberg.exe"
        raise ValueError(message)


def ensure_test_engine_is_idle() -> None:
    """覆盖固定目录前拒绝正在运行的引擎，不终止用户或其他测试的进程。."""
    powershell = shutil.which("powershell")
    if powershell is None:
        message = "无法核验固定引擎目录是否被占用，拒绝替换"
        raise ValueError(message)
    escaped = str(TEST_ENGINE).replace("'", "''") + "\\"
    command = (
        f"$taskEngineRoot = '{escaped}'; "
        "$taskEngineProcesses = @(Get-Process -ErrorAction Stop | Where-Object { "
        "$_.Path -and $_.Path.StartsWith($taskEngineRoot, [StringComparison]::OrdinalIgnoreCase) }); "
        "if ($taskEngineProcesses.Count -gt 0) { exit 1 }"
    )
    result = subprocess.run(
        [powershell, "-NoProfile", "-Command", command], capture_output=True, check=False, timeout=30
    )
    if result.returncode != 0:
        message = "固定测试引擎目录仍有运行进程或占用检查失败；保留旧目录，不运行旧版测试"
        raise ValueError(message)


def ensure_test_engine_target() -> None:
    """删除或复制前核对唯一固定物理目标；恢复不豁免此边界。."""
    if TEST_ENGINE.resolve() != EXPECTED_TEST_ENGINE or TEST_ENGINE.is_symlink() or TEST_ENGINE.is_junction():
        message = "固定测试引擎目录发生重定向，拒绝删除或复制"
        raise OSError(message)


def replace_prepared_tree(prepared: Path, expected_sha256: str) -> None:
    """实际删除前再次确认唯一固定目标，并校验新引擎落位后的身份。."""
    ensure_test_engine_target()
    if TEST_ENGINE.exists():
        shutil.rmtree(TEST_ENGINE)
    _ = shutil.copytree(prepared, TEST_ENGINE)
    if digest_file(TEST_ENGINE / "xberg.exe") != expected_sha256:
        message = "落位后的最新引擎摘要不一致"
        raise OSError(message)


def install_latest(gh: str, release: Release) -> None:
    # 唯一替换目标是用户明确指定的测试目录；源 E:\xberg 永远不读取或编译。
    ensure_test_engine_target()
    ensure_test_engine_is_idle()
    _ = subprocess.run(
        [sys.executable, str(ROOT / "scripts" / "make_tmp.py"), "workspace", "--destination", str(WORKSPACE)],
        check=True,
    )
    with tempfile.TemporaryDirectory(prefix="release-", dir=WORKSPACE) as temporary:
        scratch = Path(temporary)
        archive = scratch / ARCHIVE_NAME
        _ = subprocess.run(
            [
                gh,
                "release",
                "download",
                release.tag,
                "--repo",
                REPOSITORY,
                "--pattern",
                ARCHIVE_NAME,
                "--dir",
                str(scratch),
            ],
            check=True,
            timeout=600,
        )
        if archive.stat().st_size != release.archive_size or digest_file(archive) != release.archive_sha256:
            message = "latest 下载大小或 SHA-256 不匹配；保留旧引擎"
            raise ValueError(message)
        prepared = scratch / "prepared"
        unpack_verified_archive(archive, prepared)
        receipt = {
            "tag": release.tag,
            "archive_sha256": release.archive_sha256,
            "engine_sha256": digest_file(prepared / "xberg.exe"),
        }
        _ = (prepared / RECEIPT_NAME).write_text(json.dumps(receipt, indent=2), encoding="utf-8")
        # 旧树备份独立于下载临时目录的自动清理；恢复失败时必须继续保留。
        recovery_root = Path(tempfile.mkdtemp(prefix="recovery-", dir=WORKSPACE))
        backup = recovery_root / "previous"
        retain_backup = False
        try:
            if TEST_ENGINE.exists():
                _ = shutil.copytree(TEST_ENGINE, backup)
            # 下载与备份期间服务可能启动，真正替换前重新核验。
            ensure_test_engine_is_idle()
            try:
                replace_prepared_tree(prepared, receipt["engine_sha256"])
            except OSError:
                try:
                    ensure_test_engine_target()
                    if TEST_ENGINE.exists():
                        shutil.rmtree(TEST_ENGINE)
                    if backup.exists():
                        _ = shutil.copytree(backup, TEST_ENGINE)
                except OSError as rollback_error:
                    retain_backup = True
                    message = f"替换失败且旧引擎恢复失败；完整旧备份保留在 {backup}；{rollback_error}"
                    raise OSError(message) from rollback_error
                raise
        finally:
            if not retain_backup:
                shutil.rmtree(recovery_root)


def main() -> int:
    if isinstance(sys.stdout, io.TextIOWrapper):
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    if sys.platform != "win32":
        print("UNVERIFIED：真实 Xberg 测试仅支持 Windows")
        return 2
    gh = shutil.which("gh")
    if gh is None:
        print("UNVERIFIED：缺少 gh，无法核验官方最新发布")
        return 2
    override = os.environ.get("JCHTOOLS_TEST_XBERG_DIR")
    if override and Path(override).resolve() != TEST_ENGINE.resolve():
        print(f"UNVERIFIED：真实引擎测试必须使用固定最新目录 {TEST_ENGINE}")
        return 2
    try:
        response = subprocess.run(
            [gh, "api", f"repos/{REPOSITORY}/releases/latest"],
            capture_output=True,
            encoding="utf-8",
            check=True,
            timeout=60,
        )
        release = release_from_metadata(_parse_json(response.stdout))
        if current_engine_matches(release):
            print(f"PASS：已存在最新引擎 {release.tag}，不重复下载；目录 {TEST_ENGINE}")
        else:
            install_latest(gh, release)
            print(f"PASS：已校验并安装最新引擎 {release.tag}；目录 {TEST_ENGINE}")
    except (OSError, ValueError, subprocess.SubprocessError, zipfile.BadZipFile) as error:
        print(f"FAIL：无法准备最新测试引擎：{error}")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
