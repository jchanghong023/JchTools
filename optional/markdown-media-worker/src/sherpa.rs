//! sherpa-onnx v1.13.6 C ABI 的动态加载封装（T-19 / 附录 D）。
//!
//! DLL 与 onnxruntime.dll 均来自主程序按 T-05/T-06 安装的固定资产；符号在
//! 进程生命周期内保持有效（库句柄有意泄漏到进程退出，见 [`Sherpa::load`]）。

#[cfg(windows)]
use libloading::os::windows::{Library, Symbol};
#[cfg(not(windows))]
use libloading::{Library, Symbol};
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::{Path, PathBuf};

use crate::vad::{Segment, Transcriber, TranscriptLine, VadEngine, SAMPLE_RATE};

#[cfg(windows)]
const DLL_SEARCH_FLAGS: u32 = libloading::os::windows::LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR
    | libloading::os::windows::LOAD_LIBRARY_SEARCH_DEFAULT_DIRS;

/// Silero VAD 阈值参数（与旧媒体链路一致，见 T-19「保持解码、语音切分与识别行为」）。
pub const VAD_THRESHOLD: f32 = 0.25;
pub const VAD_MIN_SILENCE: f32 = 0.5;
pub const VAD_MIN_SPEECH: f32 = 0.5;
pub const VAD_MAX_SPEECH: f32 = 10.0;
/// VAD 内部缓冲秒数：限制引擎自身累积的语音样本量。
pub const VAD_BUFFER_SECONDS: f32 = 60.0;

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

pub struct Sherpa {
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
    pub unsafe fn load(path: &Path) -> Result<Self, String> {
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

    /// 创建 SenseVoice INT8 离线识别器（CPU、固定线程数）。
    pub unsafe fn create_recognizer(
        &self,
        model: &CStr,
        tokens: &CStr,
        threads: i32,
        provider: &CStr,
        language: &CStr,
    ) -> Result<*const OfflineRecognizer, String> {
        let config = OfflineRecognizerConfig {
            feat_config: FeatureConfig {
                sample_rate: SAMPLE_RATE,
                feature_dim: 80,
            },
            model_config: OfflineModelConfig {
                tokens: tokens.as_ptr(),
                num_threads: threads.max(1),
                provider: provider.as_ptr(),
                sense_voice: SenseVoice {
                    model: model.as_ptr(),
                    language: language.as_ptr(),
                    use_itn: 1,
                },
                ..Default::default()
            },
            decoding_method: std::ptr::null(),
            ..Default::default()
        };
        let recognizer = (self.create_recognizer)(&config);
        if recognizer.is_null() {
            return Err("创建 SenseVoice INT8 识别器失败".to_string());
        }
        Ok(recognizer)
    }

    /// 创建 Silero VAD，参数与旧媒体链路保持一致。
    pub unsafe fn create_vad(&self, model: &CStr, provider: &CStr) -> Result<*const Vad, String> {
        let silero = Silero {
            model: model.as_ptr(),
            threshold: VAD_THRESHOLD,
            min_silence_duration: VAD_MIN_SILENCE,
            min_speech_duration: VAD_MIN_SPEECH,
            window_size: crate::vad::VAD_WINDOW as i32,
            max_speech_duration: VAD_MAX_SPEECH,
        };
        let config = VadConfig {
            silero_vad: silero,
            sample_rate: SAMPLE_RATE,
            num_threads: 1,
            provider: provider.as_ptr(),
            ..Default::default()
        };
        let vad = (self.create_vad)(&config, VAD_BUFFER_SECONDS);
        if vad.is_null() {
            return Err("创建 Silero VAD 失败".to_string());
        }
        Ok(vad)
    }
}

pub fn cstring(value: &Path) -> Result<CString, String> {
    CString::new(value.to_string_lossy().as_bytes())
        .map_err(|_| format!("路径包含 NUL: {}", value.display()))
}

pub fn cstr(value: &str) -> Result<CString, String> {
    CString::new(value).map_err(|_| "配置字符串包含 NUL".to_string())
}

pub fn model_file(root: &Path, relative: &[&str]) -> PathBuf {
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

pub fn find_sherpa(root: &Path) -> Option<PathBuf> {
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

/// 真实 VAD 适配器：把 sherpa FFI 指针接入 [`VadEngine`] 接缝。
pub struct RealVad<'a> {
    sherpa: &'a Sherpa,
    vad: *const Vad,
}

impl<'a> RealVad<'a> {
    pub unsafe fn new(sherpa: &'a Sherpa, vad: *const Vad) -> Self {
        Self { sherpa, vad }
    }
}

impl VadEngine for RealVad<'_> {
    fn accept(&mut self, window: &[f32]) {
        unsafe {
            (self.sherpa.vad_accept)(self.vad, window.as_ptr(), window.len() as i32);
        }
    }

    fn flush(&mut self) {
        unsafe {
            (self.sherpa.vad_flush)(self.vad);
        }
    }

    fn pop_segment(&mut self) -> Option<Segment> {
        unsafe {
            if (self.sherpa.vad_empty)(self.vad) != 0 {
                return None;
            }
            let segment = (self.sherpa.vad_front)(self.vad);
            if segment.is_null() {
                return None;
            }
            let view = &*segment;
            let samples = if view.n <= 0 || view.samples.is_null() {
                Vec::new()
            } else {
                std::slice::from_raw_parts(view.samples, view.n as usize).to_vec()
            };
            let owned = Segment {
                start: view.start,
                samples,
            };
            (self.sherpa.vad_destroy_segment)(segment);
            (self.sherpa.vad_pop)(self.vad);
            Some(owned)
        }
    }
}

impl Drop for RealVad<'_> {
    fn drop(&mut self) {
        unsafe {
            (self.sherpa.destroy_vad)(self.vad);
        }
    }
}

/// SenseVoice INT8 转写适配器：把识别器接入 [`Transcriber`] 接缝。
pub struct SenseVoiceTranscriber<'a> {
    sherpa: &'a Sherpa,
    recognizer: *const OfflineRecognizer,
}

impl<'a> SenseVoiceTranscriber<'a> {
    pub unsafe fn new(sherpa: &'a Sherpa, recognizer: *const OfflineRecognizer) -> Self {
        Self { sherpa, recognizer }
    }
}

impl Transcriber for SenseVoiceTranscriber<'_> {
    fn transcribe(&mut self, segment: &Segment) -> Result<Option<TranscriptLine>, String> {
        if segment.samples.is_empty() {
            return Ok(None);
        }
        unsafe {
            let stream = (self.sherpa.create_stream)(self.recognizer);
            if stream.is_null() {
                return Err("创建 SenseVoice 流失败".to_string());
            }
            (self.sherpa.accept_offline)(
                stream,
                SAMPLE_RATE,
                segment.samples.as_ptr(),
                segment.samples.len() as i32,
            );
            (self.sherpa.decode_offline)(self.recognizer, stream);
            let result = (self.sherpa.get_result)(stream);
            if result.is_null() {
                (self.sherpa.destroy_stream)(stream);
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
            (self.sherpa.destroy_result)(result);
            (self.sherpa.destroy_stream)(stream);
            if text.is_empty() {
                return Ok(None);
            }
            Ok(Some(TranscriptLine {
                start: segment.start as f32 / SAMPLE_RATE as f32,
                end: (segment.start as usize + segment.samples.len()) as f32 / SAMPLE_RATE as f32,
                text: crate::normalize_terms(&text),
            }))
        }
    }
}

impl Drop for SenseVoiceTranscriber<'_> {
    fn drop(&mut self) {
        unsafe {
            (self.sherpa.destroy_recognizer)(self.recognizer);
        }
    }
}
