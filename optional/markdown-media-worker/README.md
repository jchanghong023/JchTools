# markdown-media-worker

这是 JchTools「转 Markdown」功能的可选媒体工作进程。它是独立 Rust crate，不加入主 `Cargo.toml`，也不要求 Python、PyAV、NumPy 或 `sherpa_onnx` Python 包。

## 运行接口

主程序启动：

```text
markdown-media-worker.exe --input <单个 .mp4/.m4a> --models <媒体模型根目录>
```

成功时 stdout 只输出一行 JSON：`{"markdown":"..."}`。错误写入 stderr 并以非零退出；主程序负责单文件超时和终止子进程。

## 可选资产目录

初始化器将 worker 与媒体运行时分开安装。下面的 `<asset-root>` 是 JchTools 的资产根目录，实际位置由主程序状态目录决定：

```text
<asset-root>/
  worker/v0.1.0/
    markdown-media-worker.exe
  media/sherpa-onnx/v1.13.6/
    ffmpeg/ffmpeg.exe
    sherpa-onnx-c-api.dll
    sherpa-onnx-cxx-api.dll
    onnxruntime.dll
    onnxruntime_providers_shared.dll
    models/
      sense_voice_zh_en_ja_ko_yue_2024_07_17/model.int8.onnx
      sense_voice_zh_en_ja_ko_yue_2024_07_17/tokens.txt
      vad/silero_vad.onnx
```

主程序从上述固定目录启动 worker，并将 `--models` 指向整个 `media/sherpa-onnx/v1.13.6` 根目录：

```text
<asset-root>/worker/v0.1.0/markdown-media-worker.exe \
  --input <单个 .mp4/.m4a> \
  --models <asset-root>/media/sherpa-onnx/v1.13.6
```

工作进程的模型文件位于 `--models/models/`，FFmpeg 位于 `--models/ffmpeg/ffmpeg.exe`，原生 DLL 位于 `--models` 根目录。也支持 `--ffmpeg` 和 `ALL2MARKDOWN_FFMPEG` 显式指定 FFmpeg，以及 `SHERPA_ONNX_DLL` 显式指定 sherpa DLL；显式指定 sherpa DLL 时，其同目录必须有匹配的 `onnxruntime.dll`。

Xberg 是另一项可选能力，由 JchTools GUI 让用户选择并保存 Xberg 运行目录；媒体 worker 不查找、不下载、不处理 Xberg。

旧的兼容搜索仍支持以下布局，供独立调试使用：

```text
<models>/
  sherpa-onnx-c-api.dll
  onnxruntime.dll
  ffmpeg/ffmpeg.exe
  models/...
```

主程序初始化器为 worker、FFmpeg、sherpa-onnx/ONNX Runtime 原生库及模型分别记录固定版本、下载来源、大小和 SHA-256；工作进程不会联网或自动下载。

## 固定媒体行为

FFmpeg 只映射第一条音轨并输出 16 kHz、单声道、signed 16-bit little-endian PCM。Rust 将每个样本除以 `32768.0`，按 512 样本送入 sherpa-onnx v1.13.6 的 Silero VAD（阈值 0.25、最小静音 0.5 秒、最小语音 0.5 秒、最长片段 10 秒），再将片段交给 SenseVoice INT8（CPU、`zh`、`use_itn=true`）。模型加载、FFmpeg 进程和 C ABI 失败都会给出 stderr 错误；无音轨和未检测到语音仍输出明确 Markdown 状态。
