//! Optional MP4/M4A transcription worker.
//!
//! The worker intentionally has no Python dependency.  FFmpeg is launched as
//! an external, pinned asset and sherpa-onnx is loaded through its stable C
//! ABI at runtime.  Keeping both engines out of the main binary makes the
//! Markdown feature optional without changing the existing JchTools startup.

use clap::Parser;
#[cfg(windows)]
use libloading::os::windows::{Library, Symbol};
#[cfg(not(windows))]
use libloading::{Library, Symbol};
use serde::Serialize;
use std::ffi::{c_char, c_void, CStr, CString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

#[cfg(windows)]
const DLL_SEARCH_FLAGS: u32 = libloading::os::windows::LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR
    | libloading::os::windows::LOAD_LIBRARY_SEARCH_DEFAULT_DIRS;

const SAMPLE_RATE: i32 = 16_000;
const VAD_WINDOW: usize = 512;
const VAD_THRESHOLD: f32 = 0.25;
const VAD_MIN_SILENCE: f32 = 0.5;
const VAD_MIN_SPEECH: f32 = 0.5;
const VAD_MAX_SPEECH: f32 = 10.0;
const VAD_BUFFER_SECONDS: f32 = 60.0;

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
    /// 允许主程序显式指定固定版本的 ffmpeg.exe。
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

#[derive(Debug, Clone)]
struct Segment {
    start: i32,
    samples: Vec<f32>,
}

#[derive(Debug, Clone)]
struct TranscriptLine {
    start: f32,
    end: f32,
    text: String,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FeatureConfig {
    sample_rate: i32,
    feature_dim: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Transducer {
    encoder: *const c_char,
    decoder: *const c_char,
    joiner: *const c_char,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct OneModel {
    model: *const c_char,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Whisper {
    encoder: *const c_char,
    decoder: *const c_char,
    language: *const c_char,
    task: *const c_char,
    tail_paddings: i32,
    enable_token_timestamps: i32,
    enable_segment_timestamps: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SenseVoice {
    model: *const c_char,
    language: *const c_char,
    use_itn: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Moonshine {
    preprocessor: *const c_char,
    encoder: *const c_char,
    uncached_decoder: *const c_char,
    cached_decoder: *const c_char,
    merged_decoder: *const c_char,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FireRedAsr {
    encoder: *const c_char,
    decoder: *const c_char,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Canary {
    encoder: *const c_char,
    decoder: *const c_char,
    src_lang: *const c_char,
    tgt_lang: *const c_char,
    use_pnc: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FunAsrNano {
    encoder_adaptor: *const c_char,
    llm: *const c_char,
    embedding: *const c_char,
    tokenizer: *const c_char,
    system_prompt: *const c_char,
    user_prompt: *const c_char,
    max_new_tokens: i32,
    temperature: f32,
    top_p: f32,
    seed: i32,
    language: *const c_char,
    itn: i32,
    hotwords: *const c_char,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Qwen3Asr {
    conv_frontend: *const c_char,
    encoder: *const c_char,
    decoder: *const c_char,
    tokenizer: *const c_char,
    max_total_len: i32,
    max_new_tokens: i32,
    temperature: f32,
    top_p: f32,
    seed: i32,
    hotwords: *const c_char,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CohereTranscribe {
    encoder: *const c_char,
    decoder: *const c_char,
    language: *const c_char,
    use_punct: i32,
    use_itn: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct OfflineModelConfig {
    transducer: Transducer,
    paraformer: OneModel,
    nemo_ctc: OneModel,
    whisper: Whisper,
    tdnn: OneModel,
    tokens: *const c_char,
    num_threads: i32,
    debug: i32,
    provider: *const c_char,
    model_type: *const c_char,
    modeling_unit: *const c_char,
    bpe_vocab: *const c_char,
    telespeech_ctc: *const c_char,
    sense_voice: SenseVoice,
    moonshine: Moonshine,
    fire_red_asr: FireRedAsr,
    dolphin: OneModel,
    zipformer_ctc: OneModel,
    canary: Canary,
    wenet_ctc: OneModel,
    omnilingual: OneModel,
    medasr: OneModel,
    funasr_nano: FunAsrNano,
    fire_red_asr_ctc: OneModel,
    qwen3_asr: Qwen3Asr,
    cohere_transcribe: CohereTranscribe,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Homophone {
    dict_dir: *const c_char,
    lexicon: *const c_char,
    rule_fsts: *const c_char,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct OfflineRecognizerConfig {
    feat_config: FeatureConfig,
    model_config: OfflineModelConfig,
    lm_config: [u8; 16],
    decoding_method: *const c_char,
    max_active_paths: i32,
    hotwords_file: *const c_char,
    hotwords_score: f32,
    rule_fsts: *const c_char,
    rule_fars: *const c_char,
    blank_penalty: f32,
    hr: Homophone,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Silero {
    model: *const c_char,
    threshold: f32,
    min_silence_duration: f32,
    min_speech_duration: f32,
    window_size: i32,
    max_speech_duration: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct TenVad {
    model: *const c_char,
    threshold: f32,
    min_silence_duration: f32,
    min_speech_duration: f32,
    window_size: i32,
    max_speech_duration: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VadConfig {
    silero_vad: Silero,
    sample_rate: i32,
    num_threads: i32,
    provider: *const c_char,
    debug: i32,
    ten_vad: TenVad,
}

#[repr(C)]
struct SpeechSegment {
    start: i32,
    samples: *mut f32,
    n: i32,
}

#[repr(C)]
struct OfflineResult {
    text: *const c_char,
    timestamps: *mut f32,
    count: i32,
    tokens: *const c_char,
    tokens_arr: *const *const c_char,
    json: *const c_char,
    lang: *const c_char,
    emotion: *const c_char,
    event: *const c_char,
    durations: *mut f32,
    ys_log_probs: *mut f32,
    segment_timestamps: *const f32,
    segment_durations: *const f32,
    segment_texts: *const c_char,
    segment_texts_arr: *const *const c_char,
    segment_count: i32,
}

#[allow(non_camel_case_types)]
type OfflineRecognizer = c_void;
#[allow(non_camel_case_types)]
type OfflineStream = c_void;
#[allow(non_camel_case_types)]
type Vad = c_void;

type CreateRecognizer =
    unsafe extern "C" fn(*const OfflineRecognizerConfig) -> *const OfflineRecognizer;
type DestroyRecognizer = unsafe extern "C" fn(*const OfflineRecognizer);
type CreateStream = unsafe extern "C" fn(*const OfflineRecognizer) -> *const OfflineStream;
type DestroyStream = unsafe extern "C" fn(*const OfflineStream);
type AcceptOffline = unsafe extern "C" fn(*const OfflineStream, i32, *const f32, i32);
type DecodeOffline = unsafe extern "C" fn(*const OfflineRecognizer, *const OfflineStream);
// v1.13.6 C API: SherpaOnnxGetOfflineStreamResult(stream)
type GetResult = unsafe extern "C" fn(*const OfflineStream) -> *const OfflineResult;
type DestroyResult = unsafe extern "C" fn(*const OfflineResult);
type CreateVad = unsafe extern "C" fn(*const VadConfig, f32) -> *const Vad;
type DestroyVad = unsafe extern "C" fn(*const Vad);
type VadAccept = unsafe extern "C" fn(*const Vad, *const f32, i32);
type VadEmpty = unsafe extern "C" fn(*const Vad) -> i32;
type VadFront = unsafe extern "C" fn(*const Vad) -> *const SpeechSegment;
type VadDestroySegment = unsafe extern "C" fn(*const SpeechSegment);
type VadPop = unsafe extern "C" fn(*const Vad);
type VadFlush = unsafe extern "C" fn(*const Vad);

struct Sherpa {
    create_recognizer: DynamicSymbol<CreateRecognizer>,
    destroy_recognizer: DynamicSymbol<DestroyRecognizer>,
    create_stream: DynamicSymbol<CreateStream>,
    destroy_stream: DynamicSymbol<DestroyStream>,
    accept_offline: DynamicSymbol<AcceptOffline>,
    decode_offline: DynamicSymbol<DecodeOffline>,
    get_result: DynamicSymbol<GetResult>,
    destroy_result: DynamicSymbol<DestroyResult>,
    create_vad: DynamicSymbol<CreateVad>,
    destroy_vad: DynamicSymbol<DestroyVad>,
    vad_accept: DynamicSymbol<VadAccept>,
    vad_empty: DynamicSymbol<VadEmpty>,
    vad_front: DynamicSymbol<VadFront>,
    vad_destroy_segment: DynamicSymbol<VadDestroySegment>,
    vad_pop: DynamicSymbol<VadPop>,
    vad_flush: DynamicSymbol<VadFlush>,
}

#[cfg(windows)]
type DynamicLibrary = Library;
#[cfg(windows)]
type DynamicSymbol<T> = Symbol<T>;
#[cfg(not(windows))]
type DynamicLibrary = Library;
#[cfg(not(windows))]
type DynamicSymbol<T> = Symbol<'static, T>;

unsafe fn load_library(path: &Path) -> Result<DynamicLibrary, String> {
    #[cfg(windows)]
    {
        // Load the matching ONNX Runtime first.  The sherpa DLL imports it by
        // basename, so this prevents PATH (for example an old Python venv)
        // from supplying a different API version.
        let ort = path
            .parent()
            .map(|parent| parent.join("onnxruntime.dll"))
            .filter(|candidate| candidate.is_file())
            .ok_or_else(|| {
                format!(
                    "缺少与 sherpa-onnx 同目录的 onnxruntime.dll: {}",
                    path.display()
                )
            })?;
        let _runtime_library = DynamicLibrary::load_with_flags(&ort, DLL_SEARCH_FLAGS)
            .map_err(|e| format!("加载固定 onnxruntime.dll 失败: {e}"))?;
        Box::leak(Box::new(_runtime_library));
        DynamicLibrary::load_with_flags(path, DLL_SEARCH_FLAGS)
            .map_err(|e| format!("加载 sherpa-onnx DLL 失败: {e}"))
    }
    #[cfg(not(windows))]
    {
        DynamicLibrary::new(path).map_err(|e| format!("加载 sherpa-onnx DLL 失败: {e}"))
    }
}

// Keep the DLL handle alive until process exit so all symbols remain valid.
// The worker is intentionally short lived; leaking this single handle avoids
// moving or duplicating ownership of the library behind `'static` symbols.
impl Sherpa {
    unsafe fn load(path: &Path) -> Result<Self, String> {
        let library = load_library(path)?;
        let library: &'static DynamicLibrary = Box::leak(Box::new(library));
        macro_rules! sym {
            ($name:literal, $ty:ty) => {
                library
                    .get::<$ty>($name)
                    .map_err(|e| format!("sherpa-onnx 缺少符号 {}: {e}", stringify!($name)))?
            };
        }
        Ok(Self {
            create_recognizer: sym!(b"SherpaOnnxCreateOfflineRecognizer\0", CreateRecognizer),
            destroy_recognizer: sym!(b"SherpaOnnxDestroyOfflineRecognizer\0", DestroyRecognizer),
            create_stream: sym!(b"SherpaOnnxCreateOfflineStream\0", CreateStream),
            destroy_stream: sym!(b"SherpaOnnxDestroyOfflineStream\0", DestroyStream),
            accept_offline: sym!(b"SherpaOnnxAcceptWaveformOffline\0", AcceptOffline),
            decode_offline: sym!(b"SherpaOnnxDecodeOfflineStream\0", DecodeOffline),
            get_result: sym!(b"SherpaOnnxGetOfflineStreamResult\0", GetResult),
            destroy_result: sym!(b"SherpaOnnxDestroyOfflineRecognizerResult\0", DestroyResult),
            create_vad: sym!(b"SherpaOnnxCreateVoiceActivityDetector\0", CreateVad),
            destroy_vad: sym!(b"SherpaOnnxDestroyVoiceActivityDetector\0", DestroyVad),
            vad_accept: sym!(
                b"SherpaOnnxVoiceActivityDetectorAcceptWaveform\0",
                VadAccept
            ),
            vad_empty: sym!(b"SherpaOnnxVoiceActivityDetectorEmpty\0", VadEmpty),
            vad_front: sym!(b"SherpaOnnxVoiceActivityDetectorFront\0", VadFront),
            vad_destroy_segment: sym!(b"SherpaOnnxDestroySpeechSegment\0", VadDestroySegment),
            vad_pop: sym!(b"SherpaOnnxVoiceActivityDetectorPop\0", VadPop),
            vad_flush: sym!(b"SherpaOnnxVoiceActivityDetectorFlush\0", VadFlush),
        })
    }
}

fn cstring(value: &Path) -> Result<CString, String> {
    CString::new(value.to_string_lossy().as_bytes())
        .map_err(|_| format!("路径包含 NUL: {}", value.display()))
}

fn cstr(value: &str) -> Result<CString, String> {
    CString::new(value).map_err(|_| "配置字符串包含 NUL".to_string())
}

fn model_file(root: &Path, relative: &[&str]) -> PathBuf {
    let direct = relative
        .iter()
        .fold(root.to_path_buf(), |p, part| p.join(part));
    if direct.is_file() {
        return direct;
    }
    let nested = relative
        .iter()
        .fold(root.join("models"), |p, part| p.join(part));
    nested
}

fn find_sherpa(root: &Path) -> Option<PathBuf> {
    if let Ok(path) = std::env::var("SHERPA_ONNX_DLL") {
        return Some(PathBuf::from(path));
    }
    let candidates = [
        root.join("sherpa-onnx-c-api.dll"),
        root.join("sherpa-onnx.dll"),
        root.join("bin/sherpa-onnx-c-api.dll"),
        root.join("bin/sherpa-onnx.dll"),
        root.join("native/sherpa-onnx-c-api.dll"),
        root.join("native/sherpa-onnx.dll"),
        root.join("../bin/sherpa-onnx-c-api.dll"),
        root.join("../bin/sherpa-onnx.dll"),
        root.join("../native/sherpa-onnx-c-api.dll"),
        root.join("../native/sherpa-onnx.dll"),
        root.join("../sherpa-onnx-c-api.dll"),
        root.join("../sherpa-onnx.dll"),
    ];
    candidates.into_iter().find(|p| p.is_file())
}

fn find_ffmpeg(args: &Args) -> Result<PathBuf, String> {
    fn executable(path: PathBuf) -> Option<PathBuf> {
        if path.is_file() {
            return Some(path);
        }
        if path.is_dir() {
            let direct = path.join("ffmpeg.exe");
            if direct.is_file() {
                return Some(direct);
            }
            let bin = path.join("bin/ffmpeg.exe");
            if bin.is_file() {
                return Some(bin);
            }
        }
        None
    }

    if let Some(path) = &args.ffmpeg {
        return executable(path.clone())
            .ok_or_else(|| format!("指定的 ffmpeg 路径无效: {}", path.display()));
    }
    if let Ok(path) = std::env::var("ALL2MARKDOWN_FFMPEG") {
        return executable(PathBuf::from(path))
            .ok_or_else(|| "ALL2MARKDOWN_FFMPEG 不是有效的 ffmpeg.exe 路径".to_string());
    }
    let candidates = [
        args.models.join("ffmpeg/ffmpeg.exe"),
        args.models.join("ffmpeg/bin/ffmpeg.exe"),
        args.models.join("bin/ffmpeg.exe"),
        args.models.join("ffmpeg.exe"),
        args.models.join("../ffmpeg/ffmpeg.exe"),
        args.models.join("../ffmpeg/bin/ffmpeg.exe"),
        args.models.join("../bin/ffmpeg.exe"),
    ];
    candidates
        .into_iter()
        .find_map(executable)
        .ok_or_else(|| "缺少固定版本 ffmpeg.exe；请初始化媒体可选组件或使用 --ffmpeg".to_string())
}

fn drain_vad(sherpa: &Sherpa, vad: *const Vad, out: &mut Vec<Segment>) {
    unsafe {
        while (sherpa.vad_empty)(vad) == 0 {
            let segment = (sherpa.vad_front)(vad);
            if segment.is_null() {
                break;
            }
            let view = &*segment;
            let samples = if view.n <= 0 || view.samples.is_null() {
                Vec::new()
            } else {
                std::slice::from_raw_parts(view.samples, view.n as usize).to_vec()
            };
            out.push(Segment {
                start: view.start,
                samples,
            });
            (sherpa.vad_destroy_segment)(segment);
            (sherpa.vad_pop)(vad);
        }
    }
}

#[derive(Default)]
struct Pcm16Stream {
    carry: Option<u8>,
}

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

fn decode_audio(
    ffmpeg: &Path,
    input: &Path,
    sherpa: &Sherpa,
    vad: *const Vad,
) -> Result<(u64, Vec<Segment>), String> {
    let mut child = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(input)
        .args([
            "-map", "0:a:0", "-vn", "-f", "s16le", "-ac", "1", "-ar", "16000", "pipe:1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("启动 ffmpeg 失败: {e}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "ffmpeg stdout 不可用".to_string())?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "ffmpeg stderr 不可用".to_string())?;
    let stderr_thread = thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stderr.read_to_end(&mut bytes);
        (result, bytes)
    });
    let mut bytes = [0u8; VAD_WINDOW * 2 * 32];
    let mut pending: Vec<f32> = Vec::new();
    let mut pcm = Pcm16Stream::default();
    let mut total: u64 = 0;
    let mut segments = Vec::new();
    loop {
        let n = stdout
            .read(&mut bytes)
            .map_err(|e| format!("读取 ffmpeg 音频失败: {e}"))?;
        if n == 0 {
            break;
        }
        total += pcm.push(&bytes[..n], &mut pending) as u64;
        while pending.len() >= VAD_WINDOW {
            let chunk: Vec<f32> = pending.drain(..VAD_WINDOW).collect();
            unsafe {
                (sherpa.vad_accept)(vad, chunk.as_ptr(), chunk.len() as i32);
            }
            drain_vad(sherpa, vad, &mut segments);
        }
    }
    let pcm_result = pcm.finish();
    if !pending.is_empty() {
        unsafe {
            (sherpa.vad_accept)(vad, pending.as_ptr(), pending.len() as i32);
        }
        pending.clear();
    }
    unsafe {
        (sherpa.vad_flush)(vad);
    }
    let status = child.wait().map_err(|e| format!("等待 ffmpeg 失败: {e}"))?;
    let (stderr_result, stderr_bytes) = stderr_thread
        .join()
        .map_err(|_| "读取 ffmpeg stderr 的线程异常退出".to_string())?;
    stderr_result.map_err(|e| format!("读取 ffmpeg 错误输出失败: {e}"))?;
    pcm_result?;
    if !status.success() {
        let err = String::from_utf8_lossy(&stderr_bytes);
        if err.contains("matches no streams") || err.contains("Stream map '0:a:0'") {
            return Ok((0, Vec::new()));
        }
        return Err(format!("ffmpeg 解码失败: {}", err.trim()));
    }
    Ok((total, segments))
}

fn transcribe(
    sherpa: &Sherpa,
    recognizer: *const OfflineRecognizer,
    segments: &[Segment],
) -> Result<Vec<TranscriptLine>, String> {
    let mut lines = Vec::new();
    unsafe {
        for segment in segments {
            if segment.samples.is_empty() {
                continue;
            }
            let stream = (sherpa.create_stream)(recognizer);
            if stream.is_null() {
                return Err("创建 SenseVoice 流失败".to_string());
            }
            (sherpa.accept_offline)(
                stream,
                SAMPLE_RATE,
                segment.samples.as_ptr(),
                segment.samples.len() as i32,
            );
            (sherpa.decode_offline)(recognizer, stream);
            let result = (sherpa.get_result)(stream);
            if result.is_null() {
                (sherpa.destroy_stream)(stream);
                return Err("SenseVoice 没有返回结果".to_string());
            }
            let text = if (*result).text.is_null() {
                String::new()
            } else {
                CStr::from_ptr((*result).text)
                    .to_string_lossy()
                    .trim()
                    .to_string()
            };
            (sherpa.destroy_result)(result);
            (sherpa.destroy_stream)(stream);
            if !text.is_empty() {
                lines.push(TranscriptLine {
                    start: segment.start as f32 / SAMPLE_RATE as f32,
                    end: (segment.start as usize + segment.samples.len()) as f32
                        / SAMPLE_RATE as f32,
                    text: normalize_terms(&text),
                });
            }
        }
    }
    Ok(lines)
}

fn normalize_terms(text: &str) -> String {
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

fn markdown(name: &str, duration: f32, lines: &[TranscriptLine], has_audio: bool) -> String {
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
    let model = model_file(
        &args.models,
        &["sense_voice_zh_en_ja_ko_yue_2024_07_17", "model.int8.onnx"],
    );
    let tokens = model_file(
        &args.models,
        &["sense_voice_zh_en_ja_ko_yue_2024_07_17", "tokens.txt"],
    );
    let vad_model = model_file(&args.models, &["vad", "silero_vad.onnx"]);
    for path in [&model, &tokens, &vad_model] {
        if !path.is_file() {
            return Err(format!("媒体模型缺失: {}", path.display()));
        }
    }
    let sherpa_path = find_sherpa(&args.models)
        .ok_or_else(|| "缺少 sherpa-onnx.dll（可选媒体组件未初始化）".to_string())?;
    let ffmpeg = find_ffmpeg(&args)?;
    let ffmpeg = ffmpeg
        .canonicalize()
        .map_err(|e| format!("ffmpeg 路径无效: {e}"))?;
    let model_c = cstring(&model)?;
    let tokens_c = cstring(&tokens)?;
    let vad_c = cstring(&vad_model)?;
    let zh = cstr("zh")?;
    let cpu = cstr("cpu")?;
    let config = OfflineRecognizerConfig {
        feat_config: FeatureConfig {
            sample_rate: SAMPLE_RATE,
            feature_dim: 80,
        },
        model_config: OfflineModelConfig {
            tokens: tokens_c.as_ptr(),
            num_threads: args.threads.max(1),
            provider: cpu.as_ptr(),
            sense_voice: SenseVoice {
                model: model_c.as_ptr(),
                language: zh.as_ptr(),
                use_itn: 1,
            },
            ..Default::default()
        },
        decoding_method: std::ptr::null(),
        ..Default::default()
    };
    let silero = Silero {
        model: vad_c.as_ptr(),
        threshold: VAD_THRESHOLD,
        min_silence_duration: VAD_MIN_SILENCE,
        min_speech_duration: VAD_MIN_SPEECH,
        window_size: VAD_WINDOW as i32,
        max_speech_duration: VAD_MAX_SPEECH,
    };
    let vad_config = VadConfig {
        silero_vad: silero,
        sample_rate: SAMPLE_RATE,
        num_threads: 1,
        provider: cpu.as_ptr(),
        ..Default::default()
    };
    let sherpa = unsafe { Sherpa::load(&sherpa_path)? };
    let recognizer = unsafe { (sherpa.create_recognizer)(&config) };
    if recognizer.is_null() {
        return Err("创建 SenseVoice INT8 识别器失败".to_string());
    }
    let vad = unsafe { (sherpa.create_vad)(&vad_config, VAD_BUFFER_SECONDS) };
    if vad.is_null() {
        unsafe {
            (sherpa.destroy_recognizer)(recognizer);
        }
        return Err("创建 Silero VAD 失败".to_string());
    }
    let result = (|| {
        let (samples, segments) = decode_audio(&ffmpeg, &args.input, &sherpa, vad)?;
        if samples == 0 {
            return Ok(Success {
                markdown: markdown(
                    &args.input.file_name().unwrap_or_default().to_string_lossy(),
                    0.0,
                    &[],
                    false,
                ),
            });
        }
        let lines = transcribe(&sherpa, recognizer, &segments)?;
        Ok(Success {
            markdown: markdown(
                &args.input.file_name().unwrap_or_default().to_string_lossy(),
                samples as f32 / SAMPLE_RATE as f32,
                &lines,
                true,
            ),
        })
    })();
    unsafe {
        (sherpa.destroy_vad)(vad);
        (sherpa.destroy_recognizer)(recognizer);
    }
    result
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
