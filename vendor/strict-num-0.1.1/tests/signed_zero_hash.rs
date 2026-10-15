//! 回归：有限浮点的等值键必须具有相同哈希，保持真实 HashMap 查找语义。
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::{BuildHasherDefault, Hash, Hasher};
use strict_num::{FiniteF32, FiniteF64, NormalizedF32, NormalizedF64, PositiveF32, PositiveF64};

fn digest(value: impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn assert_zero_keys<T: Debug + Eq + Hash>(positive: T, negative: T) {
    assert_eq!(positive, negative);
    assert_eq!(digest(&positive), digest(&negative));
    let mut keys: HashMap<T, usize, BuildHasherDefault<DefaultHasher>> = HashMap::default();
    keys.insert(positive, 1);
    assert_eq!(keys.get(&negative), Some(&1));
    assert_eq!(keys.insert(negative, 2), Some(1));
    assert_eq!(keys.len(), 1);
}

#[test]
fn signed_zero_f32_hash_map_keys() {
    let negative = FiniteF32::new(-0.0).unwrap();
    assert_eq!(negative.get().to_bits(), (-0.0_f32).to_bits());
    assert_zero_keys(FiniteF32::new(0.0).unwrap(), negative);
    // Positive 是 >= 0；严格 > 0 的 NonZeroPositive 不接受任何零。
    let negative = PositiveF32::new(-0.0).unwrap();
    assert_eq!(negative.get().to_bits(), (-0.0_f32).to_bits());
    assert_zero_keys(PositiveF32::new(0.0).unwrap(), negative);
    let negative = NormalizedF32::new(-0.0).unwrap();
    assert_eq!(negative.get().to_bits(), (-0.0_f32).to_bits());
    assert_zero_keys(NormalizedF32::ZERO, negative);
}

#[test]
fn signed_zero_f64_hash_map_keys() {
    let negative = FiniteF64::new(-0.0).unwrap();
    assert_eq!(negative.get().to_bits(), (-0.0_f64).to_bits());
    assert_zero_keys(FiniteF64::new(0.0).unwrap(), negative);
    let negative = PositiveF64::new(-0.0).unwrap();
    assert_eq!(negative.get().to_bits(), (-0.0_f64).to_bits());
    assert_zero_keys(PositiveF64::new(0.0).unwrap(), negative);
    let negative = NormalizedF64::new(-0.0).unwrap();
    assert_eq!(negative.get().to_bits(), (-0.0_f64).to_bits());
    assert_zero_keys(NormalizedF64::ZERO, negative);
}

#[test]
fn nonzero_hashes_preserve_ieee_bits() {
    for value in [
        1.0_f32,
        -1.0,
        f32::MIN_POSITIVE,
        f32::from_bits(1),
        f32::MAX,
    ] {
        assert_eq!(
            digest(FiniteF32::new(value).unwrap()),
            digest(value.to_bits())
        );
    }
    for value in [
        1.0_f64,
        -1.0,
        f64::MIN_POSITIVE,
        f64::from_bits(1),
        f64::MAX,
    ] {
        assert_eq!(
            digest(FiniteF64::new(value).unwrap()),
            digest(value.to_bits())
        );
    }
}
