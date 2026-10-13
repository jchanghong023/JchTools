use crate::{ule::UleError, ZeroSlice, ZeroVec};

#[test]
fn const_constructor_results_are_const_evaluable() {
    const BYTES: Result<&ZeroSlice<u8>, UleError> = ZeroSlice::<u8>::try_from_bytes(&[0, 127, 255]);
    const BOOLS: Result<&ZeroSlice<bool>, UleError> = ZeroSlice::<bool>::try_from_bytes(&[0, 1]);
    const INVALID_BOOL: Result<&ZeroSlice<bool>, UleError> =
        ZeroSlice::<bool>::try_from_bytes(&[0, 2]);
    const UNITS: Result<&ZeroSlice<()>, UleError> = ZeroSlice::<()>::try_from_bytes(&[]);
    const INVALID_UNITS: Result<&ZeroSlice<()>, UleError> = ZeroSlice::<()>::try_from_bytes(&[0]);
    const WORDS: Result<&ZeroSlice<u16>, UleError> =
        ZeroSlice::<u16>::try_from_bytes(&[0x34, 0x12, 0x78, 0x56]);
    const INVALID_WORDS: Result<&ZeroSlice<u16>, UleError> =
        ZeroSlice::<u16>::try_from_bytes(&[0x34, 0x12, 0x78]);
    // SAFETY: 空字节已满足单位类型的有效输入前提。
    const EMPTY_SLICE: &ZeroSlice<()> = unsafe { ZeroSlice::<()>::from_bytes_unchecked(&[]) };
    const EMPTY_VEC: ZeroVec<'static, ()> = unsafe { ZeroVec::<()>::from_bytes_unchecked(&[]) };

    assert_eq!(BYTES.expect("const u8 必须有效").as_bytes(), &[0, 127, 255]);
    assert_eq!(BOOLS.expect("const bool 必须有效").get(1), Some(true));
    assert!(INVALID_BOOL.is_err());
    assert!(UNITS.expect("const 空单位切片必须有效").is_empty());
    assert!(INVALID_UNITS.is_err());
    let words = WORDS.expect("const 完整多字节必须有效");
    assert_eq!(words.get(0), Some(0x1234));
    assert_eq!(words.get(1), Some(0x5678));
    assert!(INVALID_WORDS.is_err());
    assert!(EMPTY_SLICE.is_empty());
    assert!(EMPTY_VEC.is_empty());
}
