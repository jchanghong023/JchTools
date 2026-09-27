use ndarray::Array4;
use std::collections::HashMap;

#[cfg(feature = "ort")]
use ort::session::builder::SessionBuilder;

use crate::{
    base_net::BaseNet,
    constants::{CRNN_MEAN_VALUES, CRNN_NORM_VALUES},
    inference::{self, ModelBackend},
    ocr_error::OcrError,
    ocr_result::{DetailedTextLine, RecognizedWord, TextLine},
    ocr_utils::OcrUtils,
};

const CRNN_DST_HEIGHT: u32 = 48;
const CTC_SPACE_TOKEN: &str = " ";
const CTC_WORD_GAP_THRESHOLD: u32 = 5;

#[derive(Debug, Clone, Copy)]
struct DecodedToken<'a> {
    text: &'a str,
    column: u32,
    confidence: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecognitionDictionaryLayout {
    Complete,
    MissingBlank,
    MissingBlankAndSpace,
}

impl RecognitionDictionaryLayout {
    fn resolve(dictionary_len: usize, class_count: usize) -> Result<Self, OcrError> {
        match class_count.checked_sub(dictionary_len) {
            Some(0) => Ok(Self::Complete),
            Some(1) => Ok(Self::MissingBlank),
            Some(2) => Ok(Self::MissingBlankAndSpace),
            _ => Err(OcrError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "Recognition dictionary has {dictionary_len} entries but model output has \
                     {class_count} classes; expected an equal class count or exactly one/two \
                     additional CTC classes"
                ),
            ))),
        }
    }

    fn token(self, class_index: usize, keys: &[String]) -> Option<&str> {
        match self {
            Self::Complete => keys.get(class_index).map(String::as_str),
            Self::MissingBlank => keys.get(class_index.checked_sub(1)?).map(String::as_str),
            Self::MissingBlankAndSpace if class_index == keys.len() + 1 => Some(CTC_SPACE_TOKEN),
            Self::MissingBlankAndSpace => keys.get(class_index.checked_sub(1)?).map(String::as_str),
        }
    }
}

pub struct CrnnNet {
    backend: Option<Box<dyn ModelBackend>>,
    keys: Vec<String>,
}

impl std::fmt::Debug for CrnnNet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrnnNet")
            .field("initialized", &self.backend.is_some())
            .field(
                "backend",
                &self.backend.as_ref().map(|backend| backend.name()),
            )
            .field("keys", &self.keys.len())
            .finish()
    }
}

impl BaseNet for CrnnNet {
    fn new() -> Self {
        Self {
            backend: None,
            keys: Vec::new(),
        }
    }

    fn set_backend(&mut self, backend: Option<Box<dyn ModelBackend>>) {
        self.backend = backend;
    }
}

impl CrnnNet {
    pub fn init_model(&mut self, path: &str, num_thread: usize) -> Result<(), OcrError> {
        BaseNet::init_model(self, path, num_thread)?;

        self.keys = self.get_keys()?;

        Ok(())
    }

    pub fn init_model_dict_file(
        &mut self,
        path: &str,
        num_thread: usize,
        dict_file_path: &str,
    ) -> Result<(), OcrError> {
        BaseNet::init_model(self, path, num_thread)?;

        self.read_keys_from_file(dict_file_path)?;

        Ok(())
    }

    /// Load the recognition model onto an explicitly chosen backend, reading the CTC
    /// dictionary from `dict_file_path`.
    ///
    /// The explicit dictionary is what makes this backend-neutral: only `ort` can surface the
    /// model's embedded `"character"` metadata (see [`Self::get_keys`]), so a `tract` load must
    /// be given the dictionary out of band. Both engines then decode against the same key list,
    /// which is a precondition for comparing their decoded text.
    pub fn init_model_dict_file_on(
        &mut self,
        backend: crate::inference::Backend,
        path: &str,
        num_thread: usize,
        dict_file_path: &str,
    ) -> Result<(), OcrError> {
        BaseNet::init_model_on(self, backend, path, num_thread)?;

        self.read_keys_from_file(dict_file_path)?;

        Ok(())
    }

    pub fn init_model_from_memory(
        &mut self,
        model_bytes: &[u8],
        num_thread: usize,
    ) -> Result<(), OcrError> {
        BaseNet::init_model_from_memory(self, model_bytes, num_thread)?;

        self.keys = self.get_keys()?;

        Ok(())
    }

    /// Load onto the `ort` backend with a custom session builder (e.g. for GPU
    /// execution providers), deriving the dictionary from the model's embedded
    /// `"character"` metadata.
    #[cfg(feature = "ort")]
    pub fn init_model_with_ort_builder(
        &mut self,
        path: &str,
        num_thread: usize,
        builder_fn: Option<fn(SessionBuilder) -> Result<SessionBuilder, ort::Error>>,
    ) -> Result<(), OcrError> {
        BaseNet::init_model_with_ort_builder(self, path, num_thread, builder_fn)?;

        self.keys = self.get_keys()?;

        Ok(())
    }

    /// Load onto the `ort` backend with a custom session builder, reading the
    /// dictionary from an explicit file instead of model metadata.
    #[cfg(feature = "ort")]
    pub fn init_model_dict_file_with_ort_builder(
        &mut self,
        path: &str,
        num_thread: usize,
        builder_fn: Option<fn(SessionBuilder) -> Result<SessionBuilder, ort::Error>>,
        dict_file_path: &str,
    ) -> Result<(), OcrError> {
        BaseNet::init_model_with_ort_builder(self, path, num_thread, builder_fn)?;

        self.read_keys_from_file(dict_file_path)?;

        Ok(())
    }

    /// Load from ONNX bytes onto the `ort` backend with a custom session builder.
    #[cfg(feature = "ort")]
    pub fn init_model_from_memory_with_ort_builder(
        &mut self,
        model_bytes: &[u8],
        num_thread: usize,
        builder_fn: Option<fn(SessionBuilder) -> Result<SessionBuilder, ort::Error>>,
    ) -> Result<(), OcrError> {
        BaseNet::init_model_from_memory_with_ort_builder(
            self,
            model_bytes,
            num_thread,
            builder_fn,
        )?;

        self.keys = self.get_keys()?;

        Ok(())
    }

    /// Read the CTC dictionary from the loaded model's embedded `"character"`
    /// metadata.
    ///
    /// Only the `ort` backend can currently surface this ONNX metadata property (see
    /// [`ModelBackend::custom_metadata`](crate::inference::ModelBackend::custom_metadata));
    /// a `tract`-only build must load the dictionary from an explicit file via
    /// [`Self::init_model_dict_file`].
    fn get_keys(&mut self) -> Result<Vec<String>, OcrError> {
        let backend = self
            .backend
            .as_ref()
            .ok_or(OcrError::SessionNotInitialized)?;

        let model_charater_list = backend.custom_metadata("character").ok_or_else(|| {
            OcrError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "crnn_net character not found in metadata",
            ))
        })?;

        let keys: Vec<String> = model_charater_list
            .split('\n')
            .map(|s: &str| s.to_string())
            .collect();

        Ok(keys)
    }

    fn read_keys_from_file(&mut self, path: &str) -> Result<(), OcrError> {
        let content = std::fs::read_to_string(path)?;

        self.keys = content.lines().map(str::to_string).collect();
        Ok(())
    }

    pub fn get_text_lines(
        &self,
        part_imgs: &[image::RgbImage],
        angle_rollback_records: &HashMap<usize, image::RgbImage>,
        angle_rollback_threshold: f32,
        batch_size: u32,
    ) -> Result<Vec<TextLine>, OcrError> {
        self.get_detailed_text_lines(
            part_imgs,
            angle_rollback_records,
            angle_rollback_threshold,
            batch_size,
        )
        .map(|lines| lines.into_iter().map(Into::into).collect())
    }

    /// Recognize text while retaining word-level CTC alignment details.
    pub fn get_detailed_text_lines(
        &self,
        part_imgs: &[image::RgbImage],
        angle_rollback_records: &HashMap<usize, image::RgbImage>,
        angle_rollback_threshold: f32,
        batch_size: u32,
    ) -> Result<Vec<DetailedTextLine>, OcrError> {
        if part_imgs.is_empty() {
            return Ok(Vec::new());
        }

        let part_img_refs = part_imgs.iter().collect::<Vec<_>>();
        let mut text_lines = self.get_detailed_text_lines_batched(&part_img_refs, batch_size)?;

        let rollback_indices = text_lines
            .iter()
            .enumerate()
            .filter_map(|(index, text_line)| {
                if text_line.line.text_score.is_nan()
                    || text_line.line.text_score < angle_rollback_threshold
                {
                    angle_rollback_records
                        .get(&index)
                        .map(|image| (index, image))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        if rollback_indices.len() == 1 {
            let (index, image) = rollback_indices[0];
            text_lines[index] = self.get_detailed_text_line(image)?;
        } else if !rollback_indices.is_empty() {
            let images = rollback_indices
                .iter()
                .map(|(_, image)| *image)
                .collect::<Vec<_>>();
            let replacements = self.get_detailed_text_lines_batched(&images, batch_size)?;
            for ((index, _), replacement) in rollback_indices.into_iter().zip(replacements) {
                text_lines[index] = replacement;
            }
        }

        Ok(text_lines)
    }

    /// Batch recognition: sort crops by width, group into batches, pad to max width,
    /// run single ONNX inference per batch. Matches PaddleOCR/RapidOCR batching strategy.
    fn get_detailed_text_lines_batched(
        &self,
        part_imgs: &[&image::RgbImage],
        batch_size: u32,
    ) -> Result<Vec<DetailedTextLine>, OcrError> {
        let backend = self
            .backend
            .as_ref()
            .ok_or(OcrError::SessionNotInitialized)?;
        let batch_size = (batch_size as usize).max(1);

        let mut indexed_widths: Vec<(usize, u32)> = part_imgs
            .iter()
            .enumerate()
            .map(|(i, img)| {
                let scale = CRNN_DST_HEIGHT as f32 / img.height().max(1) as f32;
                let dst_width = (img.width() as f32 * scale).ceil() as u32;
                (i, dst_width.max(1))
            })
            .collect();
        indexed_widths.sort_by_key(|&(_, w)| w);

        let mut results: Vec<(usize, DetailedTextLine)> = Vec::with_capacity(part_imgs.len());

        for chunk in indexed_widths.chunks(batch_size) {
            if chunk.len() == 1 {
                let (orig_idx, _) = chunk[0];
                let text_line = self.get_detailed_text_line(part_imgs[orig_idx])?;
                results.push((orig_idx, text_line));
                continue;
            }

            let max_width = chunk.iter().map(|&(_, w)| w).max().unwrap_or(1);

            let n = chunk.len();
            let mut batch_data =
                Array4::<f32>::zeros((n, 3, CRNN_DST_HEIGHT as usize, max_width as usize));

            for (batch_idx, &(orig_idx, dst_width)) in chunk.iter().enumerate() {
                let img = part_imgs[orig_idx];
                let resized = image::imageops::resize(
                    img,
                    dst_width,
                    CRNN_DST_HEIGHT,
                    image::imageops::FilterType::Triangle,
                );
                Self::write_batch_row(&mut batch_data, batch_idx, &resized);
            }

            let (shape, flat_data) = inference::run_flat(backend.as_ref(), batch_data.into_dyn())?;
            let batch_dim = *shape.first().unwrap_or(&1);
            let timesteps = *shape.get(1).unwrap_or(&0);
            let num_classes = *shape.get(2).unwrap_or(&0);

            for (batch_idx, item) in chunk.iter().enumerate().take(batch_dim.min(n)) {
                let offset = batch_idx * timesteps * num_classes;
                let slice = &flat_data[offset..offset + timesteps * num_classes];
                let line_column_count = timesteps as f32 * item.1 as f32 / max_width as f32;
                let text_line = Self::score_to_detailed_text_line(
                    slice,
                    timesteps,
                    num_classes,
                    &self.keys,
                    line_column_count,
                )?;
                results.push((item.0, text_line));
            }
        }

        if results.len() != part_imgs.len() {
            return Err(OcrError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "batched CRNN recognition returned {} results for {} input images",
                    results.len(),
                    part_imgs.len()
                ),
            )));
        }

        results.sort_by_key(|&(idx, _)| idx);
        Ok(results.into_iter().map(|(_, tl)| tl).collect())
    }

    /// Normalize one resized crop into row `batch_idx` of `batch_data`, matching
    /// [`crate::ocr_utils::OcrUtils::substract_mean_normalize`]'s per-channel formula. ~keep
    fn write_batch_row(batch_data: &mut Array4<f32>, batch_idx: usize, resized: &image::RgbImage) {
        let cols = resized.width() as usize;
        let rows = resized.height() as usize;
        let raw = resized.as_raw();
        assert_eq!(raw.len(), rows * cols * 3, "unexpected image buffer size");
        let adjusted = [
            CRNN_MEAN_VALUES[0] * CRNN_NORM_VALUES[0],
            CRNN_MEAN_VALUES[1] * CRNN_NORM_VALUES[1],
            CRNN_MEAN_VALUES[2] * CRNN_NORM_VALUES[2],
        ];
        for r in 0..rows {
            for c in 0..cols {
                let base = r * cols * 3 + c * 3;
                for ch in 0..3 {
                    batch_data[[batch_idx, ch, r, c]] =
                        raw[base + ch] as f32 * CRNN_NORM_VALUES[ch] - adjusted[ch];
                }
            }
        }
    }

    fn get_detailed_text_line(
        &self,
        img_src: &image::RgbImage,
    ) -> Result<DetailedTextLine, OcrError> {
        let Some(backend) = &self.backend else {
            return Err(OcrError::SessionNotInitialized);
        };

        let scale = CRNN_DST_HEIGHT as f32 / img_src.height() as f32;
        let dst_width = (img_src.width() as f32 * scale).ceil() as u32;

        let src_resize = image::imageops::resize(
            img_src,
            dst_width,
            CRNN_DST_HEIGHT,
            image::imageops::FilterType::Triangle,
        );

        let input_tensors =
            OcrUtils::substract_mean_normalize(&src_resize, &CRNN_MEAN_VALUES, &CRNN_NORM_VALUES);

        let (shape, src_data) = inference::run_flat(backend.as_ref(), input_tensors.into_dyn())?;
        let height = *shape.get(1).ok_or_else(|| {
            OcrError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "CRNN output tensor missing height dimension (index 1)",
            ))
        })?;
        let width = *shape.get(2).ok_or_else(|| {
            OcrError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "CRNN output tensor missing width dimension (index 2)",
            ))
        })?;

        Self::score_to_detailed_text_line(&src_data, height, width, &self.keys, height as f32)
    }

    fn score_to_detailed_text_line(
        output_data: &[f32],
        height: usize,
        width: usize,
        keys: &[String],
        line_column_count: f32,
    ) -> Result<DetailedTextLine, OcrError> {
        let dictionary_layout = RecognitionDictionaryLayout::resolve(keys.len(), width)?;
        let mut text_line = TextLine::default();
        let mut decoded_tokens = Vec::new();
        let mut last_index = 0;

        let mut text_score_sum = 0.0;
        let mut text_score_count = 0;
        for column in 0..height {
            let start = column * width;
            let stop = (column + 1) * width;
            let slice = &output_data[start..stop.min(output_data.len())];

            let (max_index, max_value) =
                slice
                    .iter()
                    .enumerate()
                    .fold((0, f32::MIN), |(max_idx, max_val), (idx, &val)| {
                        if val > max_val {
                            (idx, val)
                        } else {
                            (max_idx, max_val)
                        }
                    });

            if max_index > 0
                && !(column > 0 && max_index == last_index)
                && let Some(token) = dictionary_layout.token(max_index, keys)
            {
                text_line.text.push_str(token);
                text_score_sum += max_value;
                text_score_count += 1;
                decoded_tokens.push(DecodedToken {
                    text: token,
                    column: column as u32,
                    confidence: max_value,
                });
            }
            last_index = max_index;
        }

        text_line.text_score = if text_score_count > 0 {
            text_score_sum / text_score_count as f32
        } else {
            0.0
        };

        Ok(DetailedTextLine {
            line: text_line,
            words: Self::group_recognized_words(&decoded_tokens),
            line_column_count,
        })
    }

    fn group_recognized_words(tokens: &[DecodedToken<'_>]) -> Vec<RecognizedWord> {
        let mut words = Vec::new();
        let mut current_text = String::new();
        let mut current_columns = Vec::new();
        let mut confidence_sum = 0.0;
        let mut current_is_cjk = None;

        for token in tokens {
            if token.text.chars().all(char::is_whitespace) {
                Self::finish_recognized_word(
                    &mut words,
                    &mut current_text,
                    &mut current_columns,
                    &mut confidence_sum,
                );
                current_is_cjk = None;
                continue;
            }

            let token_is_cjk = token.text.chars().any(Self::is_cjk_character);
            let gap_splits_word = current_columns.last().is_some_and(|&column| {
                token.column.saturating_sub(column) > CTC_WORD_GAP_THRESHOLD
            });
            let split_before_cjk = token_is_cjk && !current_columns.is_empty();
            if current_is_cjk.is_some_and(|is_cjk| is_cjk != token_is_cjk)
                || gap_splits_word
                || split_before_cjk
            {
                Self::finish_recognized_word(
                    &mut words,
                    &mut current_text,
                    &mut current_columns,
                    &mut confidence_sum,
                );
            }

            current_is_cjk = Some(token_is_cjk);
            current_text.push_str(token.text);
            current_columns.push(token.column);
            confidence_sum += token.confidence;
        }

        Self::finish_recognized_word(
            &mut words,
            &mut current_text,
            &mut current_columns,
            &mut confidence_sum,
        );
        words
    }

    fn finish_recognized_word(
        words: &mut Vec<RecognizedWord>,
        text: &mut String,
        columns: &mut Vec<u32>,
        confidence_sum: &mut f32,
    ) {
        let Some(&start_column) = columns.first() else {
            return;
        };
        let end_column = columns.last().copied().unwrap_or(start_column);
        let mean_confidence = *confidence_sum / columns.len() as f32;
        let confidence = if mean_confidence.is_finite() {
            mean_confidence
        } else {
            0.0
        };
        words.push(RecognizedWord {
            text: std::mem::take(text),
            columns: std::mem::take(columns),
            start_column,
            end_column,
            confidence,
        });
        *confidence_sum = 0.0;
    }

    fn is_cjk_character(character: char) -> bool {
        matches!(
            character,
            '\u{3400}'..='\u{4DBF}'
                | '\u{4E00}'..='\u{9FFF}'
                | '\u{F900}'..='\u{FAFF}'
                | '\u{20000}'..='\u{2FA1F}'
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score_to_text_line(
        output_data: &[f32],
        height: usize,
        width: usize,
        keys: &[String],
    ) -> Result<TextLine, OcrError> {
        CrnnNet::score_to_detailed_text_line(output_data, height, width, keys, height as f32)
            .map(Into::into)
    }

    fn output_for_classes(class_count: usize, classes: &[usize]) -> Vec<f32> {
        let mut output = vec![0.0; class_count * classes.len()];
        for (timestep, class_index) in classes.iter().copied().enumerate() {
            output[timestep * class_count + class_index] = 1.0;
        }
        output
    }

    fn output_for_class_scores(class_count: usize, classes: &[(usize, f32)]) -> Vec<f32> {
        let mut output = vec![0.0; class_count * classes.len()];
        for (timestep, &(class_index, score)) in classes.iter().enumerate() {
            output[timestep * class_count + class_index] = score;
        }
        output
    }

    #[test]
    fn test_score_to_text_line_skips_blank_index() {
        let keys = vec!["#".to_string(), "a".to_string(), "b".to_string()];
        let output = vec![1.0, 0.0, 0.0, 0.0, 0.9, 0.1, 0.0, 0.1, 0.8];
        let result = score_to_text_line(&output, 3, 3, &keys).unwrap();
        assert_eq!(result.text, "ab");
    }

    #[test]
    fn test_score_to_text_line_deduplicates_consecutive() {
        let keys = vec!["#".to_string(), "h".to_string(), "i".to_string()];
        let output = vec![0.0, 0.9, 0.0, 0.0, 0.8, 0.0, 0.0, 0.0, 0.9, 0.0, 0.0, 0.8];
        let result = score_to_text_line(&output, 4, 3, &keys).unwrap();
        assert_eq!(result.text, "hi");
    }

    #[test]
    fn should_align_duplicate_ctc_tokens_and_blanks_to_selected_columns() {
        let keys = vec!["a".to_string(), "b".to_string()];
        let class_count = keys.len() + 1;
        let output = output_for_class_scores(
            class_count,
            &[(1, 0.9), (1, 0.8), (0, 1.0), (1, 0.7), (2, 0.6)],
        );

        let result =
            CrnnNet::score_to_detailed_text_line(&output, 5, class_count, &keys, 5.0).unwrap();

        assert_eq!(result.line.text, "aab");
        assert_eq!(result.words.len(), 1);
        assert_eq!(result.words[0].text, "aab");
        assert_eq!(result.words[0].columns, [0, 3, 4]);
        assert_eq!(result.words[0].start_column, 0);
        assert_eq!(result.words[0].end_column, 4);
        assert!((result.words[0].confidence - 0.733_333_35).abs() < f32::EPSILON);
    }

    #[test]
    fn should_group_english_words_across_spaces_with_word_confidence() {
        let keys = vec![
            "h".to_string(),
            "i".to_string(),
            "b".to_string(),
            "y".to_string(),
            "e".to_string(),
        ];
        let class_count = keys.len() + 2;
        let output = output_for_class_scores(
            class_count,
            &[(1, 0.8), (2, 0.6), (6, 0.9), (3, 0.7), (4, 0.9), (5, 0.8)],
        );

        let result =
            CrnnNet::score_to_detailed_text_line(&output, 6, class_count, &keys, 4.5).unwrap();

        assert_eq!(result.line.text, "hi bye");
        assert_eq!(result.line_column_count, 4.5);
        assert_eq!(
            result
                .words
                .iter()
                .map(|word| word.text.as_str())
                .collect::<Vec<_>>(),
            ["hi", "bye"]
        );
        assert_eq!(result.words[0].columns, [0, 1]);
        assert_eq!(result.words[1].columns, [3, 4, 5]);
        assert!((result.words[0].confidence - 0.7).abs() < f32::EPSILON);
        assert!((result.words[1].confidence - 0.8).abs() < f32::EPSILON);
    }

    #[test]
    fn should_split_english_words_when_ctc_columns_have_a_large_gap() {
        let tokens = [
            DecodedToken {
                text: "a",
                column: 1,
                confidence: 0.8,
            },
            DecodedToken {
                text: "b",
                column: 8,
                confidence: 0.6,
            },
        ];

        let words = CrnnNet::group_recognized_words(&tokens);

        assert_eq!(words.len(), 2);
        assert_eq!(words[0].text, "a");
        assert_eq!(words[1].text, "b");
    }

    #[test]
    fn should_emit_each_cjk_character_as_a_word() {
        let tokens = [
            DecodedToken {
                text: "中",
                column: 1,
                confidence: 0.8,
            },
            DecodedToken {
                text: "文",
                column: 2,
                confidence: 0.6,
            },
        ];

        let words = CrnnNet::group_recognized_words(&tokens);

        assert_eq!(
            words
                .iter()
                .map(|word| word.text.as_str())
                .collect::<Vec<_>>(),
            ["中", "文"]
        );
    }

    #[test]
    fn should_sanitize_non_finite_word_confidence() {
        let tokens = [DecodedToken {
            text: "a",
            column: 1,
            confidence: f32::INFINITY,
        }];

        let words = CrnnNet::group_recognized_words(&tokens);

        assert_eq!(words.len(), 1);
        assert_eq!(words[0].confidence, 0.0);
        assert!(words[0].confidence.is_finite());
    }

    #[test]
    fn test_score_to_text_line_preserves_complete_dictionary() {
        let keys = vec!["#".to_string(), "a".to_string(), " ".to_string()];
        let output = output_for_classes(keys.len(), &[0, 1, 2]);
        let result = score_to_text_line(&output, 3, keys.len(), &keys).unwrap();
        assert_eq!(result.text, "a ");
    }

    #[test]
    fn test_score_to_text_line_prepends_missing_blank() {
        let keys = vec!["a".to_string(), "b".to_string()];
        let class_count = keys.len() + 1;
        let output = output_for_classes(class_count, &[0, 1, 2]);
        let result = score_to_text_line(&output, 3, class_count, &keys).unwrap();
        assert_eq!(result.text, "ab");
    }

    #[test]
    fn test_score_to_text_line_adds_missing_blank_and_space() {
        let keys = vec!["a".to_string(), "b".to_string()];
        let class_count = keys.len() + 2;
        let output = output_for_classes(class_count, &[0, 1, 2, 3]);
        let result = score_to_text_line(&output, 4, class_count, &keys).unwrap();
        assert_eq!(result.text, "ab ");
    }

    #[test]
    fn test_score_to_text_line_rejects_dictionary_class_mismatch() {
        let keys = vec!["a".to_string()];
        let class_count = keys.len() + 3;
        let output = output_for_classes(class_count, &[1]);
        let error = score_to_text_line(&output, 1, class_count, &keys).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("dictionary has 1 entries"));
        assert!(message.contains("model output has 4 classes"));
    }

    #[test]
    fn test_read_keys_from_file_preserves_crlf_and_internal_blank() {
        let dir =
            std::env::temp_dir().join(format!("xberg_test_crnn_dictionary_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dict_path = dir.join("test_dict.txt");
        std::fs::write(&dict_path, "a\r\n\r\nb\r\n").unwrap();

        let mut net = CrnnNet::new();
        net.read_keys_from_file(dict_path.to_str().unwrap())
            .unwrap();

        assert_eq!(net.keys, ["a", "", "b"]);

        std::fs::remove_dir_all(&dir).ok();
    }
}
