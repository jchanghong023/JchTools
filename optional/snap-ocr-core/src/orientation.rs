//! 纯方向重试策略（对应冻结仓库 `src/textsnap/orientation.py` 全文）。
//!
//! 只做判定、不接触图像：给定裁剪尺寸与首试分数，返回强制原方向识别之后
//! 需要加试的旋转角；并在多个识别尝试中按分数、非空文字与旋转偏好决胜。
//! f64 判据（`>`、`<`）与 Python 逐行一致；竖排与低置信度两条规则互斥
//! （Python 的 if/elif），不叠加成四方向重试。

use std::cmp::Ordering;

/// 疑似竖排的高宽比阈值：高/宽大于该值时加试 90° 与 270°。
pub const VERTICAL_ASPECT_THRESHOLD: f64 = 1.3;

/// 首试低置信度阈值：分数小于该值时加试 180°。
pub const LOW_CONFIDENCE_THRESHOLD: f64 = 0.5;

/// 单次识别尝试（对应 Python `RecognitionAttempt`）。
///
/// 不变式沿用 Python `__post_init__` 的调用方契约：`score` 必须有限且处于
/// [0, 1]，`rotation_degrees` 只取 0/90/180/270。字段按编排需求公开
/// （密集代码重试需要原地替换 0° 尝试）；本 crate 内的构造点均满足不变式。
#[derive(Debug, Clone, PartialEq)]
pub struct RecognitionAttempt {
    pub text: String,
    pub score: f64,
    pub rotation_degrees: u16,
}

/// 返回强制原方向识别之后需要加试的旋转角。
///
/// 判据与 Python 一致：高/宽 > [`VERTICAL_ASPECT_THRESHOLD`] 时返回
/// `[90, 270]`；否则首试分 < [`LOW_CONFIDENCE_THRESHOLD`] 时返回 `[180]`；
/// 否则返回空。两条规则互斥，不叠加。
///
/// # Panics
/// 裁剪任一边非正，或 `initial_score` 非有限/越出 [0, 1] 时 panic，
/// 对齐 Python 版的 `ValueError` 契约。
pub fn additional_rotations(crop_height: f64, crop_width: f64, initial_score: f64) -> Vec<u16> {
    assert!(
        crop_height > 0.0 && crop_width > 0.0,
        "crop dimensions must be positive"
    );
    assert!(
        initial_score.is_finite() && (0.0..=1.0).contains(&initial_score),
        "initial_score must be between 0 and 1"
    );
    if crop_height / crop_width > VERTICAL_ASPECT_THRESHOLD {
        vec![90, 270]
    } else if initial_score < LOW_CONFIDENCE_THRESHOLD {
        vec![180]
    } else {
        Vec::new()
    }
}

/// 旋转偏好：分数与非空文字都并列时，按 0°、180°、90°、270° 顺序决胜
/// （0° 偏好值最大）。
fn rotation_preference(rotation_degrees: u16) -> u8 {
    match rotation_degrees {
        0 => 3,
        180 => 2,
        90 => 1,
        // 270° 以及（按调用方契约不该出现的）其他取值同为最低偏好。
        _ => 0,
    }
}

/// 选择最高置信度的尝试：分数更高者优先；分数并列时优先非空文字；
/// 仍并列时按 0°、180°、90°、270° 偏好决胜；完全并列时保留最先出现的
/// 尝试（与 Python `max` 保留首个最大值一致，因此不用 `max_by`）。
///
/// # Panics
/// `attempts` 为空时 panic，对齐 Python `best_attempt` 空序列抛
/// `ValueError` 的语义。
pub fn select_best(attempts: &[RecognitionAttempt]) -> &RecognitionAttempt {
    assert!(
        !attempts.is_empty(),
        "at least one recognition attempt is required"
    );
    let mut best = 0;
    for index in 1..attempts.len() {
        let candidate = &attempts[index];
        let incumbent = &attempts[best];
        let better = match candidate
            .score
            .partial_cmp(&incumbent.score)
            .unwrap_or(Ordering::Equal)
        {
            Ordering::Greater => true,
            Ordering::Equal => {
                let candidate_text = !candidate.text.is_empty();
                let incumbent_text = !incumbent.text.is_empty();
                match candidate_text.cmp(&incumbent_text) {
                    Ordering::Greater => true,
                    Ordering::Equal => {
                        rotation_preference(candidate.rotation_degrees)
                            > rotation_preference(incumbent.rotation_degrees)
                    }
                    Ordering::Less => false,
                }
            }
            Ordering::Less => false,
        };
        if better {
            best = index;
        }
    }
    &attempts[best]
}

#[cfg(test)]
mod tests {
    // 覆盖 O-26：方向重试策略与平局决胜。
    // 翻译冻结仓库 tests/test_orientation.py 全部 5 例，用例名保持对应。
    // 分数均为字面量构造，允许精确比较；测试内允许 unwrap/expect。
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]

    use super::*;

    // 覆盖 O-26（test_vertical_crop_tries_both_quarter_turns）
    #[test]
    fn vertical_crop_tries_both_quarter_turns() {
        assert_eq!(additional_rotations(30.0, 20.0, 0.9), vec![90, 270]);
    }

    // 覆盖 O-26（test_low_confidence_horizontal_crop_tries_180）
    #[test]
    fn low_confidence_horizontal_crop_tries_180() {
        assert_eq!(additional_rotations(20.0, 100.0, 0.49), vec![180]);
    }

    // 覆盖 O-26（test_confident_horizontal_crop_stops）
    #[test]
    fn confident_horizontal_crop_stops() {
        assert!(additional_rotations(20.0, 100.0, 0.5).is_empty());
    }

    // 覆盖 O-26（test_best_score_wins）
    #[test]
    fn best_score_wins() {
        let attempts = vec![
            RecognitionAttempt {
                text: "upside".to_string(),
                score: 0.3,
                rotation_degrees: 0,
            },
            RecognitionAttempt {
                text: "correct".to_string(),
                score: 0.95,
                rotation_degrees: 180,
            },
        ];
        assert_eq!(select_best(&attempts), &attempts[1]);
    }

    // 覆盖 O-26（test_unrotated_wins_score_tie）
    #[test]
    fn unrotated_wins_score_tie() {
        let attempts = vec![
            RecognitionAttempt {
                text: "same".to_string(),
                score: 0.8,
                rotation_degrees: 180,
            },
            RecognitionAttempt {
                text: "same".to_string(),
                score: 0.8,
                rotation_degrees: 0,
            },
        ];
        assert_eq!(select_best(&attempts), &attempts[1]);
    }
}
