/// 把 resources/app.ico 作为 Windows 资源编译进 EXE：资源管理器、任务栏和图钉快捷键都用到它。
/// rc.exe 来自 Windows SDK；找不到时只警告（运行时窗口图标仍由 Slint 的 icon 属性设置），不中断构建。
/// 仅 gui 装配调用：关闭 gui 特性时整段不编译，避免 dead_code。
#[cfg(feature = "gui")]
fn embed_icon(manifest_dir: &std::path::Path) {
    let icon = manifest_dir.join("resources/app.ico");
    if !icon.is_file() {
        return;
    }
    let out = std::path::PathBuf::from(cargo_env("OUT_DIR"));
    let rc_file = out.join("jchtools.rc");
    let res_file = out.join("jchtools.res");
    let quoted = icon.display().to_string().replace('\\', "\\\\");
    // rc.exe 对无 BOM 的 .rc 按 ANSI 代码页解析：仓库路径含 CJK 字符时 UTF-8 字节会被
    // 误解码导致资源编译失败。写成带 BOM 的 UTF-16LE，rc.exe 按 Unicode 解析任意路径。
    let rc_text = format!("1 ICON \"{quoted}\"\r\n");
    let mut rc_bytes: Vec<u8> = vec![0xFF, 0xFE];
    rc_bytes.extend(rc_text.encode_utf16().flat_map(u16::to_le_bytes));
    if let Err(error) = std::fs::write(&rc_file, rc_bytes) {
        println!("cargo:warning=无法写入 rc 资源源文件（{error}），EXE 未嵌入图标");
        return;
    }
    let Some(rc) = find_rc() else {
        println!("cargo:warning=未找到 Windows SDK 的 rc.exe，EXE 未嵌入图标（窗口/任务栏图标仍在运行时可正常显示）");
        return;
    };
    match std::process::Command::new(&rc)
        .args(["/nologo", "/fo"])
        .arg(&res_file)
        .arg(&rc_file)
        .output()
    {
        Ok(output) if output.status.success() => {
            // MSVC 链接参数无法安全携带空格（rustc 会把引号转义成字面量）；路径含空格时
            // 显式告警而不是静默产出坏参数。
            if res_file.display().to_string().contains(' ') {
                println!(
                    "cargo:warning=构建路径含空格，MSVC 链接可能失败：{}",
                    res_file.display()
                );
            }
            println!("cargo:rustc-link-arg-bin=JchTools={}", res_file.display());
        }
        Ok(output) => println!(
            "cargo:warning=rc.exe 未能编译图标资源：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(error) => println!("cargo:warning=无法运行 rc.exe（{error}），EXE 未嵌入图标"),
    }
}

#[cfg(feature = "gui")]
fn find_rc() -> Option<std::path::PathBuf> {
    // 与 process.rs 的 system_tool 防 PATH 劫持口径一致：优先从 Windows SDK 解析 rc.exe；
    // SDK 里找不到时才回退 PATH 并明确告警（PATH 中的同名程序可能是伪造的，
    // 会在构建期执行任意代码并污染产物）。
    let kits = std::env::var_os("ProgramFiles(x86)")
        .map(std::path::PathBuf::from)
        .map(|base| base.join("Windows Kits/10/bin"));
    if let Some(kits) = kits {
        if let Ok(entries) = std::fs::read_dir(&kits) {
            let mut versions: Vec<std::path::PathBuf> = entries
                .filter_map(std::result::Result::ok)
                .map(|entry| entry.path())
                .collect();
            // 按数字元组排序（10.0.22621 > 10.0.19041），不能按字典序（否则 10.0.9 会排在 10.0.10 前面）。
            versions.sort_by_key(|path| {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                name.split('.')
                    .map(|part| part.parse::<u32>().unwrap_or(0))
                    .collect::<Vec<_>>()
            });
            versions.reverse();
            let found = versions
                .into_iter()
                .map(|version| version.join("x64/rc.exe"))
                .find(|candidate| candidate.is_file());
            if found.is_some() {
                return found;
            }
        }
    }
    let names = ["rc.exe"];
    let from_path = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
            .find(|candidate| candidate.is_file())
    });
    if from_path.is_some() {
        println!("cargo:warning=Windows SDK 中未找到 rc.exe，回退使用 PATH 中的 rc.exe（无法排除被伪造的可能，图标嵌入产物可信度降低）");
    }
    from_path
}

/// 读取 cargo 注入的环境变量（OUT_DIR / CARGO_MANIFEST_DIR 等）：
/// 这些变量只在构建脚本运行期由 cargo 保证存在，缺失即构建环境损坏，按不可达处理。
#[cfg(feature = "gui")]
fn cargo_env(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|error| panic!("cargo 构建环境缺少 {key}：{error}"))
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    #[cfg(feature = "gui")]
    let manifest_dir = std::path::PathBuf::from(cargo_env("CARGO_MANIFEST_DIR"));
    #[cfg(feature = "gui")]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=resources/windows.manifest");
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            // manifest 路径只被 gui 装配使用；绑定放块内，关闭 gui 特性时不产生未用变量。
            #[cfg(feature = "gui")]
            {
                let manifest = manifest_dir.join("resources/windows.manifest");
                println!("cargo:rustc-link-arg-bin=JchTools=/MANIFEST:EMBED");
                if manifest.display().to_string().contains(' ') {
                    println!(
                        "cargo:warning=构建路径含空格，MSVC 链接可能失败：{}",
                        manifest.display()
                    );
                }
                println!(
                    "cargo:rustc-link-arg-bin=JchTools=/MANIFESTINPUT:{}",
                    manifest.display()
                );
                println!("cargo:rerun-if-changed=resources/app.ico");
                embed_icon(&manifest_dir);
            }
        }
    }
}
