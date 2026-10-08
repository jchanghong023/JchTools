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
//! - `JCHTOOLS_MEDIA_E2E_TRACKLESS_INPUT`：第二个用例（无音轨/无语音）的合成
//!   媒体文件；显式运行时未设置即失败，不与第一个用例共用输入
//!   ——两者对正文的断言方向相反（S4-08），同一文件不可能同时满足。
//! - `JCHTOOLS_MEDIA_E2E_EXPECT_TEXT`：期望在第一个用例转录正文中出现的文本
//!   片段（逗号分隔多个候选，命中任一即通过）。
//!
//! 运行：`cargo test --features test-hooks --test markdown_media_e2e -- --ignored`。

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

fn env_required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        panic!("缺少环境变量 {name}；本测试为真实组件门控用例，运行前置见文件头注释（XB-02）")
    })
}

/// `JCHTOOLS_TEST_STATE_DIR` 是进程级环境变量，两个门控用例可能并行运行，
/// 布置隔离状态目录必须串行。
static STATE_DIR_LOCK: Mutex<()> = Mutex::new(());

struct ComponentSettings {
    /// 泄漏以保住状态目录存活到进程结束（TempDir 会在 drop 时删除目录，
    /// 而常驻引擎/SQLite 都位于其中）。
    _state: &'static tempfile::TempDir,
    _lock: MutexGuard<'static, ()>,
}

/// 把环境提供的完整组件目录经真实设置链（SQLite 保存）接入隔离状态目录；
/// 生产代码不读取任何组件目录环境变量。
fn configure_component_via_settings() -> ComponentSettings {
    let lock = STATE_DIR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let component = PathBuf::from(env_required("JCHTOOLS_MEDIA_E2E_COMPONENT_DIR"));
    assert!(
        component.is_absolute() && component.join("xberg.exe").is_file(),
        "组件目录必须是含 xberg.exe 的绝对路径：{}",
        component.display()
    );
    let state = Box::leak(Box::new(
        tempfile::tempdir().unwrap_or_else(|error| panic!("创建隔离状态目录失败：{error}")),
    ));
    std::env::set_var("JCHTOOLS_TEST_STATE_DIR", state.path());
    jchtools::xberg_settings::save(&component)
        .unwrap_or_else(|error| panic!("经设置链保存组件目录失败：{error}"));
    ComponentSettings {
        _state: state,
        _lock: lock,
    }
}

// 覆盖 XB-02/T-19/T-20：真实 Xberg 推理组件上转录合成媒体，断言 SV-06 结构
// 与期望文本命中；并断言第二文件复用常驻进程语义由 e2e 接缝外层承载。
#[test]
#[ignore = "Requires real Xberg inference components and a synthetic media file (env-provided)"]
fn real_component_transcribe_returns_structured_markdown() {
    let _settings = configure_component_via_settings();
    let input = PathBuf::from(env_required("JCHTOOLS_MEDIA_E2E_INPUT"));
    let expected_raw = env_required("JCHTOOLS_MEDIA_E2E_EXPECT_TEXT");
    assert!(input.is_file(), "输入媒体不存在：{}", input.display());
    let expected: Vec<&str> = expected_raw
        .split(',')
        .map(str::trim)
        .filter(|fragment| !fragment.is_empty())
        .collect();
    assert!(!expected.is_empty(), "期望文本片段不得为空");

    let markdown = jchtools::markdown::e2e_convert_media(&input, 600)
        .unwrap_or_else(|error| panic!("真实组件转录必须成功：{error}"));

    // T-20：正文非空且不是无解释空文件；无音轨说明属合法结果但本用例要求真实语音。
    assert!(
        !markdown.trim().is_empty(),
        "转录结果不得为空（无音轨/无语音应带明确说明，见 T-20）"
    );
    assert!(
        !markdown.contains("无音频轨道") && !markdown.contains("未检测到语音"),
        "门控用例应使用含语音的媒体：{markdown}"
    );
    // T-20：片段时间戳结构（SV-06 由 Xberg 生成，客户端透传）。
    assert!(
        markdown.contains("语音片段") || markdown.contains("音频时长"),
        "转录结果应包含 SV-06 结构（片段数/时长）：{markdown}"
    );
    let hit = expected
        .iter()
        .any(|fragment| markdown.replace(char::is_whitespace, "").contains(fragment));
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
    let markdown = jchtools::markdown::e2e_convert_media(&input, 600)
        .unwrap_or_else(|error| panic!("真实组件转录无音轨媒体必须成功返回说明：{error}"));
    assert!(
        markdown.contains("无音频轨道") || markdown.contains("未检测到语音"),
        "无音轨/无语音必须生成明确说明（T-20）：{markdown}"
    );
}
