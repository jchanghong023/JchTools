use crate::{
    base_net::BaseNet,
    constants::{IMAGENET_MEAN_VALUES, IMAGENET_NORM_VALUES},
    inference::{self, ModelBackend},
    ocr_error::OcrError,
    ocr_result::Angle,
    ocr_utils::OcrUtils,
};

const ANGLE_DST_WIDTH: u32 = 160;
const ANGLE_DST_HEIGHT: u32 = 80;
const ANGLE_COLS: usize = 2;

pub struct AngleNet {
    backend: Option<Box<dyn ModelBackend>>,
}

impl std::fmt::Debug for AngleNet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AngleNet")
            .field("initialized", &self.backend.is_some())
            .field(
                "backend",
                &self.backend.as_ref().map(|backend| backend.name()),
            )
            .finish()
    }
}

impl BaseNet for AngleNet {
    fn new() -> Self {
        Self { backend: None }
    }

    fn set_backend(&mut self, backend: Option<Box<dyn ModelBackend>>) {
        self.backend = backend;
    }
}

impl AngleNet {
    pub fn get_angles(
        &self,
        part_imgs: &[image::RgbImage],
        do_angle: bool,
        most_angle: bool,
        cls_thresh: f32,
    ) -> Result<Vec<Angle>, OcrError> {
        let mut angles = Vec::with_capacity(part_imgs.len());

        if do_angle {
            for img in part_imgs {
                let angle = self.get_angle(img, cls_thresh)?;
                angles.push(angle);
            }
        } else {
            angles.extend(part_imgs.iter().map(|_| Angle::default()));
        }

        if do_angle && most_angle {
            let sum: i32 = angles.iter().map(|x| x.index).sum();
            let half_percent = angles.len() as f32 / 2.0;
            let most_angle_index = if (sum as f32) < half_percent { 0 } else { 1 };

            for angle in angles.iter_mut() {
                angle.index = most_angle_index;
            }
        }

        Ok(angles)
    }

    fn get_angle(&self, img_src: &image::RgbImage, cls_thresh: f32) -> Result<Angle, OcrError> {
        let Some(backend) = &self.backend else {
            return Err(OcrError::SessionNotInitialized);
        };

        let angle_img = image::imageops::resize(
            img_src,
            ANGLE_DST_WIDTH,
            ANGLE_DST_HEIGHT,
            image::imageops::FilterType::Triangle,
        );

        let input_tensors = OcrUtils::substract_mean_normalize(
            &angle_img,
            &IMAGENET_MEAN_VALUES,
            &IMAGENET_NORM_VALUES,
        );

        let (_, src_data) = inference::run_flat(backend.as_ref(), input_tensors.into_dyn())?;

        let mut angle = Self::score_to_angle(&src_data, ANGLE_COLS);

        if angle.score < cls_thresh {
            angle.index = 0;
        }

        Ok(angle)
    }

    fn score_to_angle(src_data: &[f32], angle_cols: usize) -> Angle {
        let mut angle = Angle::default();
        let mut max_value = f32::MIN;
        let mut angle_index = 0;

        for (i, value) in src_data.iter().take(angle_cols).enumerate() {
            if *value > max_value {
                max_value = *value;
                angle_index = i as i32;
            }
        }

        angle.index = angle_index;
        angle.score = max_value;
        angle
    }
}
