#[cfg(feature = "gui")]
fn main() {
    let manifest_dir = std::path::PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR")
            .unwrap_or_else(|error| panic!("cargo 构建环境缺少 CARGO_MANIFEST_DIR：{error}")),
    );
    let repository = manifest_dir
        .ancestors()
        .find(|path| {
            path.join("ui/app.slint").is_file() && path.join("resources/app-icon.png").is_file()
        })
        .unwrap_or_else(|| panic!("Slint UI 编译需要仓库根目录"));
    let source = repository.join("ui/app.slint");
    println!("cargo:rerun-if-changed={}", source.display());
    println!(
        "cargo:rerun-if-changed={}",
        repository.join("resources/app-icon.png").display()
    );
    let mut config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    if cfg!(feature = "mcp-debug-info") {
        // MCP 调试构建注入元素级调试信息：运行期元素树查询与交互依赖这些数据。
        config = config.with_debug_info(true);
    }
    if let Err(error) = slint_build::compile_with_config(source, config) {
        panic!("Slint UI 编译失败：{error}");
    }
}

#[cfg(not(feature = "gui"))]
fn main() {
    // Opting out of GUI must not run code generation or require Slint dependencies.
    println!("cargo:rerun-if-changed=build.rs");
}
