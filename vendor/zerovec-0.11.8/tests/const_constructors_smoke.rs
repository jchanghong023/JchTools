extern crate zerovec;

use zerovec::{ule::ULE, ZeroSlice, ZeroVec};

#[test]
fn constructor_u8_accepts_all_bytes() {
    for bytes in [&[][..], &[0][..], &[1, 127, 255][..]] {
        let slice = ZeroSlice::<u8>::try_from_bytes(bytes).expect("u8 字节必须全部有效");
        assert_eq!(slice.as_bytes(), bytes);
        assert_eq!(slice.len(), bytes.len());
    }
}

#[test]
fn constructor_bool_validates_every_byte() {
    for bytes in [&[][..], &[0][..], &[1][..], &[0, 1, 1, 0][..]] {
        let slice = ZeroSlice::<bool>::try_from_bytes(bytes).expect("0 和 1 必须是有效布尔值");
        assert_eq!(slice.as_bytes(), bytes);
        for (index, byte) in bytes.iter().enumerate() {
            assert_eq!(slice.get(index), Some(*byte == 1));
        }
    }
    for bytes in [
        &[2][..],
        &[255][..],
        &[2, 0, 1][..],
        &[0, 2, 1][..],
        &[0, 1, 2][..],
    ] {
        assert!(ZeroSlice::<bool>::try_from_bytes(bytes).is_err());
    }
}

#[test]
fn constructor_unit_accepts_only_empty_bytes() {
    let slice = ZeroSlice::<()>::try_from_bytes(&[]).expect("空字节应产生空单位类型切片");
    assert!(slice.is_empty());
    assert!(ZeroSlice::<()>::try_from_bytes(&[0]).is_err());
}

#[test]
fn constructor_multibyte_preserves_length_validation() {
    let bytes = [0x34, 0x12, 0x78, 0x56];
    let slice = ZeroSlice::<u16>::try_from_bytes(&bytes).expect("完整的双字节元素必须有效");
    assert_eq!(slice.get(0), Some(0x1234));
    assert_eq!(slice.get(1), Some(0x5678));
    assert!(ZeroSlice::<u16>::try_from_bytes(&bytes[..3]).is_err());
}

#[test]
fn unit_parse_chain_accepts_only_empty_bytes() {
    assert!(<() as ULE>::parse_bytes_to_slice(&[])
        .expect("空单位 ULE 必须有效")
        .is_empty());
    assert!(ZeroSlice::<()>::parse_bytes(&[])
        .expect("空单位切片必须有效")
        .is_empty());
    assert!(ZeroVec::<()>::parse_bytes(&[])
        .expect("空单位向量必须有效")
        .is_empty());
    assert!(<() as ULE>::parse_bytes_to_slice(&[0]).is_err());
    assert!(ZeroSlice::<()>::parse_bytes(&[0]).is_err());
    assert!(ZeroVec::<()>::parse_bytes(&[0]).is_err());
    // SAFETY: 空字节已满足单位类型的有效输入前提。
    unsafe {
        assert!(<() as ULE>::slice_from_bytes_unchecked(&[]).is_empty());
        assert!(ZeroSlice::<()>::from_bytes_unchecked(&[]).is_empty());
        assert!(ZeroVec::<()>::from_bytes_unchecked(&[]).is_empty());
    }
}

#[test]
fn checked_parse_preserves_boolean_and_multibyte_validation() {
    assert_eq!(
        ZeroSlice::<bool>::parse_bytes(&[0, 1])
            .expect("合法 bool")
            .get(1),
        Some(true)
    );
    assert!(ZeroSlice::<bool>::parse_bytes(&[0, 2]).is_err());
    assert!(ZeroVec::<bool>::parse_bytes(&[0, 2]).is_err());
    assert!(ZeroSlice::<u16>::parse_bytes(&[1]).is_err());
    assert!(ZeroVec::<u16>::parse_bytes(&[1]).is_err());
    assert_eq!(
        ZeroVec::<u8>::parse_bytes(&[0, 255])
            .expect("合法 u8")
            .len(),
        2
    );
}

#[test]
fn zero_sized_raw_bytes_follow_ule_contract() {
    use zerovec::ule::RawBytesULE;

    assert!(RawBytesULE::<0>::validate_bytes(&[]).is_ok());
    assert!(RawBytesULE::<0>::parse_bytes_to_slice(&[])
        .expect("零尺寸 ULE 空字节必须有效")
        .is_empty());
    assert!(RawBytesULE::<0>::validate_bytes(&[0]).is_err());
    assert!(RawBytesULE::<0>::parse_bytes_to_slice(&[0]).is_err());
}

#[test]
fn zero_sized_raw_bytes_mutable_conversion_accepts_empty() {
    use zerovec::ule::RawBytesULE;
    assert!(RawBytesULE::<0>::from_bytes_unchecked_mut(&mut []).is_empty());
}

#[test]
fn unit_arrays_follow_zero_sized_ule_contract() {
    assert!(<[(); 1] as ULE>::validate_bytes(&[]).is_ok());
    assert!(<[(); 1] as ULE>::parse_bytes_to_slice(&[])
        .expect("单位类型数组的空字节必须有效")
        .is_empty());
    assert!(<[(); 1] as ULE>::validate_bytes(&[0]).is_err());
    assert!(ZeroSlice::<[(); 1]>::parse_bytes(&[])
        .expect("零尺寸数组的空字节必须有效")
        .is_empty());
}

#[test]
fn raw_bytes_mutable_conversion_preserves_nonzero_stride() {
    use zerovec::ule::RawBytesULE;
    // 非零步长的转换与验证保持不变。
    let mut bytes = [1, 2, 3, 4];
    let values = RawBytesULE::<2>::from_bytes_unchecked_mut(&mut bytes);
    assert_eq!(values.len(), 2);
    values[1] = RawBytesULE([5, 6]);
    assert_eq!(bytes, [1, 2, 5, 6]);
}
