use super::tests_support::{redirect_component_env, sha256_bytes};
use super::{load_manifest, media_component_dir, write_notice};
use jchtools_assets::asset_util::{
    inference_ready, install_inference_pack, resolve_component_with_tag, AssetDownloader,
    InferenceManifest, InferenceMember,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

// 覆盖 T-06：内置清单必须可解析（媒体段退役后仅存 xberg 段）。
#[test]
fn manifest_parses_with_xberg_segment_only() {
    let manifest = load_manifest().expect("内置资产清单必须可解析");
    assert!(
        !manifest.xberg.members.is_empty(),
        "xberg 固定版本成员清单不得为空"
    );
}

// 覆盖 XB-19/T-05（回归：任务预检按所选分组分场景——纯媒体目录缺文档模型
// 不再被文档条件提前拒绝；缺文档模型的文档任务启动前明确报错而不是静默放行
// 后逐文件失败；page_readiness 允许任一场景可用即进入转换页）
#[test]
fn readiness_for_groups_checks_only_requested_scenarios() {
    // 平台门槛前置分支（T-03：转 Markdown 仅支持 Windows 11 x64，不满足时
    // 必须在启动前明确拒绝）。CI 与其他 Server 宿主（如 windows-2022 runner，
    // build 20348 / Server 产品类型）上 platform_preflight 必须短路——此时
    // 场景就绪组合不可观察，只断言平台错误如实透传；下方场景组合断言在
    // Win11 开发机（fastcheck/fulltest 的 cargo test）上照常执行，覆盖不缩小。
    // 背景：CI run 37128504702 在 Server runner 上因原测试无条件期待 Ok 而失败
    //（validate_media 通过、readiness_for_groups 因平台门槛报错），该期望与
    // T-03 相矛盾，故按合同纠正测试而非放宽产品行为。
    if crate::markdown::platform_preflight().is_err() {
        let platform_error =
            crate::markdown::readiness_for_groups(&[crate::markdown::FormatGroup::Media])
                .expect_err("非 Windows 11 平台必须在启动前拒绝转换");
        assert!(
            platform_error.contains("Windows 11"),
            "平台错误必须如实说明原因：{platform_error}"
        );
        return;
    }
    let guard = redirect_component_env();
    let component = install_component(guard.root.path());
    // install_component 只建 media_component_dir 在位校验所需文件；
    // 清单成员检查还要求根级通用成员（engine 运行库与许可），一并补齐。
    for extra in [
        "onnxruntime.dll",
        "onnxruntime_providers_shared.dll",
        "LICENSE",
        "THIRD_PARTY_LICENSES.md",
        "MSVCP140.dll",
        "MSVCP140_1.dll",
        "VCRUNTIME140.dll",
        "VCRUNTIME140_1.dll",
        "ffmpeg/LICENSE.txt",
        "sherpa-onnx/LICENSE",
    ] {
        fs::write(component.join(extra), b"stub").expect("预置通用成员");
    }
    // 前置：媒体场景成员齐备，文档场景成员（models/models--… 文档模型）缺位。
    assert!(
        super::validate_media().is_ok(),
        "前置：媒体组件就绪（组件目录 {}）：{:?}",
        component.display(),
        super::validate_media()
    );
    // 纯媒体分组：不再被文档条件拒绝（修复前 run() 无条件 readiness()）。
    assert!(crate::markdown::readiness_for_groups(&[crate::markdown::FormatGroup::Media]).is_ok());
    // 纯文档分组：启动前明确报文档组件未就绪，并指认缺失。
    let error = crate::markdown::readiness_for_groups(&[crate::markdown::FormatGroup::Pdf])
        .expect_err("缺文档模型的文档任务必须在启动前拒绝");
    assert!(
        error.contains("文档转换组件未就绪"),
        "错误必须区分场景并指认文档组件：{error}"
    );
    // 混选分组：两个场景都必须就绪，仍报未就绪的那个。
    let error = crate::markdown::readiness_for_groups(&[
        crate::markdown::FormatGroup::Pdf,
        crate::markdown::FormatGroup::Media,
    ])
    .expect_err("混选分组要求全部所选场景就绪");
    assert!(
        error.contains("文档转换组件未就绪"),
        "错误必须指认未就绪场景：{error}"
    );
    // 页面门槛（XB-19）：任一场景可用即可开始，纯媒体目录可进入转换页。
    assert!(crate::markdown::page_readiness().is_ok());
}

/// 搭建组件在位校验所需的完整文件树（存在性校验，内容任意）。
/// 目录名与真实清单的推理组件 tag 保持一致（清单未接线时回退固定名），
/// 与 `resolve_xberg_component` 的解析规则同步。
fn install_component(base: &Path) -> PathBuf {
    let tag = load_manifest()
        .ok()
        .and_then(|manifest| manifest.xberg_inference.map(|inference| inference.tag))
        .unwrap_or_else(|| "vtest".to_string());
    let component = base.join("xberg-inference").join(tag);
    let files = [
        "xberg.exe",
        "models/sense_voice_zh_en_ja_ko_yue_2024_07_17/model.int8.onnx",
        "models/sense_voice_zh_en_ja_ko_yue_2024_07_17/tokens.txt",
        "models/vad/silero_vad.onnx",
        "sherpa-onnx/sherpa-onnx-c-api.dll",
        "sherpa-onnx/sherpa-onnx-cxx-api.dll",
        "sherpa-onnx/onnxruntime.dll",
        "sherpa-onnx/onnxruntime_providers_shared.dll",
        "ffmpeg/avutil-61.dll",
        "ffmpeg/swresample-7.dll",
        "ffmpeg/avcodec-63.dll",
        "ffmpeg/avformat-63.dll",
    ];
    for file in files {
        let target = component.join(file);
        fs::create_dir_all(target.parent().expect("组件路径有父目录")).expect("创建组件目录");
        fs::write(&target, b"x").expect("预置组件文件");
    }
    fs::write(
        base.join("xberg-runtime-path.txt"),
        component.to_str().expect("测试路径有效"),
    )
    .expect("模拟升级前的目录选择");
    component
}

// 覆盖 T-05/T-06（XB-01/XB-12）：媒体组件按在位校验，齐全时返回组件目录。
#[test]
fn media_component_ready_returns_component_dir() {
    let guard = redirect_component_env();
    let component = install_component(guard.root.path());
    let resolved = media_component_dir().expect("齐全组件应通过在位校验");
    assert_eq!(resolved, component);
    assert_eq!(
        guard.root.path().join("xberg-inference"),
        super::xberg_inference_root(),
        "安装根必须固定在 <资产根>/xberg-inference"
    );
}

// 覆盖 T-06：组件不完整时明确指出缺失项，不执行不完整环境。
#[test]
fn media_component_incomplete_reports_missing_file() {
    let guard = redirect_component_env();
    let component = install_component(guard.root.path());
    fs::remove_file(component.join("ffmpeg").join("avcodec-63.dll")).expect("删除一个 DLL");
    let error = media_component_dir().expect_err("不完整组件必须失败");
    assert!(
        error.contains("avcodec-63.dll"),
        "错误应指出缺失文件：{error}"
    );
    assert!(
        error.contains("推理组件不完整"),
        "错误应说明组件不完整：{error}"
    );
}

// 覆盖 T-05：设置页保存的本地目录即组件目录（2026-10-04 起组件目录只能
// 经配置页保存或产品内下载，环境变量覆盖已删除）；缺媒体模型时明确指出
// 缺失项，不执行不完整环境。
#[test]
fn media_component_saved_dir_points_at_local_tree() {
    let guard = redirect_component_env();
    let external = guard.root.path().join("external-component");
    fs::create_dir_all(&external).expect("创建外部组件目录");
    fs::write(external.join("xberg.exe"), b"x").expect("预置外部组件文件");
    jchtools_foundation::xberg_settings::save(&external).expect("经设置链保存本地组件目录");
    let resolved = media_component_dir().expect_err("外部树缺模型时必须指出缺失项");
    assert!(resolved.contains("model.int8.onnx"), "{resolved}");
}

// 覆盖 XB-19：保存共享目录时不强制其他场景的模型就绪；每个功能仅在自身
// 启动前按场景检查。即使目录缺少截图和媒体模型，文档所需成员齐全也可保存。
#[test]
fn save_runtime_dir_allows_other_scenarios_to_be_unready() {
    let guard = redirect_component_env();
    let manifest = load_manifest().expect("内置清单必须可解析");
    let engine = tempfile::tempdir().expect("创建自选引擎目录");
    for member in &manifest.xberg.members {
        if !jchtools_xberg::xberg_runtime::asset_for_scenario(&member.path, "document") {
            continue;
        }
        let path = engine.path().join(&member.path);
        fs::create_dir_all(path.parent().expect("成员路径有父目录")).expect("创建成员父目录");
        fs::write(&path, format!("stub-{}", member.path)).expect("写入成员桩文件");
    }

    jchtools_xberg::xberg_runtime::validate_assets(engine.path(), "document")
        .expect("文档场景成员齐全");
    assert!(
        jchtools_xberg::xberg_runtime::validate_assets(engine.path(), "snapshot").is_err(),
        "缺截图模型时截图场景应仍未就绪"
    );
    assert!(
        jchtools_xberg::xberg_runtime::validate_assets(engine.path(), "media").is_err(),
        "缺媒体模型时媒体场景应仍未就绪"
    );

    super::save_runtime_dir(engine.path()).expect("文档可用的自选目录必须可保存");
    let notice = guard
        .root
        .path()
        .join("licenses")
        .join("THIRD_PARTY_NOTICES.md");
    assert!(
        notice.is_file(),
        "保存自选目录后必须写入 notice：{}",
        notice.display()
    );
    assert!(
        super::readiness().is_ok(),
        "保存自选目录即应满足文档场景 readiness：{:?}",
        super::readiness()
    );
}

// 覆盖 T-06/XB-10（回归：清单 xberg 与 xberg_inference 两段的
// tag/URL/大小/SHA-256 四值重复但各段独立校验——漂移会让设置页下载与
// 媒体成员校验口径分叉；交叉校验必须在漂移时报错，不能静默放行）。
#[test]
fn manifest_cross_segment_drift_is_rejected() {
    let mark = "\"xberg_inference\"";
    let tamper = |from: &str, to: &str| {
        let position = super::MANIFEST.find(mark).expect("内置清单包含推理段");
        let (head, tail) = super::MANIFEST.split_at(position);
        format!("{head}{}", tail.replacen(from, to, 1))
    };
    // tag 漂移（换成另一合法形态 tag，先通过单段校验再触发段间校验）。
    let tampered = tamper(super::XBERG_TAG, "v2026.10.9-9999-run99.1");
    let error = super::parse_manifest(&tampered).expect_err("推理段 tag 漂移必须报错");
    assert!(
        error.contains("不一致"),
        "错误应说明段间元数据不一致：{error}"
    );
    // sha256 漂移（等长 64 位十六进制，单段长度检查照常通过）。
    let sha = load_manifest()
        .expect("内置清单必须可解析")
        .xberg
        .archive_sha256
        .clone();
    let tampered = tamper(&sha, &"0".repeat(64));
    assert!(
        super::parse_manifest(&tampered).is_err(),
        "推理段 sha256 漂移必须报错"
    );
    // 对照：内置清单两段一致，必须照常通过。
    super::parse_manifest(super::MANIFEST).expect("内置清单两段一致必须通过");
}

#[test]
fn notice_lists_only_xberg_licenses() {
    let manifest = load_manifest().expect("内置资产清单必须可解析");
    let root = tempfile::tempdir().expect("创建测试目录");
    let notice = root.path().join("THIRD_PARTY_NOTICES.md");
    write_notice(&notice, &manifest).expect("写入 notice");
    let text = fs::read_to_string(&notice).expect("读取 notice");
    assert!(text.contains("Xberg CLI"), "应包含 Xberg CLI 条目：{text}");
    assert!(
        !text.contains("SenseVoice INT8"),
        "媒体模型许可条目已退役，不得出现：{text}"
    );
}

// 覆盖 T-06/XB-20：GUI 与后台同时修复缺失 notice，不得互相替换目录或留下截断成品。
#[test]
fn concurrent_notice_repairs_publish_one_complete_notice() {
    let guard = redirect_component_env();
    let expected = guard.root.path().join("expected-notice.md");
    write_notice(&expected, &load_manifest().unwrap()).unwrap();
    let target = guard.root.path().join("licenses/THIRD_PARTY_NOTICES.md");
    let start = std::sync::Arc::new(std::sync::Barrier::new(32));
    let workers: Vec<_> = (0..32)
        .map(|_| {
            let start = start.clone();
            std::thread::spawn(move || {
                start.wait();
                super::ensure_document_notice()
            })
        })
        .collect();
    let results: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert!(
        results.iter().all(Result::is_ok),
        "并发许可证修复失败：{results:?}"
    );
    assert_eq!(fs::read(&target).unwrap(), fs::read(&expected).unwrap());
}

// ── Xberg 推理组件包：下载安装、复用、失败保护与版本一致性（XB-09/XB-10）──

/// 构造两成员（xberg.exe + 字典）的推理组件包夹具：内存 zip + 对应清单。
fn fixture_inference_pack() -> (InferenceManifest, Vec<u8>) {
    let exe = b"fake-xberg-exe-bytes".to_vec();
    let dict = b"fake-snapshot-dict".to_vec();
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        use std::io::Write as IoWrite;
        let mut writer = zip::ZipWriter::new(&mut cursor);
        let options = zip::write::SimpleFileOptions::default();
        writer
            .start_file("pkg/xberg.exe", options)
            .expect("写入 zip 成员");
        writer.write_all(&exe).expect("写入成员字节");
        writer
            .start_file("pkg/models/snapshot-ocr/dict.txt", options)
            .expect("写入 zip 成员");
        writer.write_all(&dict).expect("写入成员字节");
        writer.finish().expect("完成 zip");
    }
    let archive = cursor.into_inner();
    let manifest = InferenceManifest {
        tag: "vtest-inference".to_string(),
        url: "https://fixtures.invalid/inference.zip".to_string(),
        size_bytes: archive.len() as u64,
        sha256: sha256_bytes(&archive),
        members: vec![
            InferenceMember {
                path: "pkg/xberg.exe".to_string(),
                install_path: "xberg.exe".to_string(),
                size_bytes: exe.len() as u64,
                sha256: sha256_bytes(&exe),
            },
            InferenceMember {
                path: "pkg/models/snapshot-ocr/dict.txt".to_string(),
                install_path: "models/snapshot-ocr/dict.txt".to_string(),
                size_bytes: dict.len() as u64,
                sha256: sha256_bytes(&dict),
            },
        ],
    };
    (manifest, archive)
}

/// 注入式下载器：供给内存字节、可编程失败、记录调用次数。
struct FakeInferenceDownloader {
    archive: Vec<u8>,
    fail: bool,
    calls: usize,
}

impl AssetDownloader for FakeInferenceDownloader {
    fn download(
        &mut self,
        _url: &str,
        destination: &Path,
        _expected_size: u64,
        _expected_sha256: &str,
        _cancel: &AtomicBool,
        _progress: &mut dyn FnMut(String),
    ) -> Result<(), String> {
        self.calls += 1;
        if self.fail {
            return Err("模拟下载失败".to_string());
        }
        fs::write(destination, &self.archive).map_err(|error| error.to_string())
    }
}

fn run_install(
    inference: &InferenceManifest,
    root: &Path,
    downloader: &mut FakeInferenceDownloader,
) -> Result<(), String> {
    let staging = root.join("staging");
    fs::create_dir_all(&staging).expect("创建 staging");
    let cancel = AtomicBool::new(false);
    let mut progress = |_message: String| {};
    install_inference_pack(
        inference,
        &staging,
        root,
        &cancel,
        downloader,
        &mut progress,
    )
}

// 覆盖 XB-10：组件包按成员落位到 xberg-inference/<tag>/，旧版本目录被移除。
#[test]
fn inference_pack_installs_members_and_prunes_old_tags() {
    let root = tempfile::tempdir().expect("创建测试根");
    let (manifest, archive) = fixture_inference_pack();
    let old = root.path().join("xberg-inference").join("vold");
    fs::create_dir_all(&old).expect("预置旧版本目录");
    fs::write(old.join("xberg.exe"), b"old").expect("预置旧文件");

    let mut downloader = FakeInferenceDownloader {
        archive,
        fail: false,
        calls: 0,
    };
    run_install(&manifest, root.path(), &mut downloader).expect("安装推理组件包应成功");

    let component = root.path().join("xberg-inference").join("vtest-inference");
    assert_eq!(
        fs::read(component.join("xberg.exe")).expect("xberg.exe 已安装"),
        b"fake-xberg-exe-bytes"
    );
    assert_eq!(
        fs::read(
            component
                .join("models")
                .join("snapshot-ocr")
                .join("dict.txt")
        )
        .expect("字典已安装"),
        b"fake-snapshot-dict"
    );
    assert!(!old.exists(), "旧版本目录必须被移除（不混用版本）");
    assert_eq!(downloader.calls, 1);
    inference_ready(&manifest, root.path()).expect("安装后成员级校验应通过");
}

// 覆盖 XB-10「保留已校验资产」：已验证安装不重下，下载器不得被调用。
#[test]
fn inference_pack_reuse_skips_download() {
    let root = tempfile::tempdir().expect("创建测试根");
    let (manifest, archive) = fixture_inference_pack();
    let mut downloader = FakeInferenceDownloader {
        archive,
        fail: false,
        calls: 0,
    };
    run_install(&manifest, root.path(), &mut downloader).expect("首次安装应成功");
    run_install(&manifest, root.path(), &mut downloader).expect("复用安装应成功");
    assert_eq!(downloader.calls, 1, "已校验组件不得重复下载");
}

// 覆盖 XB-09/T-05：归档摘要不符时安装失败，已有安装不受影响。
#[test]
fn inference_pack_bad_archive_keeps_existing_install() {
    let root = tempfile::tempdir().expect("创建测试根");
    let (manifest, archive) = fixture_inference_pack();
    let mut downloader = FakeInferenceDownloader {
        archive,
        fail: false,
        calls: 0,
    };
    run_install(&manifest, root.path(), &mut downloader).expect("首次安装应成功");

    // 破坏一个已安装成员触发重装，再供给坏归档。
    let component = root.path().join("xberg-inference").join(&manifest.tag);
    fs::write(component.join("xberg.exe"), b"tampered").expect("破坏一个成员");
    let mut corrupt = FakeInferenceDownloader {
        archive: b"corrupt-archive".to_vec(),
        fail: false,
        calls: 0,
    };
    let error = run_install(&manifest, root.path(), &mut corrupt).expect_err("摘要不符必须失败");
    assert!(error.contains("校验失败"), "错误应说明校验失败：{error}");
    assert_eq!(corrupt.calls, 1);
}

// 覆盖 XB-09：成员被篡改时成员级校验必须发现（不因同名文件认定兼容）。
#[test]
fn inference_ready_detects_tampered_member() {
    let root = tempfile::tempdir().expect("创建测试根");
    let (manifest, archive) = fixture_inference_pack();
    let mut downloader = FakeInferenceDownloader {
        archive,
        fail: false,
        calls: 0,
    };
    run_install(&manifest, root.path(), &mut downloader).expect("首次安装应成功");
    let component = root.path().join("xberg-inference").join("vtest-inference");
    fs::write(component.join("xberg.exe"), b"tampered").expect("篡改成员");
    let error = inference_ready(&manifest, root.path()).expect_err("篡改必须被成员级校验发现");
    assert!(error.contains("xberg.exe"), "错误应指明成员：{error}");
}

// 覆盖 XB-09：目录 tag 与清单不一致时明确报错并指引更新，不静默使用。
#[test]
fn resolve_component_with_tag_mismatch_errors() {
    let root = tempfile::tempdir().expect("创建测试根");
    let base = root.path().join("xberg-inference");
    let installed = base.join("vinstalled");
    fs::create_dir_all(&installed).expect("预置安装目录");
    let error = resolve_component_with_tag(&base, Some("vexpected"), "未配置")
        .expect_err("版本不一致必须报错");
    assert!(
        error.contains("vexpected") && error.contains("更新"),
        "错误应指明清单要求并指引更新：{error}"
    );
    resolve_component_with_tag(&base, Some("vinstalled"), "未配置").expect("一致时应正常解析");
    resolve_component_with_tag(&base, None, "未配置").expect("无清单 tag 时唯一目录可解析");
}

// 覆盖 XB-09 自愈：多目录（如一次清理失败残留旧版本）时优先选中清单 tag
// 目录，不阻塞使用；无清单 tag 目录才报错。
#[test]
fn resolve_component_multiple_dirs_prefers_manifest_tag() {
    let root = tempfile::tempdir().expect("创建测试根");
    let base = root.path().join("xberg-inference");
    for tag in ["vnew", "vold"] {
        fs::create_dir_all(base.join(tag)).expect("预置版本目录");
    }
    let resolved =
        resolve_component_with_tag(&base, Some("vnew"), "未配置").expect("应选中清单 tag 目录");
    assert_eq!(
        resolved.file_name().and_then(|name| name.to_str()),
        Some("vnew")
    );
    let error = resolve_component_with_tag(&base, Some("vabsent"), "未配置")
        .expect_err("清单 tag 不在场必须报错");
    assert!(error.contains("vabsent"), "错误应指明缺失 tag：{error}");
}

// ── B-1/B-2：staging 清理降级与残留兜底清理 ──

// 覆盖 B-1：staging 清理失败时，已成功的关键变更不得被误报为失败（生产
// 触发：防护软件/索引器短暂持有 staging 内文件句柄；gui.rs 的保存链路还
// 会把误报转化为 convert_runtime_confirmed=false）。注入方式：staging
// 路径本身是普通文件时 remove_dir_all 确定性失败，无需平台句柄。
#[test]
fn finalize_staging_cleanup_failure_downgrades_to_warning() {
    let root = tempfile::tempdir().expect("创建测试目录");
    let staging = root.path().join(".staging-fake");
    fs::write(&staging, b"not-a-dir").expect("把 staging 预置为文件");
    let mut warnings = Vec::new();
    let mut progress = |message: String| warnings.push(message);

    let result = super::finalize_staging(Ok(()), &staging, &mut progress);

    assert!(result.is_ok(), "清理失败不得吞掉成功结果：{result:?}");
    assert_eq!(
        warnings
            .iter()
            .filter(|message| message.contains("警告：清理初始化临时目录失败"))
            .count(),
        1,
        "应恰好发出一条清理失败警告：{warnings:?}"
    );
}

// 覆盖 B-1 配套不变量：主结果失败时，清理失败不得覆盖原始错误
//（本用例为守护性断言，修复前后都应通过）。
#[test]
fn finalize_staging_keeps_original_error_over_cleanup_failure() {
    let root = tempfile::tempdir().expect("创建测试目录");
    let staging = root.path().join(".staging-fake");
    fs::write(&staging, b"not-a-dir").expect("把 staging 预置为文件");
    let mut progress = |_message: String| {};

    let result = super::finalize_staging(Err("原始失败".to_string()), &staging, &mut progress);

    assert_eq!(
        result.expect_err("原始错误必须保留"),
        "原始失败",
        "清理失败不得覆盖原始错误"
    );
}

// 覆盖 B-2：历史残留的 .staging-* 目录必须在 initialize 入口被兜底清理。
// 残留来源：readiness 提前返回、取消后清理失败、进程崩溃——修复前仅当次
// 收尾清理，入口不清理任何残留（残留单个可超 1GB）。
#[test]
fn initialize_cleans_stale_staging_residue_at_entry() {
    let guard = redirect_component_env();
    let root = guard.root.path();
    let residue = root.join(".staging-fake");
    fs::create_dir_all(residue.join("xberg-inference")).expect("预置残留目录");
    fs::write(
        residue.join("xberg-inference").join("download.zip"),
        b"residue",
    )
    .expect("预置残留文件");

    let cancel = AtomicBool::new(false);
    // 后续阶段会因尚未下载 Xberg 功能组件而失败；本用例只断言入口清理。
    let _ = super::initialize(&cancel, |_message: String| {});

    assert!(
        !residue.exists(),
        "残留 staging 目录必须在 initialize 入口被兜底清理（B-2）"
    );
}

// 覆盖 B-2：单个残留目录清理失败（如被防护软件锁定）必须跳过继续，不得
// 影响其余残留清理，也不得让初始化整体失败。注入方式：对残留内文件持有
// 无 FILE_SHARE_DELETE 的句柄，remove_dir_all 稳定失败（目录不涉及改名，
// 句柄方案可行；对照 asset_util 的目录原子替换用例）。
// 平台门禁原因：共享模式句柄为 Windows 独有注入；无门禁的入口清理用例
// 覆盖主路径（本项目按 P-07 仅支持 Windows，门禁不影响生产平台覆盖）。
#[cfg(windows)]
#[test]
fn initialize_skips_undeletable_stale_staging_and_continues() {
    let guard = redirect_component_env();
    let root = guard.root.path();
    let locked = root.join(".staging-locked");
    fs::create_dir_all(&locked).expect("预置被锁残留目录");
    fs::write(locked.join("inner.txt"), b"x").expect("预置锁定文件");
    let normal = root.join(".staging-normal");
    fs::create_dir_all(&normal).expect("预置普通残留目录");
    fs::write(normal.join("inner.txt"), b"x").expect("预置普通残留文件");
    {
        use std::os::windows::fs::OpenOptionsExt;
        let _handle = fs::OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(locked.join("inner.txt"))
            .expect("持有无共享删除句柄");

        let cancel = AtomicBool::new(false);
        let _ = super::initialize(&cancel, |_message: String| {});
    }
    assert!(!normal.exists(), "普通残留必须被清理");
    assert!(
        locked.exists(),
        "被锁残留必须跳过（清理尽力而为，不阻塞初始化）"
    );
    let remaining: Vec<String> = fs::read_dir(root)
        .expect("枚举资产根")
        .flatten()
        .filter_map(|entry| {
            entry
                .file_name()
                .into_string()
                .ok()
                .filter(|name| name.starts_with(".staging-"))
        })
        .collect();
    assert_eq!(
        remaining,
        vec![".staging-locked".to_string()],
        "除被锁残留外不得留下其他 .staging-* 条目"
    );
}

// 覆盖 T-05：已就绪环境也必须响应预置取消，不能显示初始化成功。
#[test]
fn initialize_cancelled_ready_runtime_reports_cancel() {
    let guard = redirect_component_env();
    let runtime = guard.root.path().join("runtime");
    for member in load_manifest().expect("读取清单").xberg.members {
        if !jchtools_xberg::xberg_runtime::asset_for_scenario(&member.path, "document") {
            continue;
        }
        let target = runtime.join(&member.path);
        fs::create_dir_all(target.parent().expect("成员父目录")).expect("创建成员目录");
        fs::write(target, b"replaced-engine").expect("预置成员");
    }
    super::save_runtime_dir(&runtime).expect("保存就绪运行目录");
    super::readiness().expect("前置：文档环境已就绪");
    let mut messages = Vec::new();
    let result = super::initialize(&AtomicBool::new(true), |message| messages.push(message));
    assert!(
        result.is_err(),
        "预置取消必须返回取消状态，不能报告就绪成功：{result:?}"
    );
    assert!(result.expect_err("取消结果").contains("取消"));
    assert!(
        messages.is_empty(),
        "预置取消不得发出初始化成功进度：{messages:?}"
    );
}

// 覆盖 B-2 扩展：save_runtime_dir 写入失败/崩溃残留的
// .xberg-runtime-path.txt.<uuid> 临时文件必须在 initialize 入口被兜底清扫
//——修复前入口只清 .staging-*，该残留永久滞留；.old-* 备份不在清扫范围
//（由原子替换路径管理），必须保留。
#[test]
fn initialize_cleans_stale_runtime_selection_residue_at_entry() {
    let guard = redirect_component_env();
    let root = guard.root.path();
    fs::create_dir_all(root).expect("预置资产根目录");
    let residue = root.join(".xberg-runtime-path.txt.deadbeef");
    fs::write(&residue, b"C:\\xberg\n").expect("预置运行目录残留临时文件");
    let keep_backup = root.join(".old-deadbeef");
    fs::create_dir_all(&keep_backup).expect("预置旧备份残留");

    let cancel = AtomicBool::new(false);
    // 后续阶段会因尚未下载 Xberg 功能组件而失败；本用例只断言入口清理。
    let _ = super::initialize(&cancel, |_message: String| {});

    assert!(
        !residue.exists(),
        "残留的 .xberg-runtime-path.txt.* 临时文件必须在 initialize 入口被兜底清理（B-2）"
    );
    assert!(
        keep_backup.exists(),
        ".old-* 备份不在初始化入口清扫范围（由原子替换路径管理），必须保留"
    );
}

// ── C-2（markdown 侧）：readiness 的推理包成员检查与组件目录解析同源 ──

// 覆盖 C-2（markdown 侧；XB-09 2026-10-02 修订后的存在性口径）：设置页
// 保存的组件目录下，推理组件成员检查必须基于 resolve_xberg_component
// 解析出的组件目录，而非资产根相对路径。覆盖树内全部 media 成员以桩字节
// （与清单摘要不同）在场：存在性口径下必须通过——资产根下无任何组件文件，
// 若校验仍走资产根相对路径或仍比对摘要则必失败，一次断言同时钉住两条口径；
// 删除成员后错误基于保存目录点名缺失项。
#[test]
fn readiness_inference_pack_honors_configured_component_dir() {
    let guard = redirect_component_env();
    let external = guard.root.path().join("external-component");
    let manifest = load_manifest().expect("内置清单必须可解析");
    let inference = manifest
        .xberg_inference
        .as_ref()
        .expect("内置清单已接入推理组件段");
    for member in &inference.members {
        if !jchtools_xberg::xberg_runtime::asset_for_scenario(&member.install_path, "media") {
            continue;
        }
        let target = external.join(&member.install_path);
        fs::create_dir_all(target.parent().expect("成员路径有父目录")).expect("创建覆盖树成员目录");
        fs::write(&target, b"replaced-engine").expect("预置桩成员（字节与清单不同）");
    }
    fs::write(external.join("xberg.exe"), b"engine").expect("预置引擎文件供设置链校验");
    jchtools_foundation::xberg_settings::save(&external).expect("经设置链保存组件目录");

    super::readiness_inference_pack(&manifest, guard.root.path())
        .expect("桩字节成员应通过存在性检查（替换引擎免摘要校验）");

    fs::remove_file(external.join("models").join("vad").join("silero_vad.onnx"))
        .expect("删除一个媒体模型");
    let error = super::readiness_inference_pack(&manifest, guard.root.path())
        .expect_err("缺失成员必须失败");
    assert!(
        error.contains("silero_vad.onnx") && error.contains("缺失"),
        "错误应基于保存目录点名缺失成员：{error}"
    );
}

// 覆盖 T-06（XB-09）：已下载的 Xberg 运行目录只做 document 场景成员
// 存在性检查；文件被用户替换为不同字节仍须放行，缺失时明确指认成员。
#[test]
fn validate_runtime_dir_presence_only_accepts_replaced_engine() {
    let temp = tempfile::tempdir().expect("创建测试目录");
    let root = temp.path();
    let manifest = load_manifest().expect("内置清单必须可解析");
    for member in &manifest.xberg.members {
        let target = root.join(&member.path);
        fs::create_dir_all(target.parent().expect("成员路径有父目录")).expect("创建成员目录");
        fs::write(&target, b"replaced-engine").expect("预置成员（字节与清单不同）");
    }
    super::validate_runtime_dir(root).expect("替换引擎文件后应放行（运行时不比对摘要）");

    fs::remove_file(root.join("xberg.exe")).expect("删除引擎文件");
    let error = super::validate_runtime_dir(root).expect_err("成员缺失必须报错");
    assert!(
        error.contains("xberg.exe") && error.contains("缺失"),
        "错误应指认缺失成员：{error}"
    );
}
