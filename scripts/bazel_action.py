"""在 Bazel 原生构建动作或测试中执行一条既有门命令.

适配即时失败通知、动态日志路径与 Cargo 运行产物声明；缓存由 Bazel 提供。
恢复先按文件表声明的长度与 mtime 检测门内写入（外部改动按合同 P-08 不防御）：
全部未动过时判定匹配，不重读产物或压缩包；出现差异时完整验证整包 SHA 与各
成员/结构 SHA 后才校验/恢复最终消费者文件。恢复写入保留快照声明的 mtime。
成功 Cargo 动作按当前编译读取输出 Bazel unused_inputs_list；未知元数据保留全部
声明输入，失败动作不发布可用于输入细化的成功声明。
"""

from __future__ import annotations

import hashlib
import importlib
import json
import os
import shutil
import stat
import subprocess
import sys
import time
import uuid
import zipfile
import zlib
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import TYPE_CHECKING, NotRequired, TypedDict, cast

if TYPE_CHECKING:
    from collections.abc import Callable
    from typing import IO


class GateSpec(TypedDict):
    name: str
    kind: str
    root: str
    argv: list[str]
    environment: dict[str, str]
    watch_test_failures: bool
    cargo_artifacts: NotRequired[bool]


class LiveRuntime(TypedDict):
    environment: dict[str, str]
    log: str
    result_file: str
    TEMP: str | None
    TMP: str | None
    failure_file: NotRequired[str | None]
    phase_file: NotRequired[str | None]


class DispatchConfig(TypedDict):
    argv: list[str]
    runtime: str


class CargoArtifact(TypedDict):
    reason: NotRequired[str]
    profile: NotRequired[dict[str, object]]
    target: NotRequired[dict[str, object]]
    executable: NotRequired[str | None]
    filenames: NotRequired[list[str]]


_PAYLOAD_PREFIX = b"\nJCHTOOLS_GATE_PAYLOAD_V1 "
_PAYLOAD_END = b"\nJCHTOOLS_GATE_PAYLOAD_END_V1\n"
_PAYLOAD_HEADER_FIELDS = 2
_PAYLOAD_HASH_LENGTH = 64
_CARGO_OUTPUT_COUNT = 2
_CARGO_SNAPSHOT_VERSION = 3
_CARGO_MAX_FILE_SIZE = 1024**4
_CARGO_MAX_TOTAL_SIZE = 16 * _CARGO_MAX_FILE_SIZE
_CARGO_MAX_METADATA_SIZE = 16 * 1024 * 1024
_CARGO_MAX_MTIME = 2**63 - 1
_CARGO_ZIP_LOCAL_HEADER_SIZE = 30


class _CargoFile(TypedDict):
    path: str
    size: int
    sha256: str
    member_sha256: str
    mtime_ns: int


class _CargoManifest(TypedDict):
    version: int
    files: list[_CargoFile]
    payload_sha256: str


def _cargo_environment_supported(environment: dict[str, str]) -> bool:
    overrides = {
        "CARGO_TARGET_DIR",
        "CARGO_BUILD_TARGET_DIR",
        "CARGO_BUILD_TARGET",
        "RUSTC",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
    }
    return not any(
        key.upper() in overrides
        or (key.upper().startswith("JCHTOOLS_") and not key.upper().startswith("JCHTOOLS_GATE_"))
        for key in environment
    )


def _cargo_arguments_supported(command: str, arguments: list[str]) -> bool:
    if command == "test" and "--no-run" not in arguments:
        return False
    for index, argument in enumerate(arguments):
        if argument in {"--doc", "--release", "-r"} or argument.partition("=")[0] in {
            "--target-dir",
            "--target",
            "--profile",
            "--config",
        }:
            return False
        if argument == "--" and command != "clippy":
            return False
        if argument.startswith("--message-format=") and argument != "--message-format=json":
            return False
        if argument == "--message-format" and arguments[index + 1 : index + 2] != ["json"]:
            return False
    return True


def cargo_cache_supported(argv: list[str], kind: str, environment: dict[str, str]) -> bool:
    """只接纳现有本地 debug target 的已知 Cargo 编译原语."""
    if kind not in {"compile", "compiler-observed"} or not argv[1:]:
        return False
    if Path(argv[0]).stem.lower() != "cargo" or argv[1] not in {"build", "check", "clippy", "test"}:
        return False
    return _cargo_environment_supported(environment) and _cargo_arguments_supported(argv[1], argv[2:])


def _cargo_path(root: Path, value: str) -> Path:
    relative = Path(value)
    if relative.is_absolute() or relative.drive or "\\" in value or ":" in value:
        message = "Cargo snapshot 必须使用仓库相对 POSIX 路径"
        raise ValueError(message)
    if not relative.parts or any(part in {".", ".."} for part in value.split("/")):
        message = "Cargo snapshot 路径越界"
        raise ValueError(message)
    path = (root / relative).resolve()
    if not path.is_relative_to(root):
        message = "Cargo snapshot 路径越界"
        raise ValueError(message)
    parts = relative.parts
    local = parts[:2] == ("target", "debug")
    optional = parts[:1] == ("optional",) and parts[2:4] == ("target", "debug") and bool(parts[4:])
    if not local and not optional:
        message = "Cargo snapshot 仅支持既有本地 debug target"
        raise ValueError(message)
    return path


def _cargo_digest(path: Path) -> str:
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def _cargo_zip_info(name: str) -> zipfile.ZipInfo:
    info = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
    info.compress_type = zipfile.ZIP_DEFLATED
    info.compress_level = 1
    info.create_system = 3
    info.external_attr = 0o100600 << 16
    return info


def _cargo_member_position(source: IO[bytes], offset: int, observe: Callable[[bytes], None] | None) -> None:
    if observe is None:
        _ = source.seek(offset)
        return
    # 整包消费者按 header_offset 排序调用，间隙和所有非运行成员也参与摘要。
    remaining = offset - source.tell()
    if remaining < 0:
        message = "Cargo snapshot archive 成员重叠或偏移无效"
        raise ValueError(message)
    while remaining:
        block = source.read(min(remaining, 1024 * 1024))
        if not block:
            message = "Cargo snapshot archive 间隙截断"
            raise ValueError(message)
        observe(block)
        remaining -= len(block)


def _cargo_member_digest(
    archive: zipfile.ZipFile, info: zipfile.ZipInfo, *, observe: Callable[[bytes], None] | None = None
) -> str:
    """绑定压缩字节和 central 元数据；observe 同步获取连续整包字节."""
    source = archive.fp
    if source is None:
        message = "Cargo snapshot archive 已关闭"
        raise ValueError(message)
    position = source.tell()
    try:
        _cargo_member_position(source, info.header_offset, observe)
        header = source.read(_CARGO_ZIP_LOCAL_HEADER_SIZE)
        if len(header) != _CARGO_ZIP_LOCAL_HEADER_SIZE or header[:4] != b"PK\x03\x04":
            message = "Cargo snapshot archive 本地头无效"
            raise ValueError(message)
        if observe is not None:
            observe(header)
        remaining = int.from_bytes(header[26:28], "little") + int.from_bytes(header[28:30], "little")
        remaining += info.compress_size
        digest = hashlib.sha256(header)
        # Python 3.13 写侧 ZipInfo 的 UTF 位可能仍为 1，而落盘 ASCII 文件名已清除此位。
        # 成员 writer 关闭后，本地头已回填；读侧仍使用 central flags，拒绝其独立篡改。
        flags = int.from_bytes(header[6:8], "little") if archive.mode != "r" else info.flag_bits
        # central CRC/长度也必须绑定，不能仅依赖未解压成员的 ZIP CRC 校验。
        digest.update(
            json.dumps(
                [info.CRC, info.file_size, info.compress_size, info.compress_type, flags],
                separators=(",", ":"),
            ).encode("ascii")
        )
        while remaining:
            block = source.read(min(remaining, 1024 * 1024))
            if not block:
                message = "Cargo snapshot archive 压缩成员截断"
                raise ValueError(message)
            digest.update(block)
            if observe is not None:
                observe(block)
            remaining -= len(block)
        return digest.hexdigest()
    finally:
        if observe is None:
            _ = source.seek(position)


def _valid_cargo_artifact(event: dict[str, object]) -> bool:
    target, profile, filenames = event.get("target"), event.get("profile"), event.get("filenames")
    if not isinstance(target, dict) or not isinstance(profile, dict) or not isinstance(filenames, list):
        return False
    target = cast("dict[str, object]", target)
    profile = cast("dict[str, object]", profile)
    filenames = cast("list[object]", filenames)
    kinds = target.get("kind")
    if not isinstance(kinds, list):
        return False
    kinds = cast("list[object]", kinds)
    name, executable = target.get("name"), event.get("executable")
    return (
        isinstance(name, str)
        and bool(name)
        and bool(kinds)
        and all(isinstance(kind, str) and bool(kind) for kind in kinds)
        and isinstance(profile.get("test"), bool)
        and bool(filenames)
        and all(isinstance(filename, str) and bool(filename) for filename in filenames)
        and "executable" in event
        and (executable is None or (isinstance(executable, str) and bool(executable)))
    )


def _cargo_log_artifacts(log: Path) -> list[CargoArtifact]:
    artifacts: list[CargoArtifact] = []
    with log.open(encoding="utf-8", errors="replace") as source:
        for line in source:
            try:
                value = cast("object", json.loads(line))
            except ValueError:
                continue
            if not isinstance(value, dict):
                continue
            event = cast("dict[str, object]", value)
            if event.get("reason") != "compiler-artifact":
                continue
            if not _valid_cargo_artifact(event):
                message = "Cargo compiler-artifact JSON 结构无效"
                raise ValueError(message)
            artifacts.append(cast("CargoArtifact", cast("object", event)))
    if not artifacts:
        message = "Cargo 日志未发布有效 compiler-artifact JSON，拒绝缓存空证据"
        raise ValueError(message)
    return artifacts


def _cargo_program_materials(program: Path) -> set[Path]:
    paths = {program.with_suffix(".d").resolve(strict=True)}
    pdb = program.with_suffix(".pdb")
    if pdb.is_file():
        paths.add(pdb.resolve(strict=True))
    return paths


def _cargo_runtime_paths(root: Path, artifacts: list[CargoArtifact]) -> set[Path]:
    paths: set[Path] = set()
    runtime_directories: set[Path] = set()
    for artifact in artifacts:
        if artifact.get("target", {}).get("kind") == ["custom-build"]:
            # build-script-build 是 Cargo 编译工具；其别名 dep-info 与指纹由 Cargo 管理。
            continue
        filenames = list(artifact.get("filenames", []))
        executable = artifact.get("executable")
        if executable:
            filenames.append(executable)
        for filename in filenames:
            path = Path(filename)
            if path.suffix.lower() not in {".exe", ".dll", ".pdb"}:
                continue
            path = (root / path).resolve(strict=True)
            _ = _cargo_path(root, path.relative_to(root).as_posix())
            paths.add(path)
            if path.suffix.lower() == ".exe":
                paths.update(_cargo_program_materials(path))
                runtime_directories.add(path.parent)
                if path.parent.name == "deps":
                    runtime_directories.add(path.parent.parent)
    for runtime in runtime_directories:
        paths.update(dll.resolve(strict=True) for dll in runtime.glob("*.dll"))
    return paths


def _cargo_native_archive(root: Path, paths: list[Path], payload: Path, engine: Path) -> list[_CargoFile]:
    """使用已声明的 bundled 7zip；子进程继承动作 Job，不另建逃逸进程组."""
    if not engine.with_name("7z.dll").is_file():
        message = "Cargo snapshot bundled 7zip 缺少 7z.dll"
        raise OSError(message)
    files: list[_CargoFile] = []
    checksums: dict[str, int] = {}
    for path in paths:
        relative = path.relative_to(root).as_posix()
        _ = _cargo_path(root, relative)
        digest = hashlib.sha256()
        size, crc = 0, 0
        with path.open("rb") as reader:
            while block := reader.read(1024 * 1024):
                digest.update(block)
                crc = zlib.crc32(block, crc)
                size += len(block)
        files.append(
            {
                "path": relative,
                "size": size,
                "sha256": digest.hexdigest(),
                "member_sha256": "",
                "mtime_ns": path.stat().st_mtime_ns,
            }
        )
        checksums[relative] = crc
    listfile = payload.parent / "cargo-files.txt"
    try:
        # 相对 POSIX 名称、显式文件清单与 -spd/-r- 禁止 wildcard 或递归扩张。
        _ = listfile.write_text("".join(record["path"] + "\n" for record in files), encoding="utf-8")
        payload.unlink(missing_ok=True)
        result = subprocess.run(
            [
                str(engine),
                "a",
                "-tzip",
                "-mm=Deflate",
                "-mx=1",
                "-mmt=on",
                # 本地头也强制 UTF-8，避免 zipfile 追加目录时重编码后与 OEM 本地头不一致。
                "-mcu=on",
                "-mtc=off",
                "-mta=off",
                "-mtm=off",
                "-scsUTF-8",
                "-spd",
                "-r-",
                "-y",
                str(payload),
                "@" + str(listfile.resolve()),
            ],
            cwd=root,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )
        if result.returncode:
            message = f"Cargo snapshot 7zip exit={result.returncode}: {result.stdout.decode('utf-8', errors='replace')}"
            raise OSError(message)
        with zipfile.ZipFile(payload) as archive:
            members = archive.infolist()
            names = [info.filename for info in members]
            if len(set(names)) != len(names) or set(names) != {record["path"] for record in files}:
                message = "Cargo snapshot 7zip 成员名称或文件集合不匹配"
                raise ValueError(message)
            for record in files:
                info = archive.getinfo(record["path"])
                if (
                    info.is_dir()
                    or info.compress_type not in {zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED}
                    or info.flag_bits & 1
                    or info.file_size != record["size"]
                    or checksums[record["path"]] != info.CRC
                ):
                    message = "Cargo snapshot 7zip 成员类型、CRC 或长度不匹配"
                    raise ValueError(message)
                with archive.open(info):
                    pass
                record["member_sha256"] = _cargo_member_digest(archive, info)
        return files
    finally:
        listfile.unlink(missing_ok=True)


def capture_cargo_artifacts(root: Path, log: Path, directory: Path, manifest: Path) -> None:
    """打包真实 Cargo 产物；v3 内嵌文件 SHA 与压缩成员/结构 SHA，整包再绑定文件表."""
    root = root.resolve(strict=True)
    paths = sorted(_cargo_runtime_paths(root, _cargo_log_artifacts(log)))
    directory.mkdir(parents=True, exist_ok=True)
    payload = (directory / "payload.zip").resolve()
    engine = root / "resources/7zip/7z.exe"
    native = bool(paths) and engine.exists()
    files: list[_CargoFile] = _cargo_native_archive(root, paths, payload, engine) if native else []
    with zipfile.ZipFile(payload, "a" if native else "w", compression=zipfile.ZIP_DEFLATED, compresslevel=1) as archive:
        for path in [] if native else paths:
            relative = path.relative_to(root).as_posix()
            _ = _cargo_path(root, relative)
            digest = hashlib.sha256()
            size = 0
            info = _cargo_zip_info(relative)
            # 显式使用固定 ZipInfo，避免本地时间及只读权限进入压缩头。
            with path.open("rb") as reader, archive.open(info, "w", force_zip64=True) as writer:
                while block := reader.read(1024 * 1024):
                    _ = writer.write(block)
                    digest.update(block)
                    size += len(block)
            files.append(
                {
                    "path": relative,
                    "size": size,
                    "sha256": digest.hexdigest(),
                    "member_sha256": _cargo_member_digest(archive, info),
                    "mtime_ns": path.stat().st_mtime_ns,
                }
            )
        embedded = json.dumps(
            {"version": _CARGO_SNAPSHOT_VERSION, "files": files},
            ensure_ascii=True,
            sort_keys=True,
        ).encode()
        archive.writestr(_cargo_zip_info("metadata.json"), embedded, compresslevel=1)
    snapshot: _CargoManifest = {
        "version": _CARGO_SNAPSHOT_VERSION,
        "files": files,
        "payload_sha256": _cargo_digest(payload),
    }
    manifest.parent.mkdir(parents=True, exist_ok=True)
    _ = manifest.write_text(json.dumps(snapshot, ensure_ascii=True, sort_keys=True) + "\n", encoding="utf-8")


def _cargo_snapshot_record(raw: object) -> _CargoFile:
    if not isinstance(raw, dict):
        message = "Cargo snapshot 文件记录不是对象"
        raise TypeError(message)
    record = cast("dict[str, object]", raw)
    relative, size, digest = record.get("path"), record.get("size"), record.get("sha256")
    mtime = record.get("mtime_ns")
    member_digest = record.get("member_sha256")
    if (
        set(record) != {"path", "size", "sha256", "member_sha256", "mtime_ns"}
        or not isinstance(relative, str)
        or type(size) is not int
        or not isinstance(digest, str)
        or not isinstance(member_digest, str)
        or type(mtime) is not int
    ):
        message = "Cargo snapshot 文件元数据类型无效"
        raise TypeError(message)
    if (
        not 0 <= size <= _CARGO_MAX_FILE_SIZE
        or not 0 <= mtime <= _CARGO_MAX_MTIME
        or not _cargo_valid_digest(digest)
        or not _cargo_valid_digest(member_digest)
    ):
        message = "Cargo snapshot 文件长度、时间或摘要无效"
        raise ValueError(message)
    return {"path": relative, "size": size, "sha256": digest, "member_sha256": member_digest, "mtime_ns": mtime}


def _cargo_valid_digest(value: object) -> bool:
    return (
        isinstance(value, str)
        and len(value) == _PAYLOAD_HASH_LENGTH
        and all(character in "0123456789abcdef" for character in value)
    )


def _cargo_json_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    value: dict[str, object] = {}
    for key, item in pairs:
        if key in value:
            message = "Cargo snapshot JSON 含重复字段"
            raise ValueError(message)
        value[key] = item
    return value


def _cargo_archive_members(archive: zipfile.ZipFile, files: list[_CargoFile], size: int) -> None:
    members = archive.infolist()
    names = [member.filename for member in members]
    if len(set(names)) != len(names) or set(names) != {"metadata.json", *(record["path"] for record in files)}:
        message = "Cargo snapshot archive 成员重复、缺失或未声明"
        raise ValueError(message)
    for info in members:
        if (
            info.is_dir()
            or info.compress_type not in {zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED}
            or info.flag_bits & 1
            or info.header_offset < 0
            or info.header_offset + info.compress_size > size
            or stat.S_IFMT(info.external_attr >> 16) not in {0, stat.S_IFREG}
        ):
            message = "Cargo snapshot archive 类型或大小边界无效"
            raise ValueError(message)
        # open 只读本地头：校验名称/重叠边界，但不解压运行文件。
        with archive.open(info):
            pass


def _cargo_snapshot_metadata(archive: zipfile.ZipFile, files: list[_CargoFile]) -> None:
    metadata = archive.getinfo("metadata.json")
    if metadata.file_size > _CARGO_MAX_METADATA_SIZE:
        message = "Cargo snapshot 内嵌文件表过大"
        raise ValueError(message)
    embedded = archive.read(metadata)
    expected = json.dumps(
        {"version": _CARGO_SNAPSHOT_VERSION, "files": files},
        ensure_ascii=True,
        sort_keys=True,
    ).encode()
    if embedded != expected:
        message = "Cargo snapshot 外部文件表与 payload 不匹配"
        raise ValueError(message)


def _cargo_snapshot_digest(archive: zipfile.ZipFile, files: list[_CargoFile], expected: object) -> None:
    # 同一顺序扫描同时验证整包 SHA 与每个压缩成员/结构 SHA，避免大型 payload 重读。
    source = archive.fp
    if source is None:
        message = "Cargo snapshot archive 已关闭"
        raise ValueError(message)
    digest = hashlib.sha256()
    _ = source.seek(0)
    for record in sorted(files, key=lambda item: archive.getinfo(item["path"]).header_offset):
        info = archive.getinfo(record["path"])
        if _cargo_member_digest(archive, info, observe=digest.update) != record["member_sha256"]:
            message = "Cargo snapshot 压缩成员或结构摘要不匹配"
            raise ValueError(message)
    while block := source.read(1024 * 1024):
        digest.update(block)
    if digest.hexdigest() != expected:
        message = "Cargo snapshot 压缩 payload 摘要不匹配"
        raise ValueError(message)


def _cargo_manifest_records(
    root: Path, directory: Path, manifest: Path
) -> tuple[Path, list[tuple[Path, _CargoFile]], str]:
    """只解析并校验 manifest 文件表与路径边界；不打开归档，供并行现有产物校验复用."""
    raw = cast("object", json.loads(manifest.read_text(encoding="utf-8"), object_pairs_hook=_cargo_json_object))
    if not isinstance(raw, dict):
        message = "Cargo snapshot manifest 不是对象"
        raise TypeError(message)
    snapshot = cast("dict[str, object]", raw)
    records = snapshot.get("files")
    if (
        set(snapshot) != {"version", "files", "payload_sha256"}
        or type(snapshot.get("version")) is not int
        or snapshot["version"] != _CARGO_SNAPSHOT_VERSION
        or not isinstance(records, list)
        or not _cargo_valid_digest(snapshot.get("payload_sha256"))
    ):
        message = "Cargo snapshot manifest 版本或文件清单无效"
        raise ValueError(message)
    payload = (directory / "payload.zip").resolve(strict=True)
    if not payload.is_relative_to(directory):
        message = "Cargo snapshot 压缩 payload 摘要不匹配"
        raise ValueError(message)
    files = [_cargo_snapshot_record(record) for record in cast("list[object]", records)]
    validated: list[tuple[Path, _CargoFile]] = []
    seen: set[str] = set()
    total = 0
    for record in files:
        target = _cargo_path(root, record["path"])
        key = os.path.normcase(str(target))
        total += record["size"]
        if key in seen or total > _CARGO_MAX_TOTAL_SIZE:
            message = "Cargo snapshot 文件表或 archive 大小边界无效"
            raise ValueError(message)
        seen.add(key)
        validated.append((target, record))
    return payload, validated, str(snapshot["payload_sha256"])


def _cargo_snapshot_paths(root: Path, directory: Path, manifest: Path) -> list[tuple[Path, Path, _CargoFile]]:
    payload, records, expected = _cargo_manifest_records(root, directory, manifest)
    files = [record for _, record in records]
    with zipfile.ZipFile(payload) as archive:
        _cargo_archive_members(archive, files, payload.stat().st_size)
        _cargo_snapshot_metadata(archive, files)
        validated: list[tuple[Path, Path, _CargoFile]] = []
        for target, record in records:
            info = archive.getinfo(record["path"])
            if info.file_size != record["size"]:
                message = "Cargo snapshot 文件表或 archive 大小边界无效"
                raise ValueError(message)
            validated.append((payload, target, record))
        _cargo_snapshot_digest(archive, files, expected)
    return validated


def _cargo_stage_file(payload: Path, record: _CargoFile, staging: Path) -> None:
    digest = hashlib.sha256()
    size = 0
    with zipfile.ZipFile(payload) as archive, archive.open(record["path"]) as source, staging.open("wb") as writer:
        while block := source.read(1024 * 1024):
            size += len(block)
            if size > record["size"]:
                message = "Cargo snapshot 解压长度越界"
                raise ValueError(message)
            digest.update(block)
            _ = writer.write(block)
    if size != record["size"] or digest.hexdigest() != record["sha256"]:
        message = "Cargo snapshot 解压长度或摘要不匹配"
        raise ValueError(message)
    # 不传播 Bazel payload 的只读属性；保留原始产物时间。
    os.utime(staging, ns=(staging.stat().st_atime_ns, record["mtime_ns"]))


def _cargo_target_matches(target: Path, record: _CargoFile) -> bool:
    return target.is_file() and target.stat().st_size == record["size"] and _cargo_digest(target) == record["sha256"]


def _cargo_target_untouched(target: Path, record: _CargoFile) -> bool:
    """按文件表声明的长度与 mtime 检测门内写入；外部改动按合同 P-08 不在防御范围.

    恢复写入会保留快照声明的 mtime，因此全部一致即表示自快照发布以来没有
    本仓库门进程改写过该文件，热轮无需重读产物与整包字节。
    """
    try:
        information = target.stat()
    except OSError:
        return False
    return information.st_size == record["size"] and information.st_mtime_ns == record["mtime_ns"]


def _cargo_target_entry_matches(entry: tuple[Path, tuple[Path, _CargoFile]]) -> bool:
    return _cargo_target_matches(entry[0], entry[1][1])


def _cargo_verified_targets(
    root: Path, snapshots: list[tuple[Path, Path]]
) -> dict[Path, tuple[Path, _CargoFile, bool]]:
    """完成全部归档校验与现有产物摘要校验；返回每项的来源与是否已匹配.

    热轮先用文件表声明的长度与 mtime 检测门内写入：全部未动过时直接判定匹配，
    不重读产物字节，也不打开压缩包。任一文件缺失、长度或时间不符时回退完整
    校验——归档校验与产物摘要并行进行，任一失败都在返回前抛出。
    """
    targets: dict[Path, tuple[Path, _CargoFile]] = {}
    for directory, manifest in snapshots:
        payload, records, _expected = _cargo_manifest_records(root, directory.resolve(strict=True), manifest)
        for target, record in records:
            targets[target] = (payload, record)
    if all(_cargo_target_untouched(target, record) for target, (_, record) in targets.items()):
        return {target: (payload, record, True) for target, (payload, record) in targets.items()}
    with ThreadPoolExecutor(max_workers=min(16, (os.cpu_count() or 4))) as pool:
        archives = [
            pool.submit(_cargo_snapshot_paths, root, directory.resolve(strict=True), manifest)
            for directory, manifest in snapshots
        ]
        matched = dict(zip(targets, pool.map(_cargo_target_entry_matches, targets.items()), strict=True))
    for future in archives:
        _ = future.result()
    return {target: (payload, record, matched[target]) for target, (payload, record) in targets.items()}


def restore_cargo_artifact_snapshots(root: Path, snapshots: list[tuple[Path, Path]]) -> None:
    """验证每批整包/成员绑定，再暂存最后变体；匹配消费者不解压，全部通过才写入.

    文件表是 Bazel 生产动作的产物完整性声明，不是测试通过证据或 Cargo 指纹缓存。
    需要恢复的成员仍校验实际解压字节的文件 SHA。归档校验与现有产物摘要校验
    只并行读取互不相同的文件，不改变任何检查内容或写入边界。
    """
    root = root.resolve(strict=True)
    temporary = root / ".tmp" / "bazel-gate"
    staged: list[tuple[Path, Path]] = []
    try:
        targets = _cargo_verified_targets(root, snapshots)
        if not temporary.is_dir():
            message = "缺少 make_tmp.py 创建的 .tmp/bazel-gate 工作目录"
            raise ValueError(message)
        for target, (payload, record, matches) in targets.items():
            if matches:
                continue
            staging = temporary / ("cargo-restore-" + uuid.uuid4().hex)
            staged.append((staging, target))
            _cargo_stage_file(payload, record, staging)
        for staging, target in staged:
            target.parent.mkdir(parents=True, exist_ok=True)
            if target.is_file() and not target.stat().st_mode & 0o200:
                target.chmod(target.stat().st_mode | 0o200)
            _ = staging.replace(target)
    except (zipfile.BadZipFile, zipfile.LargeZipFile, zlib.error, NotImplementedError) as error:
        message = "Cargo snapshot 压缩 payload 结构或 CRC 无效"
        raise ValueError(message) from error
    finally:
        for staging, _target in staged:
            staging.unlink(missing_ok=True)


def restore_cargo_artifacts(root: Path, directory: Path, manifest: Path) -> None:
    """先验证完整快照，再原子恢复真实消费者路径；不修改 Cargo 指纹数据库."""
    restore_cargo_artifact_snapshots(root, [(directory, manifest)])


def test_log_payload(native: bytes) -> bytes:
    """只解码适配器的完整帧，不把 Bazel wrapper 或损坏日志当成原始输出."""
    start = native.find(_PAYLOAD_PREFIX)
    if start < 0:
        message = "Bazel test.log 缺少适配器 payload frame"
        raise ValueError(message)
    header_start = start + len(_PAYLOAD_PREFIX)
    header_end = native.find(b"\n", header_start)
    fields = native[header_start:header_end].split(b" ") if header_end >= 0 else []
    if len(fields) != _PAYLOAD_HEADER_FIELDS or not fields[0].isdigit() or len(fields[1]) != _PAYLOAD_HASH_LENGTH:
        message = "Bazel test.log payload header 无效"
        raise ValueError(message)
    body_start = header_end + 1
    body_end = body_start + int(fields[0])
    body = native[body_start:body_end]
    if native[body_end:] != _PAYLOAD_END or hashlib.sha256(body).hexdigest().encode("ascii") != fields[1]:
        message = "Bazel test.log payload 长度、摘要或结束标记无效"
        raise ValueError(message)
    return body


def _notify(path: str | None, detail: str) -> None:
    if path:
        with Path(path).open("a", encoding="utf-8") as signal:
            _ = signal.write(json.dumps({"detail": detail}) + "\n")


def _artifact_valid(path: Path) -> bool:
    with path.open(encoding="utf-8", errors="replace") as source:
        for line in source:
            try:
                value = cast("object", json.loads(line))
            except ValueError:
                continue
            if not isinstance(value, dict):
                continue
            artifact = cast("CargoArtifact", cast("object", value))
            if (
                artifact.get("reason") == "compiler-artifact"
                and artifact.get("profile", {}).get("test") is True
                and isinstance(artifact.get("target", {}).get("name"), str)
                and artifact.get("target", {}).get("name") != "gui_flow"
                and isinstance(artifact.get("executable"), str)
            ):
                return True
    return False


def _observe_libtest(path: Path, offset: int, failure: str | None) -> tuple[int, str | None]:
    if not path.exists():
        return offset, failure
    with path.open("rb") as source:
        _ = source.seek(offset)
        while line := source.readline():
            if not line.endswith(b"\n"):
                break
            offset = source.tell()
            if line.startswith((b"failed ", b"failed:")) and failure:
                _notify(failure, line.decode("utf-8", errors="replace").strip()[:1024])
                return offset, None
    return offset, failure


def _finish(spec: GateSpec, log: Path, code: int, failure: str | None) -> int:
    if code == 0 and spec["name"].endswith("-test-build") and not _artifact_valid(log):
        _notify(failure, f"{spec['name']}-artifacts: Cargo 未发布有效测试程序。")
        code = 1
    # 沿用门的绑定环判据，在动作完成时阻止后续原语启动。
    if code == 0 and "binding loop" in log.read_text(encoding="utf-8", errors="replace"):
        _notify(failure, f"{spec['name']}: 出现 binding loop 诊断")
        code = 1
    return code


def _publish_log(log: Path, declared_log: Path | None, code: int) -> int:
    if declared_log is not None:
        if code == 0:
            # 声明输出保存完整原始字节；空日志同样物化，由原生 BEP 暴露。
            _ = shutil.copyfile(log, declared_log)
        return code
    # Bazel 的 Windows wrapper 会给 test.log 加前言；严格帧保留完整原始字节，
    # 包括合法空清单，长度和摘要使截断/损坏不能恢复成成功结果。
    with log.open("rb") as source:
        digest = hashlib.file_digest(source, "sha256").hexdigest()
    _ = sys.stdout.buffer.write(_PAYLOAD_PREFIX + f"{log.stat().st_size} {digest}\n".encode("ascii"))
    with log.open("rb") as source:
        while chunk := source.read(65536):
            _ = sys.stdout.buffer.write(chunk)
    _ = sys.stdout.buffer.write(_PAYLOAD_END)
    _ = sys.stdout.buffer.flush()
    return code


def _invoke(
    spec: GateSpec, live: LiveRuntime, argv: list[str], env: dict[str, str], failure: str | None
) -> tuple[Path, int, str | None]:
    log = Path(live["log"])
    log.parent.mkdir(parents=True, exist_ok=True)
    phase = live.get("phase_file")
    if phase is not None:
        _ = Path(phase).write_text(
            json.dumps({"compiling": spec["kind"] in {"compile", "compiler-observed"}}), encoding="utf-8"
        )
    offset = 0
    try:
        with log.open("wb") as sink:
            try:
                process = subprocess.Popen(argv, cwd=Path(spec["root"]), env=env, stdout=sink, stderr=subprocess.STDOUT)
                while process.poll() is None:
                    if spec["watch_test_failures"]:
                        offset, failure = _observe_libtest(Path(live["result_file"]), offset, failure)
                    time.sleep(0.02)
                code = process.wait()
                if code:
                    _notify(failure, f"{spec['name']} exit={code}")
            except (OSError, ValueError, KeyError, TypeError) as error:
                # 在关闭日志或复制输出之前通知，避免外层仍启动后续命令。
                _notify(failure, f"{spec['name']}：{type(error).__name__}: {error}")
                raise
    finally:
        if phase is not None:
            _ = Path(phase).write_text('{"compiling":false}', encoding="utf-8")
    return log, code, failure


def _publish_unused_inputs(spec: GateSpec, environment: dict[str, str], log: Path) -> None:
    output = os.environ.get("JCHTOOLS_GATE_UNUSED_INPUTS")
    inventory = os.environ.get("JCHTOOLS_GATE_INPUT_INVENTORY")
    if output is None or inventory is None:
        return
    # 启动失败通知只依赖标准库；仅成功 Cargo 动作才需要已声明的解析模块。
    resolver = cast(
        "Callable[[Path, list[str], list[str], dict[str, str], Path], list[str]]",
        vars(importlib.import_module("scripts.gate_inputs"))["cargo_unused_inputs"],
    )
    unused: list[str] = []
    try:
        values = cast("object", json.loads(Path(inventory).read_text(encoding="utf-8")))
        if isinstance(values, list) and all(isinstance(value, str) for value in cast("list[object]", values)):
            unused = resolver(Path(spec["root"]), cast("list[str]", values), spec["argv"], environment, log)
    except (OSError, ValueError, TypeError):
        # Unknown dependency metadata must keep the full declared input set.
        unused = []
    _ = Path(output).write_text("".join(value + "\n" for value in unused), encoding="utf-8")


def _run() -> int:
    # 构建输出相对执行根解析，不能受原始命令的工作目录影响。
    declared_log = Path(sys.argv[2]).resolve() if sys.argv[2:] else None
    spec_path = Path(sys.argv[1])
    spec = cast("GateSpec", json.loads(spec_path.read_text(encoding="utf-8")))
    live = cast("LiveRuntime", json.loads(spec_path.with_suffix(".runtime.json").read_text(encoding="utf-8")))
    env = dict(spec["environment"])
    env.update(live["environment"])
    # 只继承动态 TEMP 和失败通知地址，不让 Bazel 测试环境改变既有检查行为。
    for key in ("TEMP", "TMP"):
        value = live["TEMP"] if key == "TEMP" else live["TMP"]
        if value:
            env[key] = value
    outer_failure = os.environ.get("JCHTOOLS_GATE_FAILURE_FILE")
    # --test_env 不传每轮独占路径，防止破坏结果复用；地址通过独占元数据传入。
    outer_failure = live.get("failure_file") or outer_failure
    if outer_failure:
        env["JCHTOOLS_GATE_FAILURE_FILE"] = outer_failure
    argv = list(spec["argv"])
    artifact_outputs: tuple[Path, Path] | None = None
    if spec.get("cargo_artifacts", False):
        if len(sys.argv[3:]) != _CARGO_OUTPUT_COUNT or not cargo_cache_supported(argv, spec["kind"], env):
            message = "Cargo snapshot 声明与编译原语不匹配"
            raise ValueError(message)
        artifact_outputs = (Path(sys.argv[3]).resolve(), Path(sys.argv[4]).resolve())
        if not any(argument == "--message-format" or argument.startswith("--message-format=") for argument in argv):
            # clippy 的 -- 后属于 rustc 参数，必须在分隔符前插入 Cargo 选项。
            position = argv.index("--") if "--" in argv else len(argv)
            argv.insert(position, "--message-format=json")
    elif sys.argv[3:]:
        message = "未声明 Cargo snapshot 的动作不能接收产物输出"
        raise ValueError(message)
    result_file = Path(live["result_file"])
    if spec["watch_test_failures"]:
        argv += ["--logfile", str(result_file)]
    # 即使 Bazel 尚未发布原生输出就被终止，仍保留原始命令日志。
    log, code, outer_failure = _invoke(spec, live, argv, env, outer_failure)
    code = _finish(spec, log, code, outer_failure)
    if code == 0 and artifact_outputs is not None:
        capture_cargo_artifacts(Path(spec["root"]), log, *artifact_outputs)
        _publish_unused_inputs(spec, env, log)
    return _publish_log(log, declared_log, code)


def main() -> int:
    try:
        return _run()
    except (OSError, ValueError, KeyError, TypeError) as error:
        # 启动或配置异常同样须即时通知外层，不能等待其他原生测试完成。
        failure = os.environ.get("JCHTOOLS_GATE_FAILURE_FILE")
        if not failure:
            live = cast(
                "LiveRuntime",
                json.loads(Path(sys.argv[1]).with_suffix(".runtime.json").read_text(encoding="utf-8")),
            )
            failure = live.get("failure_file")
        # 通知地址来自本动作 sidecar；读取或通知失败必须直接传播，不能扫描其他门或吞掉权限错误。
        _notify(failure, f"Bazel 动作准备失败：{type(error).__name__}: {error}")
        print(f"Bazel 动作失败：{type(error).__name__}: {error}", file=sys.stderr, flush=True)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
