//! `capture_win` 内联测试模块原样搬移。

use super::{
    capture_pixels_are_unusable, gdi_replaced_ok, image_belongs_to_main, interpret_message_result,
};

// 覆盖 O-17/O-29：尺寸乘积溢出不能使无效缓冲通过校验或令截图线程 panic。
#[test]
fn bgr_dimensions_reject_overflow_without_allocating() {
    assert!(super::BgrImage::from_vec(usize::MAX, 2, Vec::new()).is_err());
    assert!(super::BgrImage::from_vec(usize::MAX / 3 + 1, 1, Vec::new()).is_err());
    assert!(super::BgrImage::from_vec(1usize << (usize::BITS - 1), 2, Vec::new()).is_err());
    assert!(super::BgrImage::from_vec(2, 1, vec![0; 5]).is_err());
    assert!(super::BgrImage::from_vec(2, 1, vec![0; 6]).is_ok());
}

// 覆盖 O-17/O-18 回归：全黑是合法截图内容，不能仅凭像素全零拒绝捕获。
#[test]
fn all_black_frame_is_not_marked_unusable() {
    assert!(!capture_pixels_are_unusable(&[0; 4 * 16 * 16]));
}

// 覆盖 O-18/O-30 回归：GetMessageW 的 -1 是截图链路故障，不能伪装成取消。
#[test]
fn failed_message_loop_is_reported_as_error() {
    assert!(interpret_message_result(-1).is_err());
    assert!(!interpret_message_result(0).unwrap());
    assert!(interpret_message_result(1).unwrap());
}

// 覆盖 S9-10：SelectObject 摘下旧对象的失败形态有空句柄与 HGDI_ERROR(-1)
// 两种，只判 is_null 会漏掉 -1——遮罩路径此前与主路径口径不一致。
#[test]
fn gdi_selection_rejects_null_and_hgdi_error() {
    assert!(gdi_replaced_ok(0x1234_usize as *mut std::ffi::c_void));
    assert!(!gdi_replaced_ok(std::ptr::null_mut::<std::ffi::c_void>()));
    assert!(
        !gdi_replaced_ok((-1_isize) as *mut std::ffi::c_void),
        "HGDI_ERROR(-1) 必须判为失败（S9-10）"
    );
}

// 覆盖 S9-11：隐藏候选窗口按完整进程路径比对——basename 撞名的另装
// 实例不隐藏；大小写与 \\?\ 前缀差异不影响判定；未登记主程序时一律
// 不按名字隐藏。
#[test]
fn main_window_hidden_only_by_full_registered_path() {
    let main = r"C:\Program Files\JchTools\JchTools.exe";
    // 大小写不敏感。
    assert!(image_belongs_to_main(
        r"c:\program files\jchtools\JchTools.exe",
        Some(main)
    ));
    // \\?\ 前缀差异不影响。
    assert!(image_belongs_to_main(
        r"\\?\C:\Program Files\JchTools\JchTools.exe",
        Some(main)
    ));
    assert!(
        !image_belongs_to_main(r"D:\tools\JchTools\JchTools.exe", Some(main)),
        "另装实例（basename 相同、完整路径不同）不得隐藏（S9-11）"
    );
    assert!(
        !image_belongs_to_main(r"C:\Program Files\JchTools\JchTools.exe", None),
        "未登记主程序路径时不按名字隐藏"
    );
    assert!(!image_belongs_to_main(
        r"C:\Windows\System32\cmd.exe",
        Some(main)
    ));
}

// 覆盖 O-18/O-29：左键松开已写入矩形，但更早投递的取消完成消息
// 随后被取出时，必须丢弃选区，不得交给 OCR。
#[test]
fn cancellation_discards_an_already_recorded_selection() {
    let rect = super::Rect {
        left: 10,
        top: 20,
        right: 30,
        bottom: 40,
    };
    assert!(super::completed_selection(Some(rect), false).is_none());
    assert!(super::completed_selection(Some(rect), true).is_some());
    assert!(super::completed_selection(None, true).is_none());
}
