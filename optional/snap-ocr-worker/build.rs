// 截图 OCR worker 的 Slint 编译（结果窗，O-21）。
// 字体由 Rust include_bytes! 编入服务程序；缺少源码资产即编译失败（O-05/O-09）。
fn main() {
    // 显式夹具依赖不能取代原有包内源码、Slint 及资产的构建失效范围。
    println!("cargo:rerun-if-changed=.");
    std::thread::Builder::new()
        .name("slint-compiler".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| slint_build::compile("ui/result.slint"))
        .unwrap_or_else(|error| panic!("无法启动 Slint 编译线程：{error}"))
        .join()
        .unwrap_or_else(|_| panic!("Slint 编译线程异常退出"))
        .unwrap_or_else(|error| panic!("result.slint 编译失败：{error}"));

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var_os("CARGO_FEATURE_TEST_HOOKS").is_some()
    {
        println!("cargo:rerun-if-env-changed=RUSTC");
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| panic!("Cargo 未提供 RUSTC"));
        let target =
            std::env::var("TARGET").unwrap_or_else(|error| panic!("Cargo 未提供 TARGET：{error}"));
        let output_dir = std::path::PathBuf::from(
            std::env::var_os("OUT_DIR").unwrap_or_else(|| panic!("Cargo 未提供 OUT_DIR")),
        );
        for (source, executable, variable) in [
            (
                "tests/fixtures/cancel_boundary.rs",
                "cancel_boundary.exe",
                "JCHTOOLS_TEST_CANCEL_ENGINE",
            ),
            (
                "../../tests/fixtures/shared_xberg.rs",
                "shared_xberg.exe",
                "JCHTOOLS_TEST_IDLE_ENGINE",
            ),
        ] {
            println!("cargo:rerun-if-changed={source}");
            let executable = output_dir.join(executable);
            // 合成资产含同目录假 VCRUNTIME140.dll；静态 CRT 避免加载它。
            let compiled = std::process::Command::new(&rustc)
                .arg("--edition=2021")
                .args(["-C", "target-feature=+crt-static"])
                .arg("--target")
                .arg(&target)
                .arg(source)
                .arg("-o")
                .arg(&executable)
                .output()
                .unwrap_or_else(|error| panic!("无法编译测试引擎夹具 {source}：{error}"));
            assert!(
                compiled.status.success(),
                "测试引擎夹具 {source} 编译失败：{}",
                String::from_utf8_lossy(&compiled.stderr)
            );
            println!("cargo:rustc-env={variable}={}", executable.display());
        }
    }
}
