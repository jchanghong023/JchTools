use thiserror::Error;

#[derive(Error, Debug)]
pub enum OcrError {
    #[cfg(feature = "ort")]
    #[error("Ort error: {0}")]
    Ort(#[from] ort::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Image error: {0}")]
    ImageError(#[from] image::ImageError),
    #[error("Session not initialized")]
    SessionNotInitialized,
    /// A backend-neutral inference failure (raised by the `tract` backend, and by the
    /// `ort` backend for failures that are not themselves an [`ort::Error`], such as an
    /// empty output or a shape mismatch).
    #[error("Inference error: {message}")]
    Inference {
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },
}

impl OcrError {
    /// Build an [`OcrError::Inference`] without an underlying source error.
    pub fn inference(message: impl Into<String>) -> Self {
        Self::Inference {
            message: message.into(),
            source: None,
        }
    }
}
