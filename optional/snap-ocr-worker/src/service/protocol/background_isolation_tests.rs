//! `protocol` 隔离端点回归（XB-22）：测试与 GUI 使用同一隔离控制端点，不接触用户服务。

use super::*;
// 覆盖 XB-22：真实后台测试与 GUI 使用同一隔离控制端点，不接触用户服务。
#[test]
fn isolated_background_pipe_matches_gui_endpoint() {
    let root = tempfile::tempdir().unwrap();
    let old = std::env::var_os("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT");
    std::env::set_var("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT", root.path());
    let mut identity = b"test:".to_vec();
    identity.extend_from_slice(root.path().as_os_str().as_encoded_bytes());
    let digest = hex::encode(Sha256::digest(&identity));
    let actual = pipe_name().expect("隔离测试端点应可生成");
    if let Some(value) = old {
        std::env::set_var("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT", value);
    } else {
        std::env::remove_var("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT");
    }
    assert_eq!(
        actual,
        format!(r"\\.\pipe\jchtools-snap-ocr-test-{}", &digest[..16])
    );
}
