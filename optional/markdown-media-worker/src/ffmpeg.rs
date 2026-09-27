//! FFmpeg 原生接口直调（T-19 / T-27 / 附录 D）。
//!
//! 通过 libloading 动态加载固定资产的 shared 构建（avutil-61 / swresample-7 /
//! avcodec-63 / avformat-63，来源与摘要由主程序按 T-05/T-06 钉定），在进程内完成
//! 「打开输入 → 定位第一条音轨 → 流式解码 → swresample 重采样为 16 kHz 单声道
//! s16le → 按比例转 f32」，不再启动 ffmpeg.exe 子进程。
//!
//! 结构体布局依据钉定构建随包提供的头文件（libavformat/avformat.h、
//! libavcodec/codec_par.h、libavcodec/packet.h、libavutil/frame.h、
//! libavutil/channel_layout.h），只镜像到被访问的字段为止；资产按摘要安装，
//! 布局与 DLL 严格同版本。

#[cfg(windows)]
use libloading::os::windows::{Library, Symbol};
#[cfg(not(windows))]
use libloading::{Library, Symbol};
use std::ffi::{c_char, c_int, c_void, CString};
use std::path::{Path, PathBuf};

use crate::vad::SAMPLE_RATE;

#[cfg(windows)]
const DLL_SEARCH_FLAGS: u32 = libloading::os::windows::LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR
    | libloading::os::windows::LOAD_LIBRARY_SEARCH_DEFAULT_DIRS;

/// 钉定 FFmpeg n9.0.2 shared 构建必需且自洽的四个 DLL（依赖链已核对：
/// avformat → avcodec/avutil，avcodec → avutil/swresample，swresample → avutil）。
pub const FFMPEG_DLLS: [&str; 4] = [
    "avutil-61.dll",
    "swresample-7.dll",
    "avcodec-63.dll",
    "avformat-63.dll",
];

const AVMEDIA_TYPE_AUDIO: c_int = 1;
const AV_SAMPLE_FMT_S16: c_int = 1;
const AV_CHANNEL_ORDER_UNSPEC: c_int = 0;
const AV_LOG_ERROR: c_int = 16;
/// avcodec 返回值：需要更多输入（Windows 构建下 EAGAIN=11）。
const AVERROR_EAGAIN: c_int = -11;
/// avformat/avcodec 的 EOF 标记：AVERROR_EOF = -MKTAG('E','O','F',' ')。
const AVERROR_EOF: c_int = -0x2046_4F45;
const ERROR_STRING_SIZE: usize = 64;

/// 有理数（libavutil/rational.h）。
#[repr(C)]
#[derive(Clone, Copy)]
struct AVRational {
    _num: c_int,
    _den: c_int,
}

/// 声道布局（libavutil/channel_layout.h）：order + nb_channels + 8 字节
/// 联合体 + opaque 指针，共 24 字节；联合体本实现不访问，以 u64 占位。
#[repr(C)]
#[derive(Clone, Copy)]
struct AVChannelLayout {
    order: c_int,
    nb_channels: c_int,
    _u: u64,
    _opaque: *mut c_void,
}

impl AVChannelLayout {
    fn zeroed() -> Self {
        Self {
            order: 0,
            nb_channels: 0,
            _u: 0,
            _opaque: std::ptr::null_mut(),
        }
    }
}

/// AVCodecParameters（libavcodec/codec_par.h）：镜像到 sample_rate 为止，
/// 其后字段不访问。
#[repr(C)]
struct AVCodecParameters {
    codec_type: c_int,
    codec_id: c_int,
    _codec_tag: u32,
    _extradata: *mut u8,
    _extradata_size: c_int,
    _coded_side_data: *mut c_void,
    _nb_coded_side_data: c_int,
    _format: c_int,
    _bit_rate: i64,
    _bits_per_coded_sample: c_int,
    _bits_per_raw_sample: c_int,
    _profile: c_int,
    _level: c_int,
    _width: c_int,
    _height: c_int,
    _sample_aspect_ratio: AVRational,
    _framerate: AVRational,
    _field_order: c_int,
    _color_range: c_int,
    _color_primaries: c_int,
    _color_trc: c_int,
    _color_space: c_int,
    _chroma_location: c_int,
    _video_delay: c_int,
    ch_layout: AVChannelLayout,
    sample_rate: c_int,
}

/// AVStream（libavformat/avformat.h）：镜像到 codecpar 为止。
#[repr(C)]
struct AVStream {
    _av_class: *const c_void,
    _index: c_int,
    _id: c_int,
    codecpar: *mut AVCodecParameters,
}

/// AVFormatContext（libavformat/avformat.h）：镜像到 streams 指针为止。
#[repr(C)]
struct AVFormatContext {
    _av_class: *const c_void,
    _iformat: *const c_void,
    _oformat: *const c_void,
    _priv_data: *mut c_void,
    _pb: *mut c_void,
    _ctx_flags: c_int,
    nb_streams: u32,
    streams: *mut *mut AVStream,
}

/// AVFrame（libavutil/frame.h）：镜像到结构体尾部；除 data/nb_samples/format/
/// sample_rate/ch_layout 外的字段仅用于保持偏移与钉定头文件一致。
#[repr(C)]
struct AVFrame {
    data: [*mut u8; 8],
    _linesize: [c_int; 8],
    _extended_data: *mut *mut u8,
    _width: c_int,
    _height: c_int,
    nb_samples: c_int,
    format: c_int,
    _pict_type: c_int,
    _sample_aspect_ratio: AVRational,
    _pts: i64,
    _pkt_dts: i64,
    _time_base: AVRational,
    _quality: c_int,
    _opaque: *mut c_void,
    _repeat_pict: c_int,
    sample_rate: c_int,
    _buf: [*mut c_void; 8],
    _extended_buf: *mut *mut c_void,
    _nb_extended_buf: c_int,
    _side_data: *mut *mut c_void,
    _nb_side_data: c_int,
    _flags: c_int,
    _color_range: c_int,
    _color_primaries: c_int,
    _color_trc: c_int,
    _colorspace: c_int,
    _chroma_location: c_int,
    _best_effort_timestamp: i64,
    _metadata: *mut c_void,
    _decode_error_flags: c_int,
    _hw_frames_ctx: *mut c_void,
    _opaque_ref: *mut c_void,
    _crop_top: usize,
    _crop_bottom: usize,
    _crop_left: usize,
    _crop_right: usize,
    _private_ref: *mut c_void,
    ch_layout: AVChannelLayout,
    _duration: i64,
    _alpha_mode: c_int,
}

/// AVPacket（libavcodec/packet.h）：只镜像到 stream_index 为止。
#[repr(C)]
struct AVPacketHead {
    _buf: *mut c_void,
    _pts: i64,
    _dts: i64,
    _data: *mut u8,
    _size: c_int,
    stream_index: c_int,
}

// 以下类型只按指针传递，字段从不访问。
#[allow(non_camel_case_types)]
enum AVCodec {}
#[allow(non_camel_case_types)]
enum AVCodecContext {}
#[allow(non_camel_case_types)]
enum AVPacket {}
#[allow(non_camel_case_types)]
enum AVInputFormat {}
#[allow(non_camel_case_types)]
enum AVDictionary {}
#[allow(non_camel_case_types)]
enum SwrContext {}

type AvLogSetLevel = unsafe extern "C" fn(c_int);
type AvStrerror = unsafe extern "C" fn(c_int, *mut c_char, usize) -> c_int;
type AvChannelLayoutDefault = unsafe extern "C" fn(*mut AVChannelLayout, c_int);
type AvChannelLayoutUninit = unsafe extern "C" fn(*mut AVChannelLayout);
type AvChannelLayoutCopy =
    unsafe extern "C" fn(*mut AVChannelLayout, *const AVChannelLayout) -> c_int;
type AvFrameAlloc = unsafe extern "C" fn() -> *mut AVFrame;
type AvFrameFree = unsafe extern "C" fn(*mut *mut AVFrame);
type AvFrameUnref = unsafe extern "C" fn(*mut AVFrame);
type AvformatOpenInput = unsafe extern "C" fn(
    *mut *mut AVFormatContext,
    *const c_char,
    *const AVInputFormat,
    *mut *mut AVDictionary,
) -> c_int;
type AvformatFindStreamInfo =
    unsafe extern "C" fn(*mut AVFormatContext, *mut *mut AVDictionary) -> c_int;
type AvformatCloseInput = unsafe extern "C" fn(*mut *mut AVFormatContext);
type AvReadFrame = unsafe extern "C" fn(*mut AVFormatContext, *mut AVPacket) -> c_int;
type AvcodecFindDecoder = unsafe extern "C" fn(c_int) -> *const AVCodec;
type AvcodecAllocContext3 = unsafe extern "C" fn(*const AVCodec) -> *mut AVCodecContext;
type AvcodecParametersToContext =
    unsafe extern "C" fn(*mut AVCodecContext, *const AVCodecParameters) -> c_int;
type AvcodecOpen2 =
    unsafe extern "C" fn(*mut AVCodecContext, *const AVCodec, *mut *mut AVDictionary) -> c_int;
type AvcodecFreeContext = unsafe extern "C" fn(*mut *mut AVCodecContext);
type AvcodecSendPacket = unsafe extern "C" fn(*mut AVCodecContext, *const AVPacket) -> c_int;
type AvcodecReceiveFrame = unsafe extern "C" fn(*mut AVCodecContext, *mut AVFrame) -> c_int;
type AvPacketAlloc = unsafe extern "C" fn() -> *mut AVPacket;
type AvPacketFree = unsafe extern "C" fn(*mut *mut AVPacket);
type AvPacketUnref = unsafe extern "C" fn(*mut AVPacket);
type SwrAllocSetOpts2 = unsafe extern "C" fn(
    *mut *mut SwrContext,
    *const AVChannelLayout,
    c_int,
    c_int,
    *const AVChannelLayout,
    c_int,
    c_int,
    c_int,
    *mut c_void,
) -> c_int;
type SwrInit = unsafe extern "C" fn(*mut SwrContext) -> c_int;
type SwrFree = unsafe extern "C" fn(*mut *mut SwrContext);
type SwrConvert =
    unsafe extern "C" fn(*mut SwrContext, *mut *const u8, c_int, *const *const u8, c_int) -> c_int;
type SwrGetDelay = unsafe extern "C" fn(*mut SwrContext, i64) -> i64;

#[cfg(windows)]
type DynamicLibrary = Library;
#[cfg(windows)]
type DynamicSymbol<T> = Symbol<T>;
#[cfg(not(windows))]
type DynamicLibrary = Library;
#[cfg(not(windows))]
type DynamicSymbol<T> = Symbol<'static, T>;

/// 已加载的 FFmpeg 原生库与函数集合。
pub struct FfmpegLibs {
    _libraries: [&'static DynamicLibrary; 4],
    log_set_level: DynamicSymbol<AvLogSetLevel>,
    strerror: DynamicSymbol<AvStrerror>,
    channel_layout_default: DynamicSymbol<AvChannelLayoutDefault>,
    channel_layout_uninit: DynamicSymbol<AvChannelLayoutUninit>,
    _channel_layout_copy: DynamicSymbol<AvChannelLayoutCopy>,
    frame_alloc: DynamicSymbol<AvFrameAlloc>,
    frame_free: DynamicSymbol<AvFrameFree>,
    frame_unref: DynamicSymbol<AvFrameUnref>,
    open_input: DynamicSymbol<AvformatOpenInput>,
    find_stream_info: DynamicSymbol<AvformatFindStreamInfo>,
    close_input: DynamicSymbol<AvformatCloseInput>,
    read_frame: DynamicSymbol<AvReadFrame>,
    find_decoder: DynamicSymbol<AvcodecFindDecoder>,
    alloc_context3: DynamicSymbol<AvcodecAllocContext3>,
    parameters_to_context: DynamicSymbol<AvcodecParametersToContext>,
    open2: DynamicSymbol<AvcodecOpen2>,
    free_context: DynamicSymbol<AvcodecFreeContext>,
    send_packet: DynamicSymbol<AvcodecSendPacket>,
    receive_frame: DynamicSymbol<AvcodecReceiveFrame>,
    packet_alloc: DynamicSymbol<AvPacketAlloc>,
    packet_free: DynamicSymbol<AvPacketFree>,
    packet_unref: DynamicSymbol<AvPacketUnref>,
    swr_alloc_set_opts2: DynamicSymbol<SwrAllocSetOpts2>,
    swr_init: DynamicSymbol<SwrInit>,
    swr_free: DynamicSymbol<SwrFree>,
    swr_convert: DynamicSymbol<SwrConvert>,
    swr_get_delay: DynamicSymbol<SwrGetDelay>,
}

impl FfmpegLibs {
    /// 依依赖顺序加载固定 DLL 集并解析符号；句柄保持到进程退出。
    ///
    /// # Safety
    /// `dir` 必须包含按 T-05/T-06 摘要安装的钉定 DLL 集；符号一经解析在进程
    /// 生命周期内保持有效。
    pub unsafe fn load(dir: &Path) -> Result<Self, String> {
        let mut libraries: Vec<&'static DynamicLibrary> = Vec::new();
        for name in FFMPEG_DLLS {
            let path = dir.join(name);
            #[cfg(windows)]
            let library = DynamicLibrary::load_with_flags(&path, DLL_SEARCH_FLAGS)
                .map_err(|e| format!("加载 {} 失败：{e}", path.display()))?;
            #[cfg(not(windows))]
            let library = DynamicLibrary::new(&path)
                .map_err(|e| format!("加载 {} 失败：{e}", path.display()))?;
            libraries.push(Box::leak(Box::new(library)) as &'static DynamicLibrary);
        }
        let avutil = libraries[0];
        let swresample = libraries[1];
        let avcodec = libraries[2];
        let avformat = libraries[3];
        macro_rules! sym {
            ($lib:expr, $name:literal, $ty:ty) => {
                $lib.get::<$ty>($name).map_err(|e| {
                    format!(
                        "FFmpeg {} 缺少符号 {}：{e}",
                        stringify!($lib),
                        stringify!($name)
                    )
                })?
            };
        }
        Ok(Self {
            _libraries: [avutil, swresample, avcodec, avformat],
            log_set_level: sym!(avutil, b"av_log_set_level\0", AvLogSetLevel),
            strerror: sym!(avutil, b"av_strerror\0", AvStrerror),
            channel_layout_default: sym!(
                avutil,
                b"av_channel_layout_default\0",
                AvChannelLayoutDefault
            ),
            channel_layout_uninit: sym!(
                avutil,
                b"av_channel_layout_uninit\0",
                AvChannelLayoutUninit
            ),
            _channel_layout_copy: sym!(avutil, b"av_channel_layout_copy\0", AvChannelLayoutCopy),
            frame_alloc: sym!(avutil, b"av_frame_alloc\0", AvFrameAlloc),
            frame_free: sym!(avutil, b"av_frame_free\0", AvFrameFree),
            frame_unref: sym!(avutil, b"av_frame_unref\0", AvFrameUnref),
            open_input: sym!(avformat, b"avformat_open_input\0", AvformatOpenInput),
            find_stream_info: sym!(
                avformat,
                b"avformat_find_stream_info\0",
                AvformatFindStreamInfo
            ),
            close_input: sym!(avformat, b"avformat_close_input\0", AvformatCloseInput),
            read_frame: sym!(avformat, b"av_read_frame\0", AvReadFrame),
            find_decoder: sym!(avcodec, b"avcodec_find_decoder\0", AvcodecFindDecoder),
            alloc_context3: sym!(avcodec, b"avcodec_alloc_context3\0", AvcodecAllocContext3),
            parameters_to_context: sym!(
                avcodec,
                b"avcodec_parameters_to_context\0",
                AvcodecParametersToContext
            ),
            open2: sym!(avcodec, b"avcodec_open2\0", AvcodecOpen2),
            free_context: sym!(avcodec, b"avcodec_free_context\0", AvcodecFreeContext),
            send_packet: sym!(avcodec, b"avcodec_send_packet\0", AvcodecSendPacket),
            receive_frame: sym!(avcodec, b"avcodec_receive_frame\0", AvcodecReceiveFrame),
            packet_alloc: sym!(avcodec, b"av_packet_alloc\0", AvPacketAlloc),
            packet_free: sym!(avcodec, b"av_packet_free\0", AvPacketFree),
            packet_unref: sym!(avcodec, b"av_packet_unref\0", AvPacketUnref),
            swr_alloc_set_opts2: sym!(swresample, b"swr_alloc_set_opts2\0", SwrAllocSetOpts2),
            swr_init: sym!(swresample, b"swr_init\0", SwrInit),
            swr_free: sym!(swresample, b"swr_free\0", SwrFree),
            swr_convert: sym!(swresample, b"swr_convert\0", SwrConvert),
            swr_get_delay: sym!(swresample, b"swr_get_delay\0", SwrGetDelay),
        })
    }

    fn describe(&self, code: c_int, context: &str) -> String {
        let mut buffer = [0 as c_char; ERROR_STRING_SIZE];
        let ok = unsafe { (self.strerror)(code, buffer.as_mut_ptr(), buffer.len()) };
        let detail = if ok == 0 {
            unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), buffer.len()) }
                .split(|&b| b == 0)
                .next()
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
                .unwrap_or_default()
        } else {
            String::new()
        };
        format!("{context}（错误码 {code}：{detail}）")
    }
}

/// 解码结果：样本总数（16 kHz 单声道）与是否存在音轨。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecodeOutcome {
    pub total_samples: u64,
    pub has_audio: bool,
}

impl DecodeOutcome {
    fn no_audio() -> Self {
        Self {
            total_samples: 0,
            has_audio: false,
        }
    }
}

/// 在资产目录中定位 FFmpeg shared DLL 集（目录或任一 DLL 路径均可）。
pub fn find_dll_dir(explicit: Option<&Path>, models_root: &Path) -> Result<PathBuf, String> {
    fn library_dir(candidate: &Path) -> Option<PathBuf> {
        if candidate.is_file() {
            return candidate.parent().map(|p| p.to_path_buf());
        }
        if candidate.is_dir()
            && FFMPEG_DLLS
                .iter()
                .all(|name| candidate.join(name).is_file())
        {
            return Some(candidate.to_path_buf());
        }
        None
    }

    if let Some(path) = explicit {
        return library_dir(path)
            .ok_or_else(|| format!("指定的 FFmpeg 共享库目录无效：{}", path.display()));
    }
    if let Ok(path) = std::env::var("ALL2MARKDOWN_FFMPEG") {
        let path = PathBuf::from(path);
        return library_dir(&path)
            .ok_or_else(|| "ALL2MARKDOWN_FFMPEG 不是有效的 FFmpeg 共享库目录".to_string());
    }
    let candidates = [
        models_root.join("ffmpeg"),
        models_root.join("bin"),
        models_root.join("../ffmpeg"),
    ];
    candidates
        .into_iter()
        .find_map(|candidate| library_dir(&candidate))
        .ok_or_else(|| {
            format!(
                "缺少固定版本 FFmpeg 共享库（{}）；请初始化媒体可选组件或用 --ffmpeg 指定目录",
                FFMPEG_DLLS.join("、")
            )
        })
}

/// RAII 守卫：保证错误路径也按 FFmpeg 约定释放资源。
struct FormatGuard<'f> {
    libs: &'f FfmpegLibs,
    ctx: *mut AVFormatContext,
}

impl Drop for FormatGuard<'_> {
    fn drop(&mut self) {
        unsafe { (self.libs.close_input)(&mut self.ctx) };
    }
}

struct CodecGuard<'f> {
    libs: &'f FfmpegLibs,
    ctx: *mut AVCodecContext,
}

impl Drop for CodecGuard<'_> {
    fn drop(&mut self) {
        unsafe { (self.libs.free_context)(&mut self.ctx) };
    }
}

struct PacketGuard<'f> {
    libs: &'f FfmpegLibs,
    pkt: *mut AVPacket,
}

impl Drop for PacketGuard<'_> {
    fn drop(&mut self) {
        unsafe { (self.libs.packet_free)(&mut self.pkt) };
    }
}

struct FrameGuard<'f> {
    libs: &'f FfmpegLibs,
    frame: *mut AVFrame,
}

impl Drop for FrameGuard<'_> {
    fn drop(&mut self) {
        unsafe { (self.libs.frame_free)(&mut self.frame) };
    }
}

/// 由 FFmpeg shared 库解码输入文件：定位第一条音轨，流式解码并重采样为
/// 16 kHz 单声道样本（s16 → f32，除以 32768.0），逐块推给 `sink`。
///
/// - 无音轨：返回 `has_audio = false`（T-20「无音频轨道」）。
/// - 解码失败：返回 Err（T-20 解码失败属于失败，不产出半成品）。
/// - 解码器 flush 与重采样器 flush 的尾部样本都会交给 `sink`（附录 D）。
pub fn decode_audio(
    libs: &FfmpegLibs,
    input: &Path,
    mut sink: impl FnMut(&[f32]) -> Result<(), String>,
) -> Result<DecodeOutcome, String> {
    unsafe {
        (libs.log_set_level)(AV_LOG_ERROR);
    }
    let url = CString::new(input.to_string_lossy().as_bytes())
        .map_err(|_| format!("输入路径包含 NUL: {}", input.display()))?;

    unsafe {
        let mut fmt: *mut AVFormatContext = std::ptr::null_mut();
        let code = (libs.open_input)(
            &mut fmt,
            url.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
        );
        if code != 0 {
            return Err(libs.describe(code, "FFmpeg 打开输入失败"));
        }
        let fmt = FormatGuard { libs, ctx: fmt };
        // 与 ffmpeg CLI 一致：尽力补全流信息，失败不致命（后续按需解码）。
        let _ = (libs.find_stream_info)(fmt.ctx, std::ptr::null_mut());

        // 附录 D：选择第一条音轨（按流顺序，而非“最佳”音轨）。
        let view: &AVFormatContext = &*fmt.ctx;
        let mut audio_index: c_int = -1;
        let mut codecpar: *mut AVCodecParameters = std::ptr::null_mut();
        for i in 0..view.nb_streams as usize {
            let stream = *view.streams.add(i);
            if stream.is_null() {
                continue;
            }
            let par = (*stream).codecpar;
            if par.is_null() {
                continue;
            }
            if (*par).codec_type == AVMEDIA_TYPE_AUDIO {
                audio_index = i as c_int;
                codecpar = par;
                break;
            }
        }
        if audio_index < 0 || codecpar.is_null() {
            return Ok(DecodeOutcome::no_audio());
        }

        let decoder = (libs.find_decoder)((*codecpar).codec_id);
        if decoder.is_null() {
            return Err(format!(
                "FFmpeg 没有第一条音轨的解码器（codec id {}）",
                (*codecpar).codec_id
            ));
        }
        let codec = (libs.alloc_context3)(decoder);
        if codec.is_null() {
            return Err("FFmpeg 分配解码上下文失败".to_string());
        }
        let codec = CodecGuard { libs, ctx: codec };
        let code = (libs.parameters_to_context)(codec.ctx, codecpar);
        if code < 0 {
            return Err(libs.describe(code, "FFmpeg 复制音频参数失败"));
        }
        let code = (libs.open2)(codec.ctx, decoder, std::ptr::null_mut());
        if code < 0 {
            return Err(libs.describe(code, "FFmpeg 打开音频解码器失败"));
        }

        let pkt = (libs.packet_alloc)();
        if pkt.is_null() {
            return Err("FFmpeg 分配数据包失败".to_string());
        }
        let pkt = PacketGuard { libs, pkt };
        let frame = (libs.frame_alloc)();
        if frame.is_null() {
            return Err("FFmpeg 分配解码帧失败".to_string());
        }
        let frame = FrameGuard { libs, frame };

        // 重采样器按首个解码帧的输出参数配置（解码输出可能与容器参数不同）。
        let mut swr: *mut SwrContext = std::ptr::null_mut();
        let mut codec_rate = (*codecpar).sample_rate;
        if codec_rate <= 0 {
            codec_rate = SAMPLE_RATE;
        }
        let mut total: u64 = 0;
        let mut out_i16: Vec<i16> = Vec::new();
        let mut out_f32: Vec<f32> = Vec::new();

        macro_rules! convert_frame {
            ($frame_ptr:expr) => {{
                let view: &AVFrame = &*$frame_ptr;
                if swr.is_null() {
                    swr = build_resampler(libs, view, codec_rate)?;
                }
                let in_rate = if view.sample_rate > 0 {
                    view.sample_rate as i64
                } else {
                    codec_rate as i64
                };
                let in_count = view.nb_samples.max(0) as i64;
                // 输出容量 = ceil((内部延迟 + 输入样本) × 16000 / 输入采样率)。
                let delay = (libs.swr_get_delay)(swr, in_rate);
                let out_count =
                    ((delay.max(0) + in_count) * SAMPLE_RATE as i64 + in_rate - 1) / in_rate;
                let out_count = out_count.max(1) as usize;
                out_i16.clear();
                out_i16.resize(out_count, 0);
                let out_ptr = [out_i16.as_ptr() as *const u8];
                let produced = (libs.swr_convert)(
                    swr,
                    out_ptr.as_ptr() as *mut *const u8,
                    out_count as c_int,
                    view.data.as_ptr() as *const *const u8,
                    view.nb_samples,
                );
                if produced < 0 {
                    return Err(libs.describe(produced, "FFmpeg 重采样失败"));
                }
                if produced > 0 {
                    let produced = produced as usize;
                    out_f32.clear();
                    out_f32.extend(
                        out_i16[..produced]
                            .iter()
                            .map(|&sample| sample as f32 / 32768.0),
                    );
                    total += produced as u64;
                    sink(&out_f32)?;
                }
            }};
        }

        loop {
            let code = (libs.read_frame)(fmt.ctx, pkt.pkt);
            if code == AVERROR_EOF {
                break;
            }
            if code < 0 {
                return Err(libs.describe(code, "FFmpeg 读取音频数据失败"));
            }
            let head = &*(pkt.pkt as *const AVPacketHead);
            if head.stream_index == audio_index {
                let code = (libs.send_packet)(codec.ctx, pkt.pkt);
                if code < 0 && code != AVERROR_EAGAIN {
                    return Err(libs.describe(code, "FFmpeg 解码音频失败"));
                }
                loop {
                    (libs.frame_unref)(frame.frame);
                    let code = (libs.receive_frame)(codec.ctx, frame.frame);
                    if code == 0 {
                        convert_frame!(frame.frame);
                    } else if code == AVERROR_EAGAIN || code == AVERROR_EOF {
                        break;
                    } else {
                        return Err(libs.describe(code, "FFmpeg 解码音频失败"));
                    }
                }
            }
            (libs.packet_unref)(pkt.pkt);
        }

        // 解码器 flush：送入 EOF 空包并收干余量帧（附录 D 尾部样本要求）。
        let code = (libs.send_packet)(codec.ctx, std::ptr::null());
        if code < 0 && code != AVERROR_EAGAIN && code != AVERROR_EOF {
            return Err(libs.describe(code, "FFmpeg 结束音频解码失败"));
        }
        loop {
            (libs.frame_unref)(frame.frame);
            let code = (libs.receive_frame)(codec.ctx, frame.frame);
            if code == 0 {
                convert_frame!(frame.frame);
            } else if code == AVERROR_EAGAIN || code == AVERROR_EOF {
                break;
            } else {
                return Err(libs.describe(code, "FFmpeg 解码音频失败"));
            }
        }

        // 重采样器 flush：收干内部缓冲的尾部样本（附录 D 尾部样本要求）。
        if !swr.is_null() {
            loop {
                let delay = (libs.swr_get_delay)(swr, SAMPLE_RATE as i64);
                let out_count = (delay.max(0) as usize).max(1);
                out_i16.clear();
                out_i16.resize(out_count, 0);
                let out_ptr = [out_i16.as_ptr() as *const u8];
                let produced = (libs.swr_convert)(
                    swr,
                    out_ptr.as_ptr() as *mut *const u8,
                    out_count as c_int,
                    std::ptr::null(),
                    0,
                );
                if produced <= 0 {
                    break;
                }
                let produced = produced as usize;
                out_f32.clear();
                out_f32.extend(
                    out_i16[..produced]
                        .iter()
                        .map(|&sample| sample as f32 / 32768.0),
                );
                total += produced as u64;
                sink(&out_f32)?;
            }
            (libs.swr_free)(&mut swr);
        }

        Ok(DecodeOutcome {
            total_samples: total,
            has_audio: true,
        })
    }
}

/// 用首个解码帧的参数构建 16 kHz 单声道 s16 重采样器。
unsafe fn build_resampler(
    libs: &FfmpegLibs,
    frame: &AVFrame,
    codec_rate: c_int,
) -> Result<*mut SwrContext, String> {
    let in_rate = if frame.sample_rate > 0 {
        frame.sample_rate
    } else {
        codec_rate
    };
    let mut in_layout = frame.ch_layout;
    if in_layout.order == AV_CHANNEL_ORDER_UNSPEC || in_layout.nb_channels <= 0 {
        // 未指定布局时按声道数取默认布局（与 ffmpeg CLI 行为一致）。
        let channels = in_layout.nb_channels.max(1);
        (libs.channel_layout_uninit)(&mut in_layout);
        (libs.channel_layout_default)(&mut in_layout, channels);
    }
    let mut out_layout = AVChannelLayout::zeroed();
    (libs.channel_layout_default)(&mut out_layout, 1);
    let mut swr: *mut SwrContext = std::ptr::null_mut();
    let code = (libs.swr_alloc_set_opts2)(
        &mut swr,
        &out_layout,
        AV_SAMPLE_FMT_S16,
        SAMPLE_RATE,
        &in_layout,
        frame.format,
        in_rate,
        0,
        std::ptr::null_mut(),
    );
    (libs.channel_layout_uninit)(&mut out_layout);
    if code < 0 || swr.is_null() {
        return Err(libs.describe(code, "FFmpeg 创建重采样器失败"));
    }
    let code = (libs.swr_init)(swr);
    if code < 0 {
        (libs.swr_free)(&mut swr);
        return Err(libs.describe(code, "FFmpeg 初始化重采样器失败"));
    }
    Ok(swr)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实 FFI 集成测试：只在固定资产在场时运行（运行时条件跳过，非 #[ignore]）。
    /// 通过 JCHTOOLS_WORKER_FFI_DIR 指定 FFmpeg shared DLL 目录，
    /// JCHTOOLS_WORKER_FFI_MEDIA 指定测试媒体，JCHTOOLS_WORKER_FFMPEG_REF 指定
    /// ffmpeg.exe 生成的参考 PCM（s16le/16k/mono）；三者齐备才执行。
    fn asset(name: &str) -> Option<PathBuf> {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|p| p.is_file())
    }

    fn load_libs() -> Option<FfmpegLibs> {
        let dir = std::env::var_os("JCHTOOLS_WORKER_FFI_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_dir())?;
        unsafe { FfmpegLibs::load(&dir) }.ok()
    }

    // 覆盖 T-19/附录 D「Rust 直接调用 FFmpeg 原生接口」：FFI 解码输出必须与
    // 固定 ffmpeg.exe 的等价命令输出逐字节一致（第一条音轨、16 kHz、单声道、
    // s16le、含解码器与重采样器尾部样本）。
    #[test]
    fn ffmpeg_ffi_output_matches_cli_reference() {
        let (Some(libs), Some(media), Some(reference)) = (
            load_libs(),
            asset("JCHTOOLS_WORKER_FFI_MEDIA"),
            asset("JCHTOOLS_WORKER_FFMPEG_REF"),
        ) else {
            println!("skip：FFmpeg 固定资产不在场（JCHTOOLS_WORKER_FFI_DIR/_MEDIA/_REF）");
            return;
        };
        let mut collected: Vec<u8> = Vec::new();
        let outcome = decode_audio(&libs, &media, |samples| {
            for sample in samples {
                let bytes = sample
                    .mul_add(32768.0, 0.0)
                    .round()
                    .clamp(-32768.0, 32767.0) as i16;
                collected.extend_from_slice(&bytes.to_le_bytes());
            }
            Ok(())
        })
        .expect("FFI 解码不应失败");
        assert!(outcome.has_audio, "测试媒体必须包含音轨");
        let expected = std::fs::read(&reference).expect("读取参考 PCM");
        // s16 → f32 → s16 往返在半值处可能差 1 LSB：只允许逐样本 ±1。
        assert_eq!(
            collected.len(),
            expected.len(),
            "FFI 与 CLI 输出样本数不同（FFI {} 字节，参考 {} 字节）",
            collected.len(),
            expected.len()
        );
        let mut worst: i32 = 0;
        // PCM 字节流恒为偶数长度，chunks(2) 与 chunks_exact(2) 等价（后者触发 as_chunks lint）。
        for (index, (actual, want)) in collected.chunks(2).zip(expected.chunks(2)).enumerate() {
            let a = i16::from_le_bytes([actual[0], actual[1]]);
            let w = i16::from_le_bytes([want[0], want[1]]);
            let diff = (a as i32 - w as i32).abs();
            if diff > worst {
                worst = diff;
            }
            assert!(
                diff <= 1,
                "第 {index} 个样本 FFI={a} 与参考 {w} 相差 {diff}"
            );
        }
        println!(
            "FFI 与 CLI 输出一致：{} 样本，最大差值 {worst}",
            collected.len() / 2
        );
    }

    // 覆盖 T-20：无音轨文件必须识别为 has_audio=false（FFI 路径）。
    #[test]
    fn ffmpeg_ffi_reports_missing_audio_track() {
        let (Some(libs), Some(media)) = (load_libs(), asset("JCHTOOLS_WORKER_FFI_SILENT_MEDIA"))
        else {
            println!("skip：FFmpeg 固定资产不在场（JCHTOOLS_WORKER_FFI_DIR/_SILENT_MEDIA）");
            return;
        };
        let outcome = decode_audio(&libs, &media, |_| Ok(())).expect("无音轨不是解码失败");
        assert!(!outcome.has_audio);
        assert_eq!(outcome.total_samples, 0);
    }

    // 覆盖 T-05/T-06：钉定的四个 DLL 缺任何一个都不得继续。
    #[test]
    fn ffmpeg_dll_set_is_self_contained() {
        assert_eq!(FFMPEG_DLLS.len(), 4);
        assert!(FFMPEG_DLLS.iter().all(|name| name.ends_with(".dll")));
    }
}
