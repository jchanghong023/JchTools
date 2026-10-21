//! 共享 Xberg 推理承接：资产校验、连接与预热、识别、后台推理线程与停止重试。
//! 从 `service.rs` 按内聚职责原样搬移；行为与注释保持不变。

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::{Command, LoadFailure, OcrError, Work};
use crate::capture_win::BgrImage;
use crate::shared_xberg::{ClientError, SharedXbergClient, SnapshotState};

pub(super) const WORKER_STOP_ATTEMPTS: usize = 3;

pub(super) fn stop_background_with_retry<F>(mut control: F) -> Result<(), ()>
where
    F: FnMut() -> Result<Value, String>,
{
    let started = Instant::now();
    for attempt in 0..WORKER_STOP_ATTEMPTS {
        tracing::info!(
            event = "ocr_stop_attempt_started",
            attempt = attempt + 1,
            "请求停止共享后台"
        );
        match control() {
            Ok(state) if state["running"] == false => {
                tracing::info!(
                    event = "ocr_stop_completed",
                    attempt = attempt + 1,
                    elapsed_ms = crate::logging::elapsed_ms(started),
                    "共享后台已停止"
                );
                return Ok(());
            }
            Ok(_) | Err(_) => {}
        }
        tracing::warn!(
            event = "ocr_stop_attempt_failed",
            attempt = attempt + 1,
            elapsed_ms = crate::logging::elapsed_ms(started),
            error_type = "not_stopped",
            "共享后台停止未完成"
        );
        if attempt + 1 < WORKER_STOP_ATTEMPTS {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    tracing::warn!(
        event = "ocr_stop_exhausted",
        attempts = WORKER_STOP_ATTEMPTS,
        elapsed_ms = crate::logging::elapsed_ms(started),
        "共享后台停止重试耗尽"
    );
    Err(())
}

/// 截图服务使用应用级 SQLite 保存的共享 Xberg 目录，不提供环境变量覆盖。
pub(super) fn xberg_component_dir() -> Result<PathBuf, LoadFailure> {
    crate::xberg_settings::required().map_err(LoadFailure::NotConfigured)
}

/// 组件在位校验（XB-09：运行时只检查场景所需成员是否存在，不校验摘要）：
/// `xberg.exe` + `models/snapshot-ocr` 三个模型文件 + `onnxruntime.dll`。
pub(super) fn verify_component(dir: &Path) -> Result<(), LoadFailure> {
    let required = [
        dir.join("xberg.exe"),
        dir.join("models").join("snapshot-ocr").join("det.onnx"),
        dir.join("models").join("snapshot-ocr").join("rec.onnx"),
        dir.join("models")
            .join("snapshot-ocr")
            .join("dict")
            .join("dict.txt"),
        dir.join("onnxruntime.dll"),
    ];
    for path in &required {
        if !path.is_file() {
            return Err(LoadFailure::Failed(format!(
                "推理组件不完整：缺少 {}",
                path.strip_prefix(dir).unwrap_or(path).display()
            )));
        }
    }
    Ok(())
}

/// 共享运行时的缺失资产错误会附带用户提供的 Xberg 目录；服务提示不暴露该路径。
pub(super) fn redact_user_path(message: &str, directory: &Path) -> String {
    let path = directory.display().to_string();
    if path.is_empty() {
        message.to_owned()
    } else {
        message.replace(&path, "共享推理目录")
    }
}

const SNAP_ASSET_MANIFEST: &str = include_str!("../../../../resources/snap-ocr-assets.json");
const SNAP_FONT_MEMBER: &str = "fonts/NotoSansMonoCJKsc-Regular.otf";
const SNAP_FONT_LICENSE: &str = "fonts/LICENSE-noto-ofl.txt";

pub(super) fn verify_manifest_file(
    bytes: &[u8],
    size: u64,
    expected_sha256: &str,
    label: &str,
) -> Result<(), LoadFailure> {
    if bytes.len() as u64 != size {
        return Err(LoadFailure::Failed(format!("{label}大小校验失败")));
    }
    if expected_sha256.len() != 64 || !expected_sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(LoadFailure::Failed(format!("{label}清单摘要无效")));
    }
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let actual = hex::encode(hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        return Err(LoadFailure::Failed(format!("{label}摘要校验失败")));
    }
    Ok(())
}

pub(super) fn verify_font_assets(_root: &Path) -> Result<(), LoadFailure> {
    let manifest: Value = serde_json::from_str(SNAP_ASSET_MANIFEST)
        .map_err(|_| LoadFailure::Failed("截图字体清单损坏".into()))?;
    let assets = manifest["assets"]
        .as_array()
        .ok_or_else(|| LoadFailure::Failed("截图字体清单缺少资产".into()))?;
    let font = assets
        .iter()
        .find(|asset| asset["id"] == "noto-sans-mono-cjk-sc")
        .ok_or_else(|| LoadFailure::Failed("截图字体清单缺少字体条目".into()))?;
    if font["license"]["license"] != "OFL-1.1"
        || font["license"]["component"].as_str().is_none()
        || font["license"]["source"].as_str().is_none()
    {
        return Err(LoadFailure::Failed("截图字体许可清单无效".into()));
    }
    let member = font["members"]
        .as_array()
        .and_then(|members| {
            members
                .iter()
                .find(|member| member["install_path"] == SNAP_FONT_MEMBER)
        })
        .ok_or_else(|| LoadFailure::Failed("截图字体清单缺少字体文件".into()))?;
    let size = member["size_bytes"]
        .as_u64()
        .ok_or_else(|| LoadFailure::Failed("截图字体大小清单无效".into()))?;
    let sha256 = member["sha256"]
        .as_str()
        .ok_or_else(|| LoadFailure::Failed("截图字体摘要清单无效".into()))?;
    verify_manifest_file(
        crate::result_window::EMBEDDED_FONT,
        size,
        sha256,
        "截图字体",
    )?;

    let license = assets
        .iter()
        .find(|asset| asset["id"] == "noto-cjk-ofl-license")
        .ok_or_else(|| LoadFailure::Failed("截图字体清单缺少许可文件".into()))?;
    if license["license"]["license"] != "OFL-1.1"
        || license["license"]["component"].as_str().is_none()
        || license["license"]["source"].as_str().is_none()
    {
        return Err(LoadFailure::Failed("截图字体许可条目无效".into()));
    }
    let _license_path = license["install_path"]
        .as_str()
        .filter(|path| *path == SNAP_FONT_LICENSE)
        .ok_or_else(|| LoadFailure::Failed("截图字体许可路径无效".into()))?;
    let license_size = license["size_bytes"]
        .as_u64()
        .ok_or_else(|| LoadFailure::Failed("截图字体许可大小清单无效".into()))?;
    let license_sha256 = license["sha256"]
        .as_str()
        .ok_or_else(|| LoadFailure::Failed("截图字体许可摘要清单无效".into()))?;
    verify_manifest_file(
        include_bytes!("../../../../resources/fonts/LICENSE-noto-ofl.txt"),
        license_size,
        license_sha256,
        "截图字体许可文件",
    )
}

/// 连接共享 Xberg 客户端并完成预热（模型懒加载发生在首个识别请求，
/// 预热图触发加载后 `snapshot_state` 才会是 ready，O-13）。
pub(super) fn start_inference(root: &Path) -> Result<SharedXbergClient, LoadFailure> {
    verify_font_assets(root).map_err(|error| log_load_stage(error, "font_assets"))?;
    let component_dir =
        xberg_component_dir().map_err(|error| log_load_stage(error, "configuration"))?;
    verify_component(&component_dir).map_err(|error| log_load_stage(error, "component_assets"))?;
    crate::xberg_runtime::validate_assets(&component_dir, "snapshot").map_err(|error| {
        log_load_stage(
            LoadFailure::Failed(redact_user_path(&error, &component_dir)),
            "runtime_assets",
        )
    })?;
    let mut client = SharedXbergClient::connect(&component_dir);
    warm_up(&mut client)?;
    Ok(client)
}

pub(super) fn log_load_stage(error: LoadFailure, stage: &'static str) -> LoadFailure {
    tracing::warn!(
        event = "ocr_load_stage_failed",
        stage,
        error_type = "asset_or_configuration",
        "截图模型加载阶段失败"
    );
    error
}

/// 预热失败的分类文案：优先用响应的结构化 `error_kind`（与 Xberg
/// `snapshot_ocr.rs` 的取值全集对齐：`asset_invalid` / `input_invalid` /
/// `no_text` / `cancelled` / `internal`，SNAP-15）——`asset_invalid` 是模型/
/// 资产加载失败（SNAP-05）；真实 Xberg 的错误文本是英文（如 "snapshot model
/// asset missing"），旧实现只按中文子串匹配必然落空。`kind` 缺失（旧版 Xberg
/// 或无 `error_kind` 的失败响应）时回退到既有「模型」子串启发式兼容。
pub(super) fn warm_up_failure(kind: Option<&str>, message: &str) -> String {
    match kind {
        Some("asset_invalid") => format!("推理模型加载失败：{message}"),
        // kind 缺失时回退中文「模型」子串启发式（兼容旧版 Xberg）。
        None if message.contains("模型") => format!("推理模型加载失败：{message}"),
        // 其余 kind 与不含「模型」的回退场景都按组件预热失败分类。
        Some(_) | None => format!("推理组件预热失败：{message}"),
    }
}

/// 预热：向常驻子进程发一张 1×1 白图，触发 Xberg 侧模型懒加载，并确认通道
/// 状态进入 ready（O-13 预热行为；无文字图片是成功响应）。
pub(super) fn warm_up(client: &mut SharedXbergClient) -> Result<(), LoadFailure> {
    let started = Instant::now();
    tracing::debug!(event = "ocr_prewarm_started", "开始预热截图模型");
    let outcome = warm_up_inner(client);
    if outcome.is_ok() {
        tracing::info!(
            event = "ocr_prewarm_completed",
            elapsed_ms = crate::logging::elapsed_ms(started),
            result = "ready",
            "截图模型预热完成"
        );
    } else {
        tracing::warn!(
            event = "ocr_prewarm_failed",
            elapsed_ms = crate::logging::elapsed_ms(started),
            error_type = "prewarm",
            "截图模型预热失败"
        );
    }
    outcome
}

pub(super) fn warm_up_inner(client: &mut SharedXbergClient) -> Result<(), LoadFailure> {
    let white = BgrImage::from_vec(1, 1, vec![255, 255, 255])
        .map_err(|_| LoadFailure::Failed("预热图像无效".into()))?;
    let png = white.png_bytes().map_err(LoadFailure::Failed)?;
    let cancel = AtomicBool::new(false);
    match client.recognize(&png, &cancel) {
        Ok(_) => {}
        Err(ClientError::Backend { message, kind }) => {
            return Err(LoadFailure::Failed(warm_up_failure(
                kind.as_deref(),
                &message,
            )));
        }
        Err(error) => return Err(LoadFailure::Failed(error.to_string())),
    }
    match client.snapshot_state() {
        Ok(SnapshotState::Ready) => Ok(()),
        Ok(SnapshotState::Error(message)) => {
            Err(LoadFailure::Failed(format!("推理模型加载失败：{message}")))
        }
        Ok(state) => Err(LoadFailure::Failed(format!(
            "推理组件未进入就绪状态（{}）",
            state.as_str()
        ))),
        Err(error) => Err(LoadFailure::Failed(error.to_string())),
    }
}

/// ClientError → OcrError 分类映射。请求返回即已越过原请求终态边界；
/// 超时只降级截图模型状态并保留重试入口，不终止共享引擎或其他场景。
pub(super) fn ocr_error(error: ClientError) -> OcrError {
    match error {
        ClientError::Cancelled => OcrError::Cancelled,
        ClientError::Timeout => {
            OcrError::TimedOut("当前截图识别超时；共享引擎及其他功能保持运行".into())
        }
        error @ ClientError::ProcessExited(_) => OcrError::ProcessExited(error.to_string()),
        ClientError::Backend {
            message,
            kind: Some(kind),
        } if kind == "asset_invalid" => OcrError::ModelFailure(message),
        ClientError::Backend {
            message,
            kind: Some(kind),
        } if kind == "shared_runtime" => OcrError::Communication(message),
        ClientError::Io(message) => OcrError::Communication(message),
        error => OcrError::Backend(error.to_string()),
    }
}

/// 识别一张裁剪图：内存 PNG 编码后交给 Xberg 子进程，取回布局文本。
pub(super) fn recognize(
    client: &mut SharedXbergClient,
    image: &BgrImage,
    cancel: &AtomicBool,
) -> Result<Option<String>, OcrError> {
    let encode_started = Instant::now();
    tracing::debug!(event = "ocr_encode_started", "编码内存截图");
    let png = image.png_bytes().map_err(|error| {
        tracing::warn!(
            event = "ocr_encode_failed",
            error_type = "image_encode",
            elapsed_ms = crate::logging::elapsed_ms(encode_started),
            "内存截图编码失败"
        );
        OcrError::Backend(error)
    })?;
    tracing::debug!(
        event = "ocr_encode_completed",
        size_bytes = png.len(),
        elapsed_ms = crate::logging::elapsed_ms(encode_started),
        "内存截图编码完成"
    );
    match client.recognize(&png, cancel) {
        Ok(Some(text)) if text.trim().is_empty() => Ok(None),
        Ok(text) => Ok(text),
        Err(error) => Err(ocr_error(error)),
    }
}

/// 重载决策（S8-02）：探测结果为「引擎仍就绪」时免重预热直接复用；探测
/// 不可得（未配置/通信失败）或状态非就绪都走完整重载。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReloadPlan {
    /// 引擎报告就绪：校验本服务字体后重建客户端，不重复预热模型。
    Reuse,
    /// 完整加载（资产校验 + 连接 + 预热，O-13）。
    Full,
}
pub(super) fn reload_plan(probe: Option<&Result<SnapshotState, ClientError>>) -> ReloadPlan {
    match probe {
        Some(Ok(SnapshotState::Ready)) => ReloadPlan::Reuse,
        Some(Ok(_) | Err(_)) | None => ReloadPlan::Full,
    }
}

pub(super) fn reuse_inference(
    root: &Path,
    component_dir: &Path,
) -> Result<SharedXbergClient, LoadFailure> {
    verify_font_assets(root).map_err(|error| log_load_stage(error, "reuse_font_assets"))?;
    Ok(SharedXbergClient::connect(component_dir))
}

pub(super) fn worker(
    root: &Path,
    receiver: &mpsc::Receiver<Work>,
    events: &mpsc::Sender<Command>,
    cancel: &AtomicBool,
) {
    let mut client: Option<SharedXbergClient> = None;
    let mut engine_pid = None;
    tracing::info!(event = "ocr_worker_started", "截图推理线程已启动");
    let mut verified_root = None;
    loop {
        let work = match receiver.recv_timeout(Duration::from_secs(3)) {
            Ok(work) => work,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            // 通道断开是正常服务收尾，末端统一记录。
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let alive = (|| {
                    let component =
                        crate::xberg_settings::required().map_err(LoadFailure::NotConfigured)?;
                    if verified_root.as_ref() != Some(&component) {
                        crate::xberg_runtime::validate_assets(&component, "engine").map_err(
                            |error| LoadFailure::Failed(redact_user_path(&error, &component)),
                        )?;
                        verified_root = Some(component.clone());
                    }
                    crate::xberg_runtime::request(
                        &component,
                        json!({"command":"keepalive"}),
                        Duration::from_secs(15),
                        &AtomicBool::new(false),
                    )
                    .and_then(crate::xberg_runtime::checked)
                    .map_err(LoadFailure::Failed)
                })();
                match alive {
                    Ok(value) => {
                        // S8-04：保活只比较 jchtools_xberg_pid——broker 的 keepalive
                        // 响应（crates/xberg/src/xberg_runtime_windows.rs）仅含 jchtools_xberg_pid
                        // 与 broker 协议字段，没有引擎启动时间或代次编号；Windows 的
                        // PID 复用因此可能掩盖引擎代次变化、漏一次重建预热（后果
                        // 有限：下一次识别因模型未热而变慢）。补齐需要 Xberg/broker
                        // 侧在响应中暴露引擎启动时间或代次，属跨模块配合项，本轮
                        // 不强行实现：以本注释说明边界与后果（SNAP2TEXT 附录 C
                        // 「后台生命周期」不受影响，模型常驻语义由引擎侧保证）。
                        let pid = value["jchtools_xberg_pid"].as_u64();
                        if pid == engine_pid {
                            continue;
                        }
                        engine_pid = pid;
                        tracing::info!(
                            event = "ocr_engine_changed",
                            peer_pid = pid,
                            "共享引擎实例变化，重新探测模型"
                        );
                        Work::Load(tracing::Span::current())
                    }
                    Err(failure) => {
                        tracing::warn!(
                            event = "ocr_keepalive_failed",
                            error_type = match &failure {
                                LoadFailure::NotConfigured(_) => "not_configured",
                                LoadFailure::Failed(_) => "runtime_unavailable",
                            },
                            "共享引擎保活失败，模型降级"
                        );
                        client.take();
                        engine_pid = None;
                        let _ = events.send(Command::ModelLoaded(Err(failure)));
                        continue;
                    }
                }
            }
        };
        match work {
            Work::Load(parent_span) => {
                let _parent_entered = parent_span.enter();
                let span = crate::logging::operation_span("snap_ocr", "load");
                let _entered = span.enter();
                let started = Instant::now();
                tracing::debug!(event = "ocr_load_started", "开始加载或复用截图模型");
                // 重试入口（O-13）与取消恢复共用。共享引擎为协作取消（XB-14）：
                // 取消不再终止引擎、模型仍常驻，先探测 snapshot 状态；仍就绪则
                // 免完整重预热直接复用（S8-02——旧实现无条件重载预热是直连子
                // 进程时代「取消即杀进程」的遗留语义），探测不可用或非就绪才走
                // 完整加载（校验 + 连接 + 预热）。
                let probe = crate::xberg_settings::required().ok().map(|dir| {
                    let mut probe_client = SharedXbergClient::connect(&dir);
                    probe_client.snapshot_state()
                });
                match reload_plan(probe.as_ref()) {
                    ReloadPlan::Reuse => {
                        tracing::debug!(
                            event = "ocr_reload_selected",
                            result = "reuse",
                            "复用已就绪截图模型"
                        );
                        // 探测刚确认已配置；此处失败只可能是竞态，按未配置报错
                        // 走 O-13 的初始化入口。
                        match crate::xberg_settings::required() {
                            Ok(dir) => {
                                let outcome = reuse_inference(root, &dir);
                                let status = outcome.as_ref().map(|_| ()).map_err(Clone::clone);
                                log_load_outcome(&status, started, "reuse");
                                client = outcome.ok();
                                let _ = events.send(Command::ModelLoaded(status));
                            }
                            Err(reason) => {
                                tracing::warn!(
                                    event = "ocr_load_failed",
                                    stage = "reuse_config",
                                    error_type = "not_configured",
                                    elapsed_ms = crate::logging::elapsed_ms(started),
                                    "截图模型复用配置不可用"
                                );
                                client.take();
                                let _ = events.send(Command::ModelLoaded(Err(
                                    LoadFailure::NotConfigured(reason),
                                )));
                            }
                        }
                    }
                    ReloadPlan::Full => {
                        tracing::debug!(
                            event = "ocr_reload_selected",
                            result = "full",
                            "完整加载并预热截图模型"
                        );
                        client.take();
                        let outcome = start_inference(root);
                        let status = outcome.as_ref().map(|_| ()).map_err(Clone::clone);
                        log_load_outcome(&status, started, "full");
                        client = outcome.ok();
                        let _ = events.send(Command::ModelLoaded(status));
                    }
                }
            }
            Work::Recognize(image, work, span) => {
                let _entered = span.enter();
                let started = Instant::now();
                tracing::info!(event = "ocr_infer_started", "开始截图识别");
                let outcome = if let Some(model) = client.as_mut() {
                    recognize(model, &image, cancel)
                } else {
                    Err(OcrError::Backend("模型未就绪".into()))
                };
                match &outcome {
                    Ok(text) => tracing::info!(
                        event = "ocr_infer_completed",
                        result = if text.is_some() { "success" } else { "empty" },
                        text_bytes = text.as_ref().map_or(0, String::len),
                        elapsed_ms = crate::logging::elapsed_ms(started),
                        "截图识别完成"
                    ),
                    Err(error) => tracing::warn!(
                        event = "ocr_infer_failed",
                        error_type = error.diagnostic_kind(),
                        elapsed_ms = crate::logging::elapsed_ms(started),
                        "截图识别未成功"
                    ),
                }
                match &outcome {
                    // XB-17：终态只重置本场景客户端，不结束共享引擎。
                    // 未确认结束的任务仍由代理占用本场景，防止重复提交。
                    Err(
                        OcrError::Cancelled
                        | OcrError::TimedOut(_)
                        | OcrError::ProcessExited(_)
                        | OcrError::Communication(_)
                        | OcrError::ModelFailure(_),
                    ) => {
                        drop(client.take());
                    }
                    Ok(_) | Err(OcrError::Backend(_)) => {}
                }
                let _ = events.send(Command::OcrFinished(outcome, work));
            }
            Work::Stop => {
                let span = crate::logging::operation_span("snap_ocr", "stop");
                let _entered = span.enter();
                tracing::info!(event = "ocr_stop_started", "截图推理线程开始停止");
                drop(client.take());
                // GUI 任务已结束，截图调用已返回；请求代理排空剩余业务响应并退出。
                match stop_background_with_retry(|| crate::xberg_runtime::background_control(true))
                {
                    Ok(()) => {
                        let _ = events.send(Command::WorkerStopped(span.clone()));
                        break;
                    }
                    Err(()) => {
                        let _ = events.send(Command::WorkerStopFailed(span.clone()));
                    }
                }
            }
        }
    }
    tracing::info!(event = "ocr_worker_stopped", "截图推理线程已停止");
}

pub(super) fn log_load_outcome(
    outcome: &Result<(), LoadFailure>,
    started: Instant,
    mode: &'static str,
) {
    match outcome {
        Ok(()) => tracing::info!(
            event = "ocr_load_completed",
            mode,
            elapsed_ms = crate::logging::elapsed_ms(started),
            result = "ready",
            "截图模型就绪"
        ),
        Err(failure) => tracing::warn!(
            event = "ocr_load_failed",
            mode,
            error_type = match failure {
                LoadFailure::NotConfigured(_) => "not_configured",
                LoadFailure::Failed(_) => "load_failed",
            },
            elapsed_ms = crate::logging::elapsed_ms(started),
            "截图模型加载失败"
        ),
    }
}
