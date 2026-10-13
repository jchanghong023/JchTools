/// 读取 Cargo 保证提供的构建环境；缺失意味着构建环境损坏。
fn cargo_env(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|error| panic!("cargo 构建环境缺少 {key}：{error}"))
}

/// 默认嵌入仓库清单；打包脚本先构建可选 worker，随后生成回填
/// size/SHA-256 的清单并通过构建环境覆盖。绝不把 worker 本体放进主程序。
fn embed_snap_ocr_manifest(repository_root: &std::path::Path) {
    let source = std::env::var_os("JCHTOOLS_SNAP_OCR_MANIFEST").map_or_else(
        || repository_root.join("resources/snap-ocr-assets.json"),
        std::path::PathBuf::from,
    );
    println!("cargo:rerun-if-env-changed=JCHTOOLS_SNAP_OCR_MANIFEST");
    println!("cargo:rerun-if-changed={}", source.display());
    let output = std::path::PathBuf::from(cargo_env("OUT_DIR")).join("snap-ocr-assets.json");
    std::fs::copy(&source, &output)
        .unwrap_or_else(|error| panic!("无法将截图 OCR 资产清单嵌入主程序：{error}"));
}

fn main() {
    let manifest_dir = std::path::PathBuf::from(cargo_env("CARGO_MANIFEST_DIR"));
    let repository_root = manifest_dir
        .ancestors()
        .find(|path| path.join("resources/snap-ocr-assets.json").is_file())
        .unwrap_or_else(|| panic!("无法定位仓库截图 OCR 资产清单"));
    embed_snap_ocr_manifest(repository_root);
}
