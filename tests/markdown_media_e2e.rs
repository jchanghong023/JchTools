#![warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    future_incompatible,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]

//! 媒体转录真实链路 E2E（XB-02/T-19/T-20）。
//!
//! 本测试驱动真实公开接缝 `jchtools::markdown::e2e_convert_media`（组件解析 →
//! 常驻 `xberg.exe worker` → `transcribe` 请求 → SV-06 Markdown），对合成媒体
//! 断言转录正文与结构，不以「模型加载成功/程序未崩溃」代替结果断言。
//!
//! 运行前置（显式提供，测试自身不联网、不合成）：
//! - `JCHTOOLS_MEDIA_E2E_COMPONENT_DIR`：完整的 Xberg 推理组件目录（xberg.exe +
//!   SenseVoice/VAD 模型 + sherpa-onnx/FFmpeg DLL，见 XB-10）。测试自身把它经
//!   `xberg_settings::save` 存入隔离临时状态目录——组件目录不再有任何环境变量
//!   覆盖（2026-10-04 删除 `JCHTOOLS_XBERG_INFERENCE_DIR`），只能经设置页保存
//!   或产品内下载，本测试走的正是同一条设置链。
//! - `JCHTOOLS_MEDIA_E2E_INPUT`：第一个用例（含语音转录）的合成媒体文件
//!   （MP4/M4A，公开合成，不含业务内容，T-26），必须包含真实语音。
//! - `JCHTOOLS_MEDIA_E2E_TRACKLESS_INPUT`：第二个用例（无音轨）的合成
//!   媒体文件；显式运行时未设置即失败，不与第一个用例共用输入
//!   ——两者对正文的断言方向相反（S4-08），同一文件不可能同时满足。
//! - `JCHTOOLS_MEDIA_E2E_EXPECT_TEXT`：期望在第一个用例转录正文中出现的文本
//!   片段（逗号分隔多个候选，命中任一即通过）。
//!
//! 运行：`cargo test --features test-hooks --test markdown_media_e2e -- --ignored`。

mod common;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex, MutexGuard};

fn env_required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        panic!("缺少环境变量 {name}；本测试为真实组件门控用例，运行前置见文件头注释（XB-02）")
    })
}

/// `JCHTOOLS_TEST_STATE_DIR` 是进程级环境变量，两个门控用例可能并行运行，
/// 布置隔离状态目录必须串行。
static STATE_DIR_LOCK: Mutex<()> = Mutex::new(());
static STATE_DIRECTORY: LazyLock<tempfile::TempDir> = LazyLock::new(|| {
    let temporary_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".tmp");
    std::fs::create_dir_all(&temporary_root)
        .unwrap_or_else(|error| panic!("创建测试临时根失败：{error}"));
    tempfile::tempdir_in(&temporary_root)
        .unwrap_or_else(|error| panic!("创建隔离状态目录失败：{error}"))
});

struct ComponentSettings {
    /// 同一测试进程复用一个隔离目录，供常驻引擎与 SQLite 使用；
    /// 所有权由 STATE_DIRECTORY 持有，不为每个用例泄漏新的分配。
    _state: &'static tempfile::TempDir,
    _lock: MutexGuard<'static, ()>,
    _session: common::SessionLock,
    previous_state_dir: Option<std::ffi::OsString>,
}

impl Drop for ComponentSettings {
    fn drop(&mut self) {
        if let Some(previous) = &self.previous_state_dir {
            std::env::set_var("JCHTOOLS_TEST_STATE_DIR", previous);
        } else {
            std::env::remove_var("JCHTOOLS_TEST_STATE_DIR");
        }
    }
}

/// 把环境提供的完整组件目录经真实设置链（SQLite 保存）接入隔离状态目录；
/// 生产代码不读取任何组件目录环境变量。
fn configure_component_via_settings() -> ComponentSettings {
    common::ensure_child_reaper();
    let session = common::session_lock();
    let lock = STATE_DIR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let component = PathBuf::from(env_required("JCHTOOLS_MEDIA_E2E_COMPONENT_DIR"));
    assert!(
        component.is_absolute() && component.join("xberg.exe").is_file(),
        "组件目录必须是含 xberg.exe 的绝对路径：{}",
        component.display()
    );
    let state = &*STATE_DIRECTORY;
    let previous_state_dir = std::env::var_os("JCHTOOLS_TEST_STATE_DIR");
    // 保存失败或后续断言 panic 时也恢复配置；守卫始终在两种锁的保护内销毁。
    let settings = ComponentSettings {
        _state: state,
        _lock: lock,
        _session: session,
        previous_state_dir,
    };
    std::env::set_var("JCHTOOLS_TEST_STATE_DIR", state.path());
    jchtools::xberg_settings::save(&component)
        .unwrap_or_else(|error| panic!("经设置链保存组件目录失败：{error}"));
    settings
}

// 使用生产 SV-06 格式；与 markdown_acceptance.py 一样把「## 转录」后的
// 片段正文与标题/元数据分开，且无音轨与有音轨但无语音分别判定。
fn media_sections(markdown: &str) -> Result<(&str, &str), &'static str> {
    let (metadata, transcript) = markdown
        .split_once("\n## 转录\n")
        .ok_or("缺少独立的转录标题行")?;
    if transcript.lines().any(|line| line == "## 转录") {
        return Err("转录标题行重复");
    }
    Ok((metadata, transcript))
}

fn metadata_value<'a>(metadata: &'a str, prefix: &str) -> Result<&'a str, &'static str> {
    let mut values = metadata.lines().filter_map(|line| line.strip_prefix(prefix));
    let value = values.next().ok_or("缺少媒体统计行")?;
    if values.next().is_some() {
        return Err("媒体统计行重复");
    }
    Ok(value)
}

fn timestamp_ms(value: &str) -> Option<u64> {
    let (clock, millis) = value.split_once('.')?;
    let mut parts = clock.split(':');
    let hours = parts.next()?;
    let minutes = parts.next()?;
    let seconds = parts.next()?;
    if parts.next().is_some()
        || hours.len() < 2
        || minutes.len() != 2
        || seconds.len() != 2
        || millis.len() != 3
        || ![hours, minutes, seconds, millis]
            .iter()
            .all(|part| part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return None;
    }
    let hours = hours.parse::<u64>().ok()?;
    let minutes = minutes.parse::<u64>().ok()?;
    let seconds = seconds.parse::<u64>().ok()?;
    let millis = millis.parse::<u64>().ok()?;
    if minutes >= 60 || seconds >= 60 {
        return None;
    }
    hours.checked_mul(3_600_000)?.checked_add(minutes * 60_000 + seconds * 1000 + millis)
}

fn speech_transcript(markdown: &str) -> Result<Vec<&str>, &'static str> {
    let (metadata, body) = media_sections(markdown)?;
    let duration = timestamp_ms(metadata_value(metadata, "- 音频时长: ")?)
        .filter(|duration| *duration > 0)
        .ok_or("含语音媒体必须有合法的正时长")?;
    let count = metadata_value(metadata, "- 语音片段: ")?;
    if count.is_empty() || !count.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("片段数必须是非负整数");
    }
    let count = count.parse::<usize>().map_err(|_| "片段数无法解析")?;
    if count == 0 {
        return Err("含语音媒体必须有转录片段");
    }
    let mut transcript = Vec::new();
    let mut last_start = 0;
    for line in body.lines().filter(|line| !line.trim().is_empty()) {
        let (range, text) = line
            .strip_prefix('[')
            .and_then(|line| line.split_once("] "))
            .ok_or("转录片段缺少起止时间戳")?;
        let (start, end) = range.split_once(" --> ").ok_or("转录时间戳分隔符错误")?;
        let start = timestamp_ms(start).ok_or("片段开始时间戳非法")?;
        let end = timestamp_ms(end).ok_or("片段结束时间戳非法")?;
        if start >= end || end > duration || start < last_start {
            return Err("片段起止时间、音频范围或输出顺序非法");
        }
        if text.trim().is_empty() {
            return Err("转录片段正文为空");
        }
        last_start = start;
        transcript.push(text);
    }
    if transcript.len() != count {
        return Err("语音片段统计与实际转录片段数不一致");
    }
    Ok(transcript)
}

fn trackless_markdown(markdown: &str) -> Result<(), &'static str> {
    let (metadata, body) = media_sections(markdown)?;
    if metadata_value(metadata, "- 音频时长: ")? != "无音频轨道"
        || metadata_value(metadata, "- 语音片段: ")? != "0"
        || body.trim() != "（无音频轨道）"
    {
        return Err("无音轨结果必须有无音轨时长说明、零片段和独立正文说明");
    }
    Ok(())
}

// T-20 回归反例：标题/标签不能冒充正文、有效时间戳或片段统计。
#[test]
fn media_speech_predicate_rejects_metadata_and_invalid_segments() {
    let title_only = "# 期望正文.mp4\n\n- 音频时长: 标签\n- 语音片段: 标签\n";
    assert!(title_only.contains("期望正文"));
    assert!(title_only.contains("音频时长") && title_only.contains("语音片段"));
    assert!(speech_transcript(title_only).is_err());

    let valid = "# 期望正文.mp4\n\n- 音频时长: 00:00:02.000\n- 语音片段: 1\n\n## 转录\n\n[00:00:00.100 --> 00:00:01.500] 实际正文\n";
    let transcript = speech_transcript(valid).expect("生产格式的有效片段应被接受");
    assert_eq!(transcript, ["实际正文"]);
    assert!(!transcript.iter().any(|text| text.contains("期望正文")));
    for invalid in [
        valid.replace("- 音频时长: 00:00:02.000", "- 音频时长: 00:00:00.000"),
        valid.replace("- 语音片段: 1", "- 语音片段: 2"),
        valid.replace("- 语音片段: 1", "- 语音片段: 0"),
        valid.replace("00:00:00.100 -->", "00:60:00.100 -->"),
        valid.replace("--> 00:00:01.500", "--> 00:00:00.100"),
        valid.replace("--> 00:00:01.500", "--> 00:00:00.000"),
        valid.replace("--> 00:00:01.500", "--> 00:00:02.001"),
        valid.replace("] 实际正文", "] "),
        valid.replace("[00:00:00.100 --> 00:00:01.500] ", ""),
        valid.replace("## 转录", "## 其他"),
        valid.replace("- 语音片段: 1", "- 语音片段: 1\n- 语音片段: 1"),
    ] {
        assert!(speech_transcript(&invalid).is_err(), "不得接受：{invalid}");
    }
    let unordered = valid
        .replace("- 语音片段: 1", "- 语音片段: 2")
        + "[00:00:00.000 --> 00:00:00.050] 逆序正文\n";
    assert!(speech_transcript(&unordered).is_err());
}

// T-20/XB-07：有音轨但无语音不能充当无音轨用例的成功结果。
#[test]
fn media_trackless_predicate_distinguishes_silence() {
    let trackless = "# noaudio.mp4\n\n- 音频时长: 无音频轨道\n- 语音片段: 0\n\n## 转录\n\n（无音频轨道）\n";
    assert!(trackless_markdown(trackless).is_ok());
    let silent = trackless
        .replace("- 音频时长: 无音频轨道", "- 音频时长: 00:00:02.000")
        .replace("（无音频轨道）", "（未检测到语音）");
    assert!(silent.contains("无音频轨道") || silent.contains("未检测到语音"));
    assert!(trackless_markdown(&silent).is_err());
    assert!(trackless_markdown(&trackless.replace("- 语音片段: 0", "- 语音片段: 1")).is_err());
    assert!(trackless_markdown(&trackless.replace("（无音频轨道）", "（未检测到语音）")).is_err());
}

// 覆盖 XB-02/T-09/T-19/T-20：真实 Xberg 推理组件上转录合成媒体，
// 断言正文、时长和片段数，以及转换前后的源文件保护。
#[test]
#[ignore = "Requires real Xberg inference components and a synthetic media file (env-provided)"]
fn real_component_transcribe_returns_structured_markdown() {
    let _settings = configure_component_via_settings();
    let input = PathBuf::from(env_required("JCHTOOLS_MEDIA_E2E_INPUT"));
    let expected_raw = env_required("JCHTOOLS_MEDIA_E2E_EXPECT_TEXT");
    assert!(input.is_file(), "输入媒体不存在：{}", input.display());
    let original = std::fs::read(&input).expect("读取转换前媒体夹具失败");
    let expected: Vec<String> = expected_raw
        .split(',')
        .map(|fragment| fragment.replace(char::is_whitespace, ""))
        .filter(|fragment| !fragment.is_empty())
        .collect();
    assert!(!expected.is_empty(), "期望文本片段不得为空");

    let markdown = jchtools::markdown::e2e_convert_media(&input, 600)
        .unwrap_or_else(|error| panic!("真实组件转录必须成功：{error}"));
    assert_eq!(
        std::fs::read(&input).expect("读取转换后媒体夹具失败"),
        original,
        "T-09：成功转录不得改写或删除源媒体"
    );

    // T-20：正文非空且不是无解释空文件；无音轨说明属合法结果但本用例要求真实语音。
    assert!(
        !markdown.trim().is_empty(),
        "转录结果不得为空（无音轨/无语音应带明确说明，见 T-20）"
    );
    assert!(
        !markdown.contains("无音频轨道") && !markdown.contains("未检测到语音"),
        "门控用例应使用含语音的媒体：{markdown}"
    );
    // T-20 要求时长和片段数同时在场，不能只命中其中一个标签。
    assert!(
        markdown.contains("语音片段") && markdown.contains("音频时长"),
        "转录结果应同时包含片段数和音频时长（T-20）：{markdown}"
    );
    assert!(
        markdown.contains(input.file_name().unwrap().to_str().unwrap()),
        "转录结果应包含输入文件名（T-20）：{markdown}"
    );
    assert_eq!(
        markdown.lines().next(),
        Some(format!("# {}", input.file_name().unwrap().to_str().unwrap()).as_str()),
        "输入文件名必须位于媒体标题行（T-20）：{markdown}"
    );
    let transcript = speech_transcript(&markdown)
        .unwrap_or_else(|error| panic!("T-20 转录结构不合法：{error}：{markdown}"));
    let normalized_markdown = transcript.join("\n").replace(char::is_whitespace, "");
    let hit = expected
        .iter()
        .any(|fragment| normalized_markdown.contains(fragment));
    assert!(
        hit,
        "转录正文应命中期望片段 {expected:?}（术语按附录 C 映射）：{markdown}"
    );
}

// 覆盖 T-20/T-24：无音轨输入产出明确说明（不产生无解释空文件）；该用例使用
// 独立输入 JCHTOOLS_MEDIA_E2E_TRACKLESS_INPUT（S4-08：与「含语音」用例共用
// 同一输入时两断言方向相反，按文件头命令一起运行必有一败），缺依赖不得计通过。
#[test]
#[ignore = "Requires real Xberg inference components and a trackless synthetic media file"]
fn real_component_trackless_media_reports_no_audio() {
    let trackless = env_required("JCHTOOLS_MEDIA_E2E_TRACKLESS_INPUT");
    assert!(
        !trackless.trim().is_empty(),
        "JCHTOOLS_MEDIA_E2E_TRACKLESS_INPUT 不得为空；缺少夹具不能冒充验收通过"
    );
    let _settings = configure_component_via_settings();
    let input = PathBuf::from(trackless);
    assert!(input.is_file(), "输入媒体不存在：{}", input.display());
    let original = std::fs::read(&input).expect("读取转换前媒体夹具失败");
    let markdown = jchtools::markdown::e2e_convert_media(&input, 600)
        .unwrap_or_else(|error| panic!("真实组件转录无音轨媒体必须成功返回说明：{error}"));
    assert_eq!(
        std::fs::read(&input).expect("读取转换后媒体夹具失败"),
        original,
        "T-09：无音轨或无语音结果也不得改写或删除源媒体"
    );
    assert!(
        markdown.contains("无音频轨道") && !markdown.contains("未检测到语音"),
        "无音轨必须生成无音轨说明，不能冒充有音轨但无语音（T-20）：{markdown}"
    );
    assert_eq!(
        markdown.lines().next(),
        Some(format!("# {}", input.file_name().unwrap().to_str().unwrap()).as_str()),
        "无音轨结果也必须包含输入文件名标题（T-20）：{markdown}"
    );
    trackless_markdown(&markdown)
        .unwrap_or_else(|error| panic!("T-20 无音轨结构不合法：{error}：{markdown}"));
}
