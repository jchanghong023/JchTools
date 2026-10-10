"""为 markdown_media_e2e 准备公开合成语音与真正无音轨的 MP4.

只准备夹具，不调用转录模型；期望正文与语音原文在识别前确定。
下载、工具解包、语音及子进程临时文件全部落在仓库 make_tmp 工作目录。
便携 FFmpeg 来自 ffmpeg.org/download.html 指向的 gyan.dev，
固定 SHA-256 取自发布方对应版本的 .zip.sha256 文件。
不安装工具、不改 PATH、不播放声音、不操作 GUI、不生成 Python COM 缓存。

用法：python scripts/media_e2e_fixture.py --component-dir <Xberg 目录>
      --output <.tmp 内绝对 JSON 路径>
输出包含描述字段和原测试的四个环境变量。
fixture-receipt.json 保存预定语音、身份、摘要与 ffprobe 旁证；
保留工作目录以复用已校验产物，只清理 .tmp/media-e2e-fixture 与显式输出 JSON。
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import io
import json
import os
import shutil
import subprocess
import sys
import wave
import zipfile
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from collections.abc import Callable
    from typing import TypeIs

ROOT = Path(__file__).resolve().parent.parent
WORKSPACE = ROOT / ".tmp" / "media-e2e-fixture"
VERSION = "9.0.2"
ARCHIVE_NAME = f"ffmpeg-{VERSION}-essentials_build.zip"
ARCHIVE_URL = f"https://www.gyan.dev/ffmpeg/builds/packages/{ARCHIVE_NAME}"
ARCHIVE_SHA256 = "60f467265b1e312373dbcd92200c2618a74850f98d3d078e94296bb3fa2047ba"
HASH_SOURCE = f"{ARCHIVE_URL}.sha256"
SPEECH = {
    "804": (
        "今天的天气很好。我们一起学习，认真工作。欢迎使用语音转文字工具。",
        "今天的天气很好,欢迎使用语音转文字工具",
    ),
    "409": (
        "The weather is very nice today. We learn together and work carefully. Welcome to the speech test.",
        "Theweatherisverynicetoday,theweatherisverynicetoday",
    ),
}
MIN_SPEECH_SECONDS = 5
PCM_SAMPLE_BYTES = 2
_parse_json: Callable[[str], object] = json.loads

# 不创建 Python COM 包装器；只读身份与原生语音资产摘要。
# GetVoices 仅枚举；Speak 必须在绑定文件输出流后调用。
VOICE_QUERY = r"""
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = New-Object Text.UTF8Encoding($false)
$v = New-Object -ComObject SAPI.SpVoice
$voices = @($v.GetVoices())
$t = $null
foreach ($language in @('804', '409')) {
    $t = $voices | Where-Object { $_.GetAttribute('Language') -eq $language } |
        Sort-Object { $_.Id } | Select-Object -First 1
    if ($null -ne $t) { break }
}
if ($null -eq $t) { throw '未安装中文或英文 SAPI 语音' }
$p = Get-ItemProperty -LiteralPath ('Registry::' + $t.Id)
$assets = @{}
$voicePath = [Environment]::ExpandEnvironmentVariables($p.VoicePath)
$prefix = [IO.Path]::GetFileName($voicePath)
$files = @(Get-ChildItem -LiteralPath ([IO.Path]::GetDirectoryName($voicePath)) -File |
    Where-Object { $_.Name.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase) })
$files += Get-Item -LiteralPath ([Environment]::ExpandEnvironmentVariables($p.LangDataPath))
$engine = (Get-Item -LiteralPath ('Registry::HKEY_CLASSES_ROOT\CLSID\' + $p.CLSID + '\InprocServer32')).GetValue('')
$files += Get-Item -LiteralPath ([Environment]::ExpandEnvironmentVariables($engine))
foreach ($f in $files) { $assets[$f.FullName] = (Get-FileHash -LiteralPath $f.FullName -Algorithm SHA256).Hash }
if ($assets.Count -lt 2) { throw 'SAPI 语音资产缺失' }
@{id=$t.Id; name=$t.GetDescription(); language=$t.GetAttribute('Language'); assets=$assets} |
    ConvertTo-Json -Depth 5 -Compress
"""
VOICE_SYNTHESIS = r"""
$ErrorActionPreference = 'Stop'
$c = $env:JCHTOOLS_FIXTURE_SPEECH_JSON | ConvertFrom-Json
$v = New-Object -ComObject SAPI.SpVoice
$t = @($v.GetVoices()) | Where-Object { $_.Id -ceq $c.voice_id } | Select-Object -First 1
if ($null -eq $t) { throw '选定的 SAPI 语音已不存在' }
$v.Voice = $t
$v.Rate = 0
$v.Volume = 100
$s = New-Object -ComObject SAPI.SpFileStream
$s.Format.Type = 22
try {
    $s.Open($c.wav, 3, $false)
    $v.AudioOutputStream = $s
    $null = $v.Speak($c.text, 0)
} finally { $s.Close() }
"""


class Arguments(argparse.Namespace):
    component_dir: str = ""
    output: str = ""


def object_map(value: object) -> TypeIs[dict[str, object]]:
    return isinstance(value, dict)


def object_list(value: object) -> TypeIs[list[object]]:
    return isinstance(value, list)


def digest_file(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def temporary_path(path: Path) -> Path:
    resolved = path.resolve()
    if not resolved.is_relative_to(ROOT / ".tmp") or resolved == ROOT / ".tmp":
        message = f"夹具写入必须限于仓库 .tmp 内：{resolved}"
        raise ValueError(message)
    return resolved


def run(argv: list[str], environment: dict[str, str], *, timeout: int = 120) -> str:
    result = subprocess.run(
        argv, env=environment, capture_output=True, encoding="utf-8", errors="replace", check=False, timeout=timeout
    )
    if result.returncode:
        message = f"命令失败（{result.returncode}）：{argv[0]}\n{result.stderr[-2000:]}"
        raise RuntimeError(message)
    return result.stdout


def workspace(path: Path, environment: dict[str, str]) -> None:
    _ = run(
        [sys.executable, str(ROOT / "scripts" / "make_tmp.py"), "workspace", "--destination", str(path)], environment
    )


def powershell(command: str, executable: str, environment: dict[str, str]) -> str:
    encoded = base64.b64encode(command.encode("utf-16-le")).decode("ascii")
    return run([executable, "-NoProfile", "-NonInteractive", "-EncodedCommand", encoded], environment)


def portable_tools(environment: dict[str, str]) -> tuple[Path, Path]:
    archive = WORKSPACE / ARCHIVE_NAME
    if not archive.is_file() or digest_file(archive) != ARCHIVE_SHA256:
        curl = shutil.which("curl.exe")
        if curl is None:
            message = "缺少用于下载固定便携 FFmpeg 的 Windows curl.exe"
            raise FileNotFoundError(message)
        partial = archive.with_suffix(".zip.download")
        _ = run(
            [
                curl,
                "--fail",
                "--location",
                "--proto",
                "=https",
                "--proto-redir",
                "=https",
                "--output",
                str(partial),
                ARCHIVE_URL,
            ],
            environment,
            timeout=600,
        )
        if digest_file(partial) != ARCHIVE_SHA256:
            message = f"便携 FFmpeg SHA-256 不匹配，未执行的下载保留于：{partial}"
            raise ValueError(message)
        _ = partial.replace(archive)
    tools = WORKSPACE / "tools"
    workspace(tools, environment)
    with zipfile.ZipFile(archive) as package:
        for name in ("ffmpeg.exe", "ffprobe.exe"):
            member = f"ffmpeg-{VERSION}-essentials_build/bin/{name}"
            with package.open(member) as source:
                digest = hashlib.sha256()
                for block in iter(lambda: source.read(1024 * 1024), b""):
                    digest.update(block)
                expected = digest.hexdigest()
            target = tools / name
            if not target.is_file() or digest_file(target) != expected:
                with package.open(member) as source, target.open("wb") as output:
                    shutil.copyfileobj(source, output)
                if digest_file(target) != expected:
                    message = f"解包后的 FFmpeg 工具身份不匹配：{target}"
                    raise ValueError(message)
    return tools / "ffmpeg.exe", tools / "ffprobe.exe"


def voice_identity(executable: str, environment: dict[str, str]) -> dict[str, object]:
    value = _parse_json(powershell(VOICE_QUERY, executable, environment))
    if (
        not object_map(value)
        or not isinstance(value.get("id"), str)
        or not isinstance(value.get("language"), str)
        or value.get("language") not in SPEECH
    ):
        message = "SAPI 返回的语音身份无效"
        raise ValueError(message)
    return value


def probe(path: Path, executable: Path, environment: dict[str, str], *, speech: bool) -> dict[str, object]:
    value = _parse_json(
        run([str(executable), "-v", "error", "-show_format", "-show_streams", "-of", "json", str(path)], environment)
    )
    if not object_map(value):
        message = f"ffprobe 未返回媒体对象：{path}"
        raise ValueError(message)
    streams = value.get("streams")
    container = value.get("format")
    if not object_list(streams) or not object_map(container) or "mp4" not in str(container.get("format_name")):
        message = f"不是真实 MP4 容器：{path}"
        raise ValueError(message)
    audio = [stream for stream in streams if object_map(stream) and stream.get("codec_type") == "audio"]
    video = [stream for stream in streams if object_map(stream) and stream.get("codec_type") == "video"]
    if len(video) != 1 or video[0].get("codec_name") != "mpeg4":
        message = f"缺少固定配置的 MPEG-4 视频轨道：{path}"
        raise ValueError(message)
    if speech:
        if (
            len(audio) != 1
            or audio[0].get("codec_name") != "aac"
            or audio[0].get("channels") != 1
            or audio[0].get("sample_rate") != "16000"
            or float(str(container.get("duration", "0"))) < MIN_SPEECH_SECONDS
        ):
            message = f"缺少真实语音音轨：{path}"
            raise ValueError(message)
    elif audio:
        message = f"无音轨夹具意外包含音轨：{path}"
        raise ValueError(message)
    return value


def synthesize(configuration: dict[str, object], ffmpeg: Path, executable: str, environment: dict[str, str]) -> None:
    wav = WORKSPACE / "spoken.wav"
    speech_environment = {
        **environment,
        "JCHTOOLS_FIXTURE_SPEECH_JSON": json.dumps(
            {
                "voice_id": configuration["voice_id"],
                "text": configuration["spoken_text"],
                "wav": str(wav),
            },
            ensure_ascii=False,
        ),
    }
    _ = powershell(VOICE_SYNTHESIS, executable, speech_environment)
    with wave.open(str(wav), "rb") as recording:
        duration = recording.getnframes() / recording.getframerate()
        samples = recording.readframes(recording.getnframes())
        if duration < MIN_SPEECH_SECONDS or not any(samples) or recording.getsampwidth() != PCM_SAMPLE_BYTES:
            message = "SAPI 未生成非空 16 位语音 PCM"
            raise ValueError(message)
    common = [str(ffmpeg), "-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-threads", "1"]
    # 复用 markdown_acceptance._synth_media 的 lavfi color / MPEG-4 原语；
    # 不导入 GUI 模块，避免引入无关的 pywinauto / Pillow 依赖。
    _ = run(
        [
            *common,
            "-f",
            "lavfi",
            "-i",
            "color=c=black:size=64x64:rate=25",
            "-i",
            str(wav),
            "-map",
            "0:v:0",
            "-map",
            "1:a:0",
            "-c:v",
            "mpeg4",
            "-pix_fmt",
            "yuv420p",
            "-af",
            "adelay=500,apad=pad_dur=0.5",
            "-ar",
            "16000",
            "-ac",
            "1",
            "-c:a",
            "aac",
            "-b:a",
            "64k",
            "-shortest",
            "-movflags",
            "+faststart",
            str(WORKSPACE / "speech.mp4"),
        ],
        environment,
    )
    _ = run(
        [
            *common,
            "-f",
            "lavfi",
            "-i",
            "color=c=black:size=64x64:duration=1",
            "-an",
            "-c:v",
            "mpeg4",
            "-pix_fmt",
            "yuv420p",
            "-movflags",
            "+faststart",
            str(WORKSPACE / "trackless.mp4"),
        ],
        environment,
    )


def reusable(receipt: Path, config_sha256: str, files: list[Path]) -> bool:
    if not receipt.is_file() or not all(path.is_file() for path in files):
        return False
    value = _parse_json(receipt.read_text(encoding="utf-8"))
    if not object_map(value) or value.get("config_sha256") != config_sha256:
        return False
    hashes = value.get("files_sha256")
    return object_map(hashes) and all(hashes.get(path.name) == digest_file(path) for path in files)


def prepare(component: Path, output: Path) -> None:
    if not component.is_absolute() or not (component / "xberg.exe").is_file():
        message = "--component-dir 必须是包含 xberg.exe 的绝对目录"
        raise ValueError(message)
    component = component.resolve()
    if not output.is_absolute():
        message = "--output 必须是仓库 .tmp 内的绝对路径"
        raise ValueError(message)
    _ = temporary_path(WORKSPACE)
    output = temporary_path(output)
    environment = {**os.environ, "TEMP": str(WORKSPACE), "TMP": str(WORKSPACE), "PYTHONDONTWRITEBYTECODE": "1"}
    workspace(WORKSPACE, environment)
    workspace(output.parent, environment)
    executable = shutil.which("powershell.exe")
    if executable is None:
        message = "文件输出式 SAPI 语音合成需要 Windows PowerShell"
        raise FileNotFoundError(message)
    ffmpeg, ffprobe = portable_tools(environment)
    voice = voice_identity(executable, environment)
    language = str(voice["language"])
    spoken_text, expect_text = SPEECH[language]
    configuration: dict[str, object] = {
        "script_sha256": digest_file(Path(__file__)),
        "component_dir": str(component),
        "engine_sha256": digest_file(component / "xberg.exe"),
        "voice": voice,
        "voice_id": voice["id"],
        "spoken_text": spoken_text,
        "expect_text": expect_text,
        "sapi_format": 22,
        "rate": 0,
        "volume": 100,
        "ffmpeg_sha256": digest_file(ffmpeg),
        "ffprobe_sha256": digest_file(ffprobe),
        "powershell_sha256": digest_file(Path(executable)),
        "archive_sha256": ARCHIVE_SHA256,
    }
    config_sha256 = hashlib.sha256(json.dumps(configuration, sort_keys=True, ensure_ascii=False).encode()).hexdigest()
    inputs = [WORKSPACE / "spoken.wav", WORKSPACE / "speech.mp4", WORKSPACE / "trackless.mp4"]
    receipt = WORKSPACE / "fixture-receipt.json"
    reused = reusable(receipt, config_sha256, inputs)
    if not reused:
        synthesize(configuration, ffmpeg, executable, environment)
    evidence = {
        "speech": probe(inputs[1], ffprobe, environment, speech=True),
        "trackless": probe(inputs[2], ffprobe, environment, speech=False),
    }
    _ = receipt.write_text(
        json.dumps(
            {
                "configuration": configuration,
                "config_sha256": config_sha256,
                "files_sha256": {path.name: digest_file(path) for path in inputs},
                "media": evidence,
                "archive_url": ARCHIVE_URL,
                "hash_source": HASH_SOURCE,
                "archive_bytes": (WORKSPACE / ARCHIVE_NAME).stat().st_size,
            },
            ensure_ascii=False,
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )
    values = {
        "component_dir": str(component),
        "input": str(inputs[1]),
        "trackless_input": str(inputs[2]),
        "expect_text": expect_text,
    }
    values.update({f"JCHTOOLS_MEDIA_E2E_{name.upper()}": value for name, value in values.copy().items()})
    _ = output.write_text(json.dumps(values, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(
        json.dumps(
            {
                "output": str(output),
                "receipt": str(receipt),
                "reused": reused,
                "spoken_text": spoken_text,
                "expect_text": expect_text,
            },
            ensure_ascii=False,
        )
    )


def main() -> int:
    if isinstance(sys.stdout, io.TextIOWrapper):
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    parser = argparse.ArgumentParser(description=__doc__)
    _ = parser.add_argument("--component-dir", required=True)
    _ = parser.add_argument("--output", required=True)
    arguments = parser.parse_args(namespace=Arguments())
    if sys.platform != "win32":
        print("FAIL：夹具准备仅支持 Windows", file=sys.stderr)
        return 2
    try:
        prepare(Path(arguments.component_dir), Path(arguments.output))
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError, zipfile.BadZipFile, wave.Error) as error:
        print(f"FAIL：媒体夹具准备失败：{error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
