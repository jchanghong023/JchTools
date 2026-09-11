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
