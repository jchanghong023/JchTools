use imagesize::ImageSize;
use std::cmp::Ordering;
use std::collections::BTreeSet;

#[test]
fn equal_area_sizes_remain_distinct_in_btree_set() {
    let portrait = ImageSize {
        width: 1,
        height: 2,
    };
    let landscape = ImageSize {
        width: 2,
        height: 1,
    };
    let mut sizes = BTreeSet::new();
    sizes.insert(portrait);
    sizes.insert(landscape);

    assert_eq!(sizes.len(), 2, "不同宽高不能因面积相同而被集合合并");
    assert_eq!(portrait.cmp(&landscape), Ordering::Less);
    assert_eq!(landscape.cmp(&portrait), Ordering::Greater);
    assert_eq!(portrait.partial_cmp(&landscape), Some(Ordering::Less));
    assert_eq!(portrait.cmp(&portrait), Ordering::Equal);
}

#[test]
fn area_remains_the_primary_sort_key() {
    let smaller_area = ImageSize {
        width: 100,
        height: 1,
    };
    let larger_area = ImageSize {
        width: 2,
        height: 100,
    };

    assert!(smaller_area < larger_area);
}

#[test]
fn probed_equal_area_png_dimensions_remain_distinct() {
    fn png_dimensions(width: u32, height: u32) -> ImageSize {
        let mut header = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
        header.extend_from_slice(&width.to_be_bytes());
        header.extend_from_slice(&height.to_be_bytes());
        imagesize::blob_size(&header).expect("有效的 PNG 尺寸头应能探测")
    }

    let portrait = png_dimensions(1, 2);
    let landscape = png_dimensions(2, 1);
    assert_eq!(
        portrait,
        ImageSize {
            width: 1,
            height: 2
        }
    );
    assert_eq!(
        landscape,
        ImageSize {
            width: 2,
            height: 1
        }
    );
    let mut sizes = BTreeSet::new();
    sizes.insert(portrait);
    sizes.insert(landscape);
    assert_eq!(sizes.len(), 2);
}
