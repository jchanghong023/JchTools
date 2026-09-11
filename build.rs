/// 把 resources/app.ico 作为 Windows 资源编译进 EXE：资源管理器、任务栏和图钉快捷键都用到它。
/// rc.exe 来自 Windows SDK；找不到时只警告（运行时窗口图标仍由 Slint 的 icon 属性设置），不中断构建。
fn embed_icon(manifest_dir: &std::path::Path) {
    let icon = manifest_dir.join("resources/app.ico");
    if !icon.is_file() { return; }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let rc_file = out.join("jchtools.rc");
    let res_file = out.join("jchtools.res");
    let quoted = icon.display().to_string().replace('\\', "\\\\");
    if std::fs::write(&rc_file, format!("1 ICON \"{quoted}\"\n")).is_err() { return; }
    let Some(rc) = find_rc() else {
        println!("cargo:warning=未找到 Windows SDK 的 rc.exe，EXE 未嵌入图标（窗口/任务栏图标仍在运行时可正常显示）");
        return;
    };
    match std::process::Command::new(&rc).args(["/nologo", "/fo"]).arg(&res_file).arg(&rc_file).output() {
        Ok(output) if output.status.success() => {
            println!("cargo:rustc-link-arg-bin=JchTools={}", res_file.display());
        }
        Ok(output) => println!("cargo:warning=rc.exe 未能编译图标资源：{}", String::from_utf8_lossy(&output.stderr).trim()),
        Err(error) => println!("cargo:warning=无法运行 rc.exe（{error}），EXE 未嵌入图标"),
    }
}

fn find_rc() -> Option<std::path::PathBuf> {
    let names = ["rc.exe"];
    let from_path = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).flat_map(|dir| names.iter().map(move |name| dir.join(name))).find(|candidate| candidate.is_file())
    });
    if from_path.is_some() { return from_path; }
    let kits = std::env::var_os("ProgramFiles(x86)").map(std::path::PathBuf::from).map(|base| base.join("Windows Kits/10/bin"))?;
    let mut versions: Vec<std::path::PathBuf> = std::fs::read_dir(&kits).ok()?.filter_map(|entry| entry.ok()).map(|entry| entry.path()).collect();
    versions.sort();
    versions.reverse();
    versions.into_iter().map(|version| version.join("x64/rc.exe")).find(|candidate| candidate.is_file())
}

/// 把已校验的 7-Zip 引擎（7z.exe + 7z.dll + manifest.json）用 zlib 压缩后编进 EXE。
/// 运行期由 src/engine_bundle.rs 释放到用户数据目录并逐文件校验 sha256；
/// 发布包仍保留 licenses/ 与上游源码，满足 LGPL 的替换与再分发要求。
/// 引擎文件不在时（未运行 fetch-7zip.ps1）只警告，不影响构建。
fn embed_engine(manifest_dir: &std::path::Path) {
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let source = manifest_dir.join("resources/7zip");
    let generated = out_dir.join("embedded_engine.rs");
    let mut files: Vec<(String, i64)> = Vec::new();
    let mut manifest_text = String::new();
    let mut id = String::new();

    let exe_name = if cfg!(windows) { "7z.exe" } else { "7zz" };
    let manifest_path = source.join("manifest.json");
    let names: Vec<String> = if cfg!(windows) { vec![exe_name.into(), "7z.dll".into()] } else { vec![exe_name.into()] };
    let ready = manifest_path.is_file() && names.iter().all(|name| source.join(name).is_file());
    if ready {
        if let Ok(text) = std::fs::read_to_string(&manifest_path) {
            manifest_text = text;
            let version = serde_json::from_str::<serde_json::Value>(&manifest_text).ok()
                .and_then(|value| value["version"].as_str().map(str::to_owned))
                .unwrap_or_else(|| "unknown".into());
            let tag = serde_json::from_str::<serde_json::Value>(&manifest_text).ok()
                .and_then(|value| value["files"].as_array().and_then(|list| list.first().and_then(|first| first["sha256"].as_str().map(|s| s[..8.min(s.len())].to_owned()))))
                .unwrap_or_default();
            id = if tag.is_empty() { version } else { format!("{version}-{tag}") };
            for name in &names {
                match std::fs::read(source.join(name)).map(|bytes| compress(&bytes)) {
                    Ok(compressed) => {
                        let path = out_dir.join(format!("{name}.zlib"));
                        let length = compressed.len() as i64;
                        if std::fs::write(&path, compressed).is_ok() {
                            files.push((name.clone(), length));
                        }
                    }
                    Err(error) => println!("cargo:warning=读取引擎文件 {name} 失败：{error}"),
                }
            }
        }
    } else {
        println!("cargo:warning=resources/7zip 里没有完整引擎，本次构建不内嵌 7-Zip（运行期只查找随包目录；需要内嵌请先运行 scripts/fetch-7zip.ps1）");
    }
    for name in &names {
        println!("cargo:rerun-if-changed=resources/7zip/{name}");
    }
    println!("cargo:rerun-if-changed=resources/7zip/manifest.json");

    let mut code = String::from("// 由 build.rs 生成：内嵌 7-Zip 引擎（zlib 压缩）\n");
    code.push_str(&format!("pub const ID: &str = {:?};\n", id));
    code.push_str(&format!("pub const MANIFEST: &str = {:?};\n", manifest_text));
    code.push_str("pub const FILES: &[(&str, &[u8])] = &[\n");
    for (name, _) in &files {
        code.push_str(&format!("    ({:?}, include_bytes!(concat!(env!(\"OUT_DIR\"), \"/{}.zlib\"))),\n", name, name));
    }
    code.push_str("];\n");
    std::fs::write(&generated, code).expect("write embedded_engine.rs");
}

fn compress(bytes: &[u8]) -> Vec<u8> {
    use flate2::{write::ZlibEncoder, Compression};
    use std::io::Write;
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(bytes).expect("compress engine file");
    encoder.finish().expect("finish engine compression")
}

fn main() {
    #[cfg(feature = "gui")]
    {
        println!("cargo:rerun-if-changed=ui/app.slint");
        println!("cargo:rerun-if-changed=resources/app-icon.png");
        let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
        slint_build::compile_with_config("ui/app.slint", config)
            .expect("Slint UI compilation failed");
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        let manifest = manifest_dir.join("resources/windows.manifest");
        embed_engine(&manifest_dir);
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            #[cfg(feature = "gui")] {
                println!("cargo:rustc-link-arg-bin=JchTools=/MANIFEST:EMBED");
                println!("cargo:rustc-link-arg-bin=JchTools=/MANIFESTINPUT:{}", manifest.display());
                println!("cargo:rerun-if-changed=resources/app.ico");
                embed_icon(&manifest_dir);
            }
            println!("cargo:rustc-link-arg-bin=jchtools-cli=/MANIFEST:EMBED");
            println!("cargo:rustc-link-arg-bin=jchtools-cli=/MANIFESTINPUT:{}", manifest.display());
        }
    }
}
