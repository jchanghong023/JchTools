//! Optional MP4/M4A transcription worker.
//!
//! The worker intentionally has no Python dependency.  FFmpeg and sherpa-onnx
//! are both loaded as pinned, optional assets through their native C ABIs at
//! runtime (see `ffmpeg.rs` / `sherpa.rs`); audio is decoded in-process, fed
//! through the streaming VAD pipeline (`vad.rs`) and transcribed segment by
//! segment.  Keeping the engines out of the main binary makes the Markdown
//! feature optional without changing the existing JchTools startup.

mod ffmpeg;
mod sherpa;
mod vad;

use clap::Parser;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "markdown-media-worker",
    version,
    about = "离线 MP4/M4A 转 Markdown"
)]
struct Args {
    /// 输入的单个 MP4/M4A 文件。
    #[arg(long)]
    input: PathBuf,
    /// 可选媒体模型根目录。
    #[arg(long)]
    models: PathBuf,
    /// 调试用：显式指定 FFmpeg shared DLL 目录（默认从 --models 解析）。
    #[arg(long)]
    ffmpeg: Option<PathBuf>,
    /// SenseVoice 推理线程数。
    #[arg(long, default_value_t = 1)]
    threads: i32,
}

#[derive(Debug, Serialize)]
struct Success {
    markdown: String,
}

/// 旧的 ffmpeg.exe 管道路径使用的 s16le 字节流 → f32 转换器；FFI 路径直接按
/// i16 切片取样本后不再经过字节流。保留实现与其既有回归测试（样本跨读取
/// 边界的对齐语义、截断字节必须报错）。
#[cfg(test)]
#[derive(Default)]
struct Pcm16Stream {
    carry: Option<u8>,
}

#[cfg(test)]
impl Pcm16Stream {
    /// Decode arbitrary stdout chunks without assuming read boundaries align to samples.
    fn push(&mut self, bytes: &[u8], out: &mut Vec<f32>) -> usize {
        let mut offset = 0;
        let mut produced = 0;
        if let Some(low) = self.carry.take() {
            if let Some(&high) = bytes.first() {
                out.push(i16::from_le_bytes([low, high]) as f32 / 32768.0);
                produced += 1;
                offset = 1;
            } else {
                self.carry = Some(low);
                return 0;
            }
        }
        while offset + 1 < bytes.len() {
            out.push(i16::from_le_bytes([bytes[offset], bytes[offset + 1]]) as f32 / 32768.0);
            produced += 1;
            offset += 2;
        }
        if offset < bytes.len() {
            self.carry = Some(bytes[offset]);
        }
        produced
    }

    /// Reject a final incomplete byte instead of silently truncating PCM.
    fn finish(&mut self) -> Result<(), String> {
        if self.carry.take().is_some() {
            return Err("ffmpeg 输出的 PCM 包含不完整的最后一个字节".to_string());
        }
        Ok(())
    }
}

pub(crate) fn normalize_terms(text: &str) -> String {
    let mut terms = [
        ("扫描压缩", "scan compression"),
        ("扫描使能", "scan enable"),
        ("扫描链", "scan chain"),
        ("静态时序分析", "STA"),
        ("标准延时格式", "SDF"),
        ("标准测试接口语言", "STIL"),
        ("固定型故障", "stuck-at"),
        ("固定故障", "stuck-at"),
        ("转换故障", "transition fault"),
        ("故障覆盖率", "coverage"),
        ("扫描测试", "scan"),
        ("威格尔", "WGL"),
        ("迪弗蒂", "DFT"),
        ("迪弗特", "DFT"),
        ("艾特皮吉", "ATPG"),
        ("A T P G", "ATPG"),
        ("M B I S T", "MBIST"),
        ("L B I S T", "LBIST"),
        ("B I S T", "BIST"),
        ("S T A", "STA"),
        ("S D F", "SDF"),
        ("S T I L", "STIL"),
        ("V e r i l o g", "Verilog"),
        ("系統维罗格", "Verilog"),
        ("dft", "DFT"),
        ("atpg", "ATPG"),
        ("mbist", "MBIST"),
        ("lbist", "LBIST"),
        ("bist", "BIST"),
        ("verilog", "Verilog"),
        ("systemverilog", "SystemVerilog"),
        ("vcs", "VCS"),
        ("verdi", "Verdi"),
        ("primetime", "PrimeTime"),
        ("tessent", "Tessent"),
        ("innovus", "Innovus"),
        ("genus", "Genus"),
    ];
    // Match the legacy converter: longer phrases win before their shorter
    // components (for example `systemverilog` before `verilog`).
    terms.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.0.len()));
    let mut normalized = text.to_string();
    for (from, to) in terms {
        normalized = normalized.replace(from, to);
    }
    normalized
}

fn timestamp(seconds: f32) -> String {
    let total_ms = (seconds.max(0.0) * 1000.0).round() as u64;
    let hours = total_ms / 3_600_000;
    let minutes = (total_ms % 3_600_000) / 60_000;
    let secs = (total_ms % 60_000) / 1000;
    let millis = total_ms % 1000;
    format!("{hours:02}:{minutes:02}:{secs:02}.{millis:03}")
}

fn markdown(name: &str, duration: f32, lines: &[vad::TranscriptLine], has_audio: bool) -> String {
    let mut out = vec![format!("# {name}"), String::new()];
    out.push(if has_audio {
        format!("- 音频时长: {}", timestamp(duration))
    } else {
        "- 音频时长: 无音频轨道".to_string()
    });
    out.push(format!("- 语音片段: {}", lines.len()));
    out.extend([String::new(), "## 转录".to_string(), String::new()]);
    if !has_audio {
        out.push("（无音频轨道）".to_string());
    } else if lines.is_empty() {
        out.push("（未检测到语音）".to_string());
    } else {
        for line in lines {
            out.push(format!(
                "[{} --> {}] {}",
                timestamp(line.start),
                timestamp(line.end),
                line.text
            ));
        }
    }
    out.push(String::new());
    out.join("\n")
}

fn run(args: Args) -> Result<Success, String> {
    if !args.input.is_file() {
        return Err(format!("输入文件不存在: {}", args.input.display()));
    }
    if !matches!(
        args.input
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("mp4") | Some("m4a")
    ) {
        return Err("只支持 .mp4 和 .m4a".to_string());
    }
    if !args.models.is_dir() {
        return Err(format!("模型目录不存在: {}", args.models.display()));
    }
    let model = sherpa::model_file(
        &args.models,
        &["sense_voice_zh_en_ja_ko_yue_2024_07_17", "model.int8.onnx"],
    );
    let tokens = sherpa::model_file(
        &args.models,
        &["sense_voice_zh_en_ja_ko_yue_2024_07_17", "tokens.txt"],
    );
    let vad_model = sherpa::model_file(&args.models, &["vad", "silero_vad.onnx"]);
    for path in [&model, &tokens, &vad_model] {
        if !path.is_file() {
            return Err(format!("媒体模型缺失: {}", path.display()));
        }
    }
    let sherpa_path = sherpa::find_sherpa(&args.models)
        .ok_or_else(|| "缺少 sherpa-onnx.dll（可选媒体组件未初始化）".to_string())?;
    let ffmpeg_dir = ffmpeg::find_dll_dir(args.ffmpeg.as_deref(), &args.models)?;

    let model_c = sherpa::cstring(&model)?;
    let tokens_c = sherpa::cstring(&tokens)?;
    let vad_c = sherpa::cstring(&vad_model)?;
    let zh = sherpa::cstr("zh")?;
    let cpu = sherpa::cstr("cpu")?;
    // SAFETY：FFI 指针自此到进程退出有效（库句柄有意保持存活）。
    let sherpa_engine = unsafe { sherpa::Sherpa::load(&sherpa_path)? };
    let recognizer =
        unsafe { sherpa_engine.create_recognizer(&model_c, &tokens_c, args.threads, &cpu, &zh)? };
    let vad_ptr = unsafe { sherpa_engine.create_vad(&vad_c, &cpu)? };
    let mut real_vad = unsafe { sherpa::RealVad::new(&sherpa_engine, vad_ptr) };
    let mut transcriber = unsafe { sherpa::SenseVoiceTranscriber::new(&sherpa_engine, recognizer) };
    let mut pipeline = vad::VadPipeline::new(&mut real_vad, &mut transcriber);
    // SAFETY：DLL 目录来自固定资产校验后的位置。
    let libs = unsafe { ffmpeg::FfmpegLibs::load(&ffmpeg_dir) }?;
    let outcome =
        ffmpeg::decode_audio(&libs, &args.input, |samples| pipeline.push_samples(samples))?;
    pipeline.finish()?;
    // 时长以实际喂入 VAD 的样本为准（与片段时间戳同源）。
    let samples = pipeline.total_samples();
    let lines = pipeline.into_lines();
    let name = args
        .input
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    Ok(Success {
        markdown: markdown(
            &name,
            samples as f32 / vad::SAMPLE_RATE as f32,
            &lines,
            outcome.has_audio,
        ),
    })
}

fn main() {
    let args = Args::parse();
    match run(args) {
        Ok(value) => match serde_json::to_string(&value) {
            Ok(json) => println!("{json}"),
            Err(e) => {
                eprintln!("输出 JSON 失败: {e}");
                std::process::exit(4);
            }
        },
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(3);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_matches_legacy_format() {
        assert_eq!(timestamp(3661.5), "01:01:01.500");
        assert_eq!(timestamp(-1.0), "00:00:00.000");
    }

    #[test]
    fn terminology_replacements_are_fixed() {
        assert_eq!(
            normalize_terms("扫描链和静态时序分析，A T P G 与未知词"),
            "scan chain和STA，ATPG 与未知词"
        );
        assert_eq!(normalize_terms("systemverilog"), "SystemVerilog");
    }

    #[test]
    fn markdown_has_empty_audio_states() {
        assert!(markdown("silent.mp4", 0.0, &[], true).contains("（未检测到语音）"));
        assert!(markdown("video.mp4", 0.0, &[], false).contains("无音频轨道"));
    }

    #[test]
    fn pcm_stream_preserves_sample_split_across_reads() {
        let mut decoder = Pcm16Stream::default();
        let mut samples = Vec::new();
        assert_eq!(decoder.push(&[0x00], &mut samples), 0);
        assert_eq!(decoder.push(&[0x40, 0x00, 0x80], &mut samples), 2);
        decoder.finish().expect("完整样本不应留下半个字节");
        assert_eq!(samples, vec![0.5, -1.0]);
    }

    #[test]
    fn pcm_stream_rejects_truncated_final_sample() {
        let mut decoder = Pcm16Stream::default();
        let mut samples = Vec::new();
        assert_eq!(decoder.push(&[0x7f], &mut samples), 0);
        assert!(decoder.finish().is_err());
    }
}
