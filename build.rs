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

/// 把已校验的 7-Zip 引擎（7z.exe + 7z.dll + manifest.json）用 zlib 压缩后编进 EXE。
/// 运行期由 src/engine_bundle.rs 释放到用户数据目录并逐文件校验 sha256；
/// 发布包仍保留 licenses/ 与上游源码，满足 LGPL 的替换与再分发要求。
/// 引擎文件不在时（未运行 fetch-7zip.ps1）只警告，不影响构建；
/// 引擎存在但 sha256 与 manifest.json 不一致时 panic，拒绝嵌入被篡改的引擎。
/// 内嵌结果写入 OUT_DIR/engine_embed_status.txt，供打包脚本 fail-closed 校验：
/// "ok" 表示全部文件成功内嵌，"incomplete" 表示本次构建按无内嵌处理。
/// 读取 cargo 注入的环境变量（OUT_DIR / CARGO_MANIFEST_DIR 等）：
/// 这些变量只在构建脚本运行期由 cargo 保证存在，缺失即构建环境损坏，按不可达处理。
fn cargo_env(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|error| panic!("cargo 构建环境缺少 {key}：{error}"))
}
fn embed_engine(manifest_dir: &std::path::Path) {
    let out_dir = std::path::PathBuf::from(cargo_env("OUT_DIR"));
    let source = manifest_dir.join("resources/7zip");
    let generated = out_dir.join("embedded_engine.rs");
    let status_file = out_dir.join("engine_embed_status.txt");
    let mut files: Vec<(String, u64)> = Vec::new();
    let mut manifest_text = String::new();
    let mut id = String::new();
    let mut embed_ok = false;

    // 按“目标平台”而不是构建宿主选择引擎文件名，交叉编译时才不会内嵌错误引擎。
    let target_windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let exe_name = if target_windows { "7z.exe" } else { "7zz" };
    let manifest_path = source.join("manifest.json");
    let names: Vec<String> = if target_windows {
        vec![exe_name.into(), "7z.dll".into()]
    } else {
        vec![exe_name.into()]
    };
    let ready = manifest_path.is_file() && names.iter().all(|name| source.join(name).is_file());
    if ready {
        match std::fs::read_to_string(&manifest_path) {
            Ok(text) => {
                manifest_text = text;
                let parsed = match serde_json::from_str::<serde_json::Value>(&manifest_text) {
                    Ok(value) => value,
                    Err(error) => {
                        panic!("manifest.json 应是 fetch-7zip.ps1 生成的合法 JSON：{error}")
                    }
                };
                let version = parsed["version"]
                    .as_str()
                    .map_or_else(|| "unknown".into(), str::to_owned);
                let tag = parsed["files"]
                    .as_array()
                    .and_then(|list| {
                        list.first().and_then(|first| {
                            first["sha256"]
                                .as_str()
                                .map(|s| s[..8.min(s.len())].to_owned())
                        })
                    })
                    .unwrap_or_default();
                id = if tag.is_empty() {
                    version
                } else {
                    format!("{version}-{tag}")
                };
                // 嵌入前逐文件校验 sha256 与清单一致：防止把被篡改或与清单不匹配的引擎编进 EXE。
                for name in &names {
                    let bytes = match std::fs::read(source.join(name)) {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            println!("cargo:warning=读取引擎文件 {name} 失败：{error}");
                            break; // 进入下方 files.len() 不完整分支处理
                        }
                    };
                    let expected = parsed["files"].as_array().and_then(|list| {
                        list.iter().find_map(|entry| {
                            let entry_name = entry["name"].as_str()?;
                            if entry_name.eq_ignore_ascii_case(name) {
                                entry["sha256"].as_str().map(str::to_ascii_lowercase)
                            } else {
                                None
                            }
                        })
                    });
                    let Some(expected) = expected else {
                        panic!("manifest.json 缺少引擎文件 {name} 的条目，拒绝嵌入未校验的引擎；清单必须为每个待内嵌文件提供 sha256（scripts/fetch-7zip.ps1 只生成 7z.exe/7z.dll 的条目，手动放置的其他引擎文件无法内嵌）");
                    };
                    let actual = sha256_hex(&bytes);
                    assert!(actual == expected, "引擎文件 {name} 的 sha256（{actual}）与 manifest.json 期望值（{expected}）不一致，拒绝嵌入；请重新运行 scripts/fetch-7zip.ps1 获取官方完整引擎");
                    let compressed = compress(&bytes);
                    let path = out_dir.join(format!("{name}.zlib"));
                    let length = u64::try_from(compressed.len()).unwrap_or(u64::MAX);
                    if std::fs::write(&path, compressed).is_ok() {
                        files.push((name.clone(), length));
                    } else {
                        println!("cargo:warning=写入 {name} 的压缩副本失败，本次构建不内嵌该文件");
                    }
                }
                if files.len() < names.len() {
                    // 部分内嵌会让运行期 embedded_available()=false，必须让构建日志能看出原因
                    //（常见于杀毒软件短暂占用引擎文件），否则会被误判成“没有运行 fetch-7zip.ps1”。
                    // 同时清空 MANIFEST，让运行期 embedded_available() 确实返回 false，
                    // 与“本次构建按无内嵌引擎处理”的口径一致，而不是运行到一半报“缺少清单文件”。
                    println!("cargo:warning=引擎文件不完整：期望 {} 个，实际内嵌 {} 个；本次构建按无内嵌引擎处理", names.len(), files.len());
                    manifest_text = String::new();
                    files.clear();
                } else if !manifest_text.is_empty() && !files.is_empty() {
                    embed_ok = true;
                }
            }
            // 读失败（锁/权限等）按无内嵌处理并警告；哈希不匹配才会 panic（见上方循环）。
            Err(error) => {
                println!("cargo:warning=读取 manifest.json 失败（{error}），本次构建不内嵌 7-Zip");
            }
        }
    } else {
        println!("cargo:warning=resources/7zip 里没有完整引擎，本次构建不内嵌 7-Zip（运行期只查找随包目录；需要内嵌请先运行 scripts/fetch-7zip.ps1）");
    }
    // 写入打包可检出的内嵌状态标记（fail-closed）：package-windows.ps1 在构建后要求 "ok"。
    // 先删再写：避免上次成功留下的旧 "ok" 在本次写失败时被误读。
    // 写失败直接 panic：与哈希不一致同级，禁止「构建成功但状态不可信」。
    let status = if embed_ok { "ok" } else { "incomplete" };
    let _ = std::fs::remove_file(&status_file);
    if let Err(error) = std::fs::write(&status_file, format!("{status}\n")) {
        panic!("写入 engine_embed_status.txt 失败（{error}），拒绝继续构建以免打包误用陈旧状态");
    }
    for name in &names {
        println!("cargo:rerun-if-changed=resources/7zip/{name}");
    }
    println!("cargo:rerun-if-changed=resources/7zip/manifest.json");

    use std::fmt::Write as _;
    let mut code = String::from("// 由 build.rs 生成：内嵌 7-Zip 引擎（zlib 压缩）\n");
    // pub(crate)：该文件被 include! 进私有 mod embedded，裸 pub 会触发 unreachable_pub 门禁。
    let _ = writeln!(code, "pub(crate) const ID: &str = {id:?};");
    let _ = writeln!(code, "pub(crate) const MANIFEST: &str = {manifest_text:?};");
    code.push_str("pub(crate) const FILES: &[(&str, &[u8])] = &[\n");
    for (name, _) in &files {
        let _ = writeln!(
            code,
            "    ({name:?}, include_bytes!(concat!(env!(\"OUT_DIR\"), \"/{name}.zlib\"))),"
        );
    }
    code.push_str("];\n");
    if let Err(error) = std::fs::write(&generated, code) {
        panic!("写入 embedded_engine.rs 失败：{error}");
    }
}

fn compress(bytes: &[u8]) -> Vec<u8> {
    use flate2::{write::ZlibEncoder, Compression};
    use std::io::Write;
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    if let Err(error) = encoder.write_all(bytes) {
        panic!("压缩引擎文件失败：{error}");
    }
    encoder
        .finish()
        .unwrap_or_else(|error| panic!("结束引擎压缩失败：{error}"))
}

/// 构建脚本专用的 SHA-256（小写十六进制）。
/// build-dependencies 里没有 sha2，这里内联实现，避免为构建脚本引入额外依赖。
#[allow(clippy::many_single_char_names)] // SHA-2 标准记号（a..h、w、K）
fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    let mut message = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in message.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[4 * i],
                chunk[4 * i + 1],
                chunk[4 * i + 2],
                chunk[4 * i + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    use std::fmt::Write as _;
    let mut digest = String::with_capacity(h.len() * 8);
    for word in &h {
        let _ = write!(digest, "{word:08x}");
    }
    digest
}

fn main() {
    #[cfg(feature = "gui")]
    {
        println!("cargo:rerun-if-changed=ui/app.slint");
        println!("cargo:rerun-if-changed=resources/app-icon.png");
        let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
        if let Err(error) = slint_build::compile_with_config("ui/app.slint", config) {
            panic!("Slint UI 编译失败：{error}");
        }
    }
    let manifest_dir = std::path::PathBuf::from(cargo_env("CARGO_MANIFEST_DIR"));
    // src/engine_bundle.rs 无条件 include! 这个生成文件，所以非 Windows 目标
    // 也必须生成；没有引擎时内容为空，只影响内嵌释放能力（平台范围按 P-07 仅 Windows）。
    embed_engine(&manifest_dir);
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
