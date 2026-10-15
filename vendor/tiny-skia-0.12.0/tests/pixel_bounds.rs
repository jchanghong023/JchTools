use tiny_skia::{Pixmap, PremultipliedColorU8};

#[test]
fn owned_pixel_bounds_do_not_wrap_to_next_row() {
    let pixmap = Pixmap::new(2, 2).unwrap();
    assert_eq!(pixmap.pixel(0, 0), Some(PremultipliedColorU8::TRANSPARENT));
    assert_eq!(pixmap.pixel(1, 1), Some(PremultipliedColorU8::TRANSPARENT));
    for (x, y) in [(2, 0), (3, 0), (2, 1), (0, 2), (u32::MAX, 0), (0, u32::MAX)] {
        assert_eq!(
            pixmap.pixel(x, y),
            None,
            "pixel({x}, {y}) must be out of bounds"
        );
    }
}

#[test]
fn borrowed_pixel_bounds_do_not_wrap_to_next_row() {
    let pixmap = Pixmap::new(2, 2).unwrap();
    let pixmap = pixmap.as_ref();
    assert_eq!(pixmap.pixel(0, 0), Some(PremultipliedColorU8::TRANSPARENT));
    assert_eq!(pixmap.pixel(1, 1), Some(PremultipliedColorU8::TRANSPARENT));
    for (x, y) in [(2, 0), (3, 0), (2, 1), (0, 2), (u32::MAX, 0), (0, u32::MAX)] {
        assert_eq!(
            pixmap.pixel(x, y),
            None,
            "pixel({x}, {y}) must be out of bounds"
        );
    }
}
