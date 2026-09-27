//! VAD 流水线：把解码得到的 16 kHz 单声道 f32 样本按 512 样本窗口喂入 VAD，
//! 收割语音片段并即时转写（T-19 / 附录 D）。
//!
//! [`VadEngine`] 与 [`Transcriber`] 把 sherpa-onnx FFI 从纯 Rust 流水线逻辑中隔离
//! 出来：生产路径用真实 FFI 实现，单元测试注入可编排的假实现，覆盖 EOF 尾部
//! 样本、短音频、窗口边界与流式转写等场景。
//!
//! 流式职责（附录 D）：片段一出队就交给转写器并释放 PCM，流水线全程只保留
//! 最终文本与时间戳，常驻样本量上限为一个片段加一个窗口缓冲，与整条音频
//! 长度无关。

/// 目标采样率（Hz）。解码与重采样固定输出该采样率的单声道 PCM。
pub const SAMPLE_RATE: i32 = 16_000;
/// Silero VAD 的窗口大小（样本数）。
pub const VAD_WINDOW: usize = 512;

/// VAD 收割出的语音片段：`start` 为该段首样本在整条 16 kHz 音频流中的位置。
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub start: i32,
    pub samples: Vec<f32>,
}

/// 一行最终转录结果：起止时间（秒）与文本。
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptLine {
    pub start: f32,
    pub end: f32,
    pub text: String,
}

/// 语音活动检测引擎接缝（生产实现为 sherpa-onnx Silero VAD）。
pub trait VadEngine {
    /// 喂入一段样本（生产实现要求按窗口大小喂入；EOF 前的尾段可小于窗口）。
    fn accept(&mut self, window: &[f32]);
    /// 输入结束时调用：让引擎把未定稿的语音定稿为片段。
    fn flush(&mut self);
    /// 依序取出已定稿片段；队列为空时返回 `None`。
    fn pop_segment(&mut self) -> Option<Segment>;
}

/// 片段转写器接缝（生产实现为 SenseVoice INT8）。
pub trait Transcriber {
    /// 转写单个片段；返回 `None` 表示该段没有可输出文本。
    fn transcribe(&mut self, segment: &Segment) -> Result<Option<TranscriptLine>, String>;
}

/// VAD 流水线：吸收任意分块的样本，负责窗口切分、EOF 收尾、片段收割与流式转写。
pub struct VadPipeline<'a> {
    vad: &'a mut dyn VadEngine,
    transcriber: &'a mut dyn Transcriber,
    pending: Vec<f32>,
    lines: Vec<TranscriptLine>,
    total: u64,
    peak_resident: usize,
}

impl<'a> VadPipeline<'a> {
    pub fn new(vad: &'a mut dyn VadEngine, transcriber: &'a mut dyn Transcriber) -> Self {
        Self {
            vad,
            transcriber,
            pending: Vec::new(),
            lines: Vec::new(),
            total: 0,
            peak_resident: 0,
        }
    }

    /// 喂入任意大小的样本块；内部按窗口切分后交给 VAD，出段即转写。
    pub fn push_samples(&mut self, samples: &[f32]) -> Result<(), String> {
        self.pending.extend_from_slice(samples);
        while self.pending.len() >= VAD_WINDOW {
            let window: Vec<f32> = self.pending.drain(..VAD_WINDOW).collect();
            self.vad.accept(&window);
            self.total += window.len() as u64;
            self.drain_ready()?;
        }
        self.peak_resident = self.peak_resident.max(self.pending.len());
        Ok(())
    }

    /// 输入结束：不足一窗口的尾部样本同样喂入，flush 之后必须继续收割，
    /// 否则 flush 定稿的最后一段会静默丢失（附录 D 尾部样本要求）。
    pub fn finish(&mut self) -> Result<(), String> {
        if !self.pending.is_empty() {
            let tail = std::mem::take(&mut self.pending);
            self.vad.accept(&tail);
            self.total += tail.len() as u64;
        }
        self.vad.flush();
        self.drain_ready()
    }

    /// 收割 VAD 已定稿的全部片段：逐段即时转写，PCM 在本函数结束即释放。
    fn drain_ready(&mut self) -> Result<(), String> {
        while let Some(segment) = self.vad.pop_segment() {
            self.peak_resident = self.peak_resident.max(segment.samples.len());
            if let Some(line) = self.transcriber.transcribe(&segment)? {
                self.lines.push(line);
            }
        }
        Ok(())
    }

    /// 已喂入 VAD 的样本总数（含 EOF 尾段）。
    pub fn total_samples(&self) -> u64 {
        self.total
    }

    /// 流水线任一时刻持有的未转写样本峰值（窗口缓冲或单个正在转写的片段）。
    /// 回归测试用它断言流式常驻上限（F16）；生产路径不读取。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn peak_resident_samples(&self) -> usize {
        self.peak_resident
    }

    /// 消费流水线，返回全部转录行。
    pub fn into_lines(self) -> Vec<TranscriptLine> {
        self.lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    /// 可编排的假 VAD：样本值按绝对索引编码为 `idx as f32`，测试据此核对内容完整。
    /// `mid_spans` 在累计接受样本到达右端点时立即出队；`flush_spans` 在 flush 时出队。
    struct FakeVad {
        accepted: usize,
        queue: VecDeque<Segment>,
        mid_spans: VecDeque<(usize, usize)>,
        flush_spans: VecDeque<(usize, usize)>,
    }

    impl FakeVad {
        fn new(mid: &[(usize, usize)], flush: &[(usize, usize)]) -> Self {
            Self {
                accepted: 0,
                queue: VecDeque::new(),
                mid_spans: mid.iter().copied().collect(),
                flush_spans: flush.iter().copied().collect(),
            }
        }

        fn emit(&mut self, span: (usize, usize)) {
            let (a, b) = span;
            let samples = (a..b).map(|i| i as f32).collect();
            self.queue.push_back(Segment {
                start: a as i32,
                samples,
            });
        }
    }

    impl VadEngine for FakeVad {
        fn accept(&mut self, window: &[f32]) {
            self.accepted += window.len();
            while let Some(&(_, b)) = self.mid_spans.front() {
                if b > self.accepted {
                    break;
                }
                let span = self.mid_spans.pop_front().expect("前一行已确认非空");
                self.emit(span);
            }
        }

        fn flush(&mut self) {
            while let Some(span) = self.flush_spans.pop_front() {
                self.emit(span);
            }
        }

        fn pop_segment(&mut self) -> Option<Segment> {
            self.queue.pop_front()
        }
    }

    /// 假转写器：把每次转写的行记录进共享列表，测试据此核对次数、时序与内容。
    struct FakeTranscriber {
        calls: Rc<RefCell<Vec<TranscriptLine>>>,
    }

    impl FakeTranscriber {
        fn new(calls: &Rc<RefCell<Vec<TranscriptLine>>>) -> Self {
            Self {
                calls: Rc::clone(calls),
            }
        }
    }

    impl Transcriber for FakeTranscriber {
        fn transcribe(&mut self, segment: &Segment) -> Result<Option<TranscriptLine>, String> {
            let line = TranscriptLine {
                start: segment.start as f32 / SAMPLE_RATE as f32,
                end: (segment.start as usize + segment.samples.len()) as f32 / SAMPLE_RATE as f32,
                text: format!("seg@{}", segment.start),
            };
            self.calls.borrow_mut().push(line.clone());
            Ok(Some(line))
        }
    }

    /// 跑完「喂样本 → finish」完整流程后的汇总：喂入总数与常驻峰值。
    struct RunSummary {
        total: u64,
        peak: usize,
        lines: usize,
    }

    /// 跑完「喂样本 → finish」的完整流程并返回汇总。
    fn run_stream(
        vad: &mut FakeVad,
        calls: &Rc<RefCell<Vec<TranscriptLine>>>,
        total_samples: usize,
    ) -> RunSummary {
        let mut transcriber = FakeTranscriber::new(calls);
        let mut pipeline = VadPipeline::new(vad, &mut transcriber);
        // 用不规则分块驱动，验证窗口切分与尾段处理不依赖输入边界。
        let mut fed = 0usize;
        let mut chunk = 700usize;
        while fed < total_samples {
            let take = chunk.min(total_samples - fed);
            let block: Vec<f32> = (fed..fed + take).map(|i| i as f32).collect();
            pipeline.push_samples(&block).expect("假 VAD 不会失败");
            fed += take;
            chunk = if chunk == 700 { 93 } else { 700 };
        }
        pipeline.finish().expect("假 VAD 不会失败");
        RunSummary {
            total: pipeline.total_samples(),
            peak: pipeline.peak_resident_samples(),
            lines: pipeline.into_lines().len(),
        }
    }

    fn recorder() -> Rc<RefCell<Vec<TranscriptLine>>> {
        Rc::new(RefCell::new(Vec::new()))
    }

    // 覆盖 T-19/T-20/附录 D「完整处理……VAD 的尾部样本」：
    // 一句语音刚结束就 EOF，片段只在 flush 时定稿，必须被收割转写。
    #[test]
    fn vad_flush_tail_segment_is_transcribed_after_eof() {
        let calls = recorder();
        let mut vad = FakeVad::new(&[], &[(100, 400)]);
        let summary = run_stream(&mut vad, &calls, 400);
        assert_eq!(summary.total, 400);
        let calls = calls.borrow();
        assert_eq!(calls.len(), 1, "flush 定稿的尾段不得静默丢失：{calls:?}");
        assert_eq!(calls[0].start, 100.0 / SAMPLE_RATE as f32);
        assert_eq!(calls[0].end, 400.0 / SAMPLE_RATE as f32);
    }

    // 覆盖 T-19/T-20/附录 D：无尾静音——语音在流末尾戛然而止，最后片段不丢失。
    #[test]
    fn vad_stream_end_without_trailing_silence_keeps_final_segment() {
        let calls = recorder();
        let mut vad = FakeVad::new(&[], &[(512, 2048)]);
        let summary = run_stream(&mut vad, &calls, 2048);
        assert_eq!(summary.total, 2048);
        let calls = calls.borrow();
        assert_eq!(calls.len(), 1, "无尾静音时最后一句不得丢失：{calls:?}");
        assert_eq!(calls[0].start, 512.0 / SAMPLE_RATE as f32);
        assert_eq!(calls[0].end, 2048.0 / SAMPLE_RATE as f32);
    }

    // 覆盖 T-19/T-20/附录 D：整条音频不足一个窗口时不得整段漏掉并误报无语音。
    #[test]
    fn vad_short_audio_below_one_window_is_not_lost() {
        let calls = recorder();
        let mut vad = FakeVad::new(&[], &[(0, 300)]);
        let summary = run_stream(&mut vad, &calls, 300);
        assert_eq!(summary.total, 300);
        let calls = calls.borrow();
        assert_eq!(
            calls.len(),
            1,
            "短于一个窗口的音频必须保留语音片段：{calls:?}"
        );
        assert_eq!(calls[0].start, 0.0);
        assert_eq!(calls[0].end, 300.0 / SAMPLE_RATE as f32);
    }

    // 覆盖 T-19/T-20/附录 D：恰好整数个窗口的音频同样要经过 EOF 定稿路径。
    #[test]
    fn vad_exact_multiple_of_windows_completes() {
        let calls = recorder();
        let mut vad = FakeVad::new(&[], &[(0, VAD_WINDOW * 3)]);
        let summary = run_stream(&mut vad, &calls, VAD_WINDOW * 3);
        assert_eq!(summary.total, (VAD_WINDOW * 3) as u64);
        let calls = calls.borrow();
        assert_eq!(calls.len(), 1, "恰满窗口的音频不得丢失末段：{calls:?}");
        assert_eq!(calls[0].end, (VAD_WINDOW * 3) as f32 / SAMPLE_RATE as f32);
    }

    // 覆盖 T-20：末尾静音不得延长最后片段的结束时间戳。
    #[test]
    fn vad_trailing_silence_does_not_extend_last_segment() {
        let calls = recorder();
        let mut vad = FakeVad::new(&[(0, 1024)], &[]);
        let summary = run_stream(&mut vad, &calls, 4096);
        assert_eq!(summary.total, 4096);
        let calls = calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].end,
            1024.0 / SAMPLE_RATE as f32,
            "结束戳必须停在语音末尾"
        );
    }

    // 覆盖 T-20：全程无语音时输出空行集（界面显示「未检测到语音」），不是错误。
    #[test]
    fn vad_no_speech_reports_empty_without_error() {
        let calls = recorder();
        let mut vad = FakeVad::new(&[], &[]);
        let summary = run_stream(&mut vad, &calls, 4100);
        assert!(calls.borrow().is_empty());
        assert_eq!(summary.total, 4100);
    }

    // 覆盖 T-20/附录 D：多段流水的最后一段起止时间戳与样本内容必须完整。
    #[test]
    fn vad_multi_segment_stream_keeps_last_timestamps_and_content() {
        let calls = recorder();
        let mut vad = FakeVad::new(&[(0, 700), (1200, 2600)], &[(3000, 3600)]);
        let summary = run_stream(&mut vad, &calls, 3600);
        let calls = calls.borrow();
        assert_eq!(
            calls.len(),
            3,
            "全部三段（含 flush 定稿段）都应转写：{calls:?}"
        );
        let last = calls.last().expect("前一行已断言非空");
        assert_eq!(last.start, 3000.0 / SAMPLE_RATE as f32);
        assert_eq!(last.end, 3600.0 / SAMPLE_RATE as f32);
        // 流水线只保留最终文本行，不再持有任何片段 PCM。
        assert_eq!(summary.lines, 3);
    }

    // 覆盖附录 D 流式职责/T-19：VAD 出段即转写并释放 PCM，不得等整条音频解码完。
    #[test]
    fn vad_segments_transcribe_incrementally_during_stream() {
        let calls = recorder();
        let mut vad = FakeVad::new(&[(0, 1024)], &[]);
        let mut transcriber = FakeTranscriber::new(&calls);
        let mut pipeline = VadPipeline::new(&mut vad, &mut transcriber);
        // 前两个窗口已完整包含第一段语音：此刻它必须已被转写。
        pipeline
            .push_samples(&vec![0.0; VAD_WINDOW * 2])
            .expect("假 VAD 不会失败");
        assert_eq!(
            calls.borrow().len(),
            1,
            "片段在流式中途就应转写，不得推迟到整条音频结束"
        );
        pipeline
            .push_samples(&vec![0.0; VAD_WINDOW])
            .expect("假 VAD 不会失败");
        pipeline.finish().expect("假 VAD 不会失败");
        assert_eq!(calls.borrow().len(), 1);
    }

    // 覆盖附录 D 流式职责/T-19：总样本量远超上限的长流，常驻样本必须有界且末段完整。
    #[test]
    fn vad_pipeline_residency_bounded_for_long_stream() {
        const SEGMENT: usize = 1024;
        const WINDOWS: usize = 600;
        let total = WINDOWS * VAD_WINDOW;
        // 每 1024 样本产生一段语音，全程共 total/1024 段。
        let spans: Vec<(usize, usize)> = (0..total / SEGMENT)
            .map(|i| (i * SEGMENT, i * SEGMENT + SEGMENT))
            .collect();
        let calls = recorder();
        let mut vad = FakeVad::new(&spans, &[]);
        let summary = run_stream(&mut vad, &calls, total);
        let calls = calls.borrow();
        assert_eq!(calls.len(), total / SEGMENT, "长流的全部片段都必须转写");
        // 常驻上限：一个未转写片段 + 窗口缓冲（不足一窗口的尾样本）。
        assert!(
            summary.peak <= SEGMENT + VAD_WINDOW * 2,
            "常驻样本必须有界：峰值 {} 超过上限 {}",
            summary.peak,
            SEGMENT + VAD_WINDOW * 2
        );
        assert_eq!(
            calls.last().expect("前一行已断言非空").end,
            total as f32 / SAMPLE_RATE as f32
        );
    }
}
