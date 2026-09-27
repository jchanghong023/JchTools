//! Runtime-neutral inference backend seam.
//!
//! [`ModelBackend`] abstracts over the concrete ONNX runtime so `DbNet`, `AngleNet`,
//! and `CrnnNet` never depend on a specific engine:
//!
//! - `ort` -- native ONNX Runtime (desktop/server; today's only path).
//! - `tract` -- pure-Rust ONNX, needed on wasm32 and the Android x86_64 emulator,
//!   where `ort` cannot link.
//!
//! [`load_backend`] selects an implementation from [`Backend`]; a backend that is not
//! compiled in returns an [`OcrError`] naming the missing cargo feature.

use ndarray::ArrayD;

use crate::ocr_error::OcrError;

#[cfg(feature = "ort")]
pub(crate) mod ort_backend;
#[cfg(feature = "tract")]
pub(crate) mod tract_backend;

/// A dynamically-shaped `f32` tensor exchanged with a backend.
pub type Tensor = ArrayD<f32>;

/// Which concrete inference engine a model is loaded onto.
///
/// Both variants always exist regardless of which backend features are compiled in
/// (so [`default_backend`] and [`load_backend`] stay simple `match`es); a variant
/// whose feature is disabled is naturally unreachable at those call sites, which is
/// not a real dead-code condition.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Native ONNX Runtime (`ort`).
    Ort,
    /// Pure-Rust ONNX (`tract-onnx`).
    Tract,
}

impl Backend {
    /// Short backend name, matching [`ModelBackend::name`] for a model loaded onto it.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Ort => "ort",
            Self::Tract => "tract",
        }
    }

    /// Whether a plan for this backend can only run at the one input shape it was built for.
    ///
    /// `tract` optimizes a graph against concrete facts; DBNet's FPN skip connections do not
    /// unify under a symbolic H/W, so a DBNet plan has to be pinned to a concrete input shape
    /// (see [`tract_backend`](self::tract_backend)). `ort` shape-infers the dynamic graph at
    /// run time and reuses one session for every page.
    pub(crate) fn requires_concrete_input_shape(self) -> bool {
        match self {
            Self::Ort => false,
            Self::Tract => true,
        }
    }
}

/// Check that `model_bytes` is a model `backend` can load, without building a runnable plan.
///
/// A backend that needs a concrete input shape cannot build its plan until the first page's
/// extent is known; without this check a corrupt or unsupported detection model would surface
/// mid-extraction rather than at load time, where every other model failure is reported.
pub(crate) fn validate_model(backend: Backend, model_bytes: &[u8]) -> Result<(), OcrError> {
    let _ = model_bytes;
    match backend {
        #[cfg(feature = "tract")]
        Backend::Tract => tract_backend::parse_only(model_bytes),
        _ => Ok(()),
    }
}

/// The compile-time default backend for the plain (non-customized) `init_model*`
/// call sites: `ort` when it is compiled in, preserving today's behavior exactly;
/// otherwise `tract`, so a `tract`-only build stays usable through the same API.
pub(crate) fn default_backend() -> Backend {
    #[cfg(feature = "ort")]
    {
        Backend::Ort
    }
    #[cfg(not(feature = "ort"))]
    {
        Backend::Tract
    }
}

/// A loaded model that can run inference on a single input tensor.
pub trait ModelBackend: Send + Sync {
    /// Short backend name, for diagnostics.
    fn name(&self) -> &str;

    /// Run inference, mapping one input tensor to one output tensor.
    fn run(&self, input: Tensor) -> Result<Tensor, OcrError>;

    /// Fetch a custom ONNX metadata property embedded in the model.
    ///
    /// PaddleOCR's CRNN recognition models embed their character dictionary under
    /// the `"character"` metadata key; [`CrnnNet`](crate::crnn_net::CrnnNet) reads it
    /// through this method rather than depending on a concrete backend type. Only
    /// `ort` can currently surface arbitrary ONNX metadata; `tract` returns `None`,
    /// so a `tract` build must supply the dictionary explicitly via
    /// `CrnnNet::init_model_dict_file`.
    fn custom_metadata(&self, _key: &str) -> Option<String> {
        None
    }
}

/// Load a model from ONNX bytes using the requested backend.
///
/// `threads` caps the backend's intra-op parallelism where supported (`ort` only;
/// `tract` ignores it -- see the `tract-linalg` note in
/// [`tract_backend`](self::tract_backend)). `fixed_input` pins the model's input to a
/// concrete shape for backends that cannot shape-infer a graph with data-dependent
/// dynamic dimensions; `ort` ignores it unconditionally.
pub fn load_backend(
    backend: Backend,
    model_bytes: &[u8],
    threads: usize,
    fixed_input: Option<&[usize]>,
) -> Result<Box<dyn ModelBackend>, OcrError> {
    let _ = (model_bytes, threads, fixed_input);
    match backend {
        #[cfg(feature = "ort")]
        Backend::Ort => Ok(Box::new(ort_backend::OrtBackend::load(
            model_bytes,
            threads,
            None,
        )?)),
        #[cfg(feature = "tract")]
        Backend::Tract => Ok(Box::new(tract_backend::TractBackend::load(
            model_bytes,
            fixed_input,
        )?)),
        #[allow(unreachable_patterns)]
        other => Err(OcrError::inference(format!(
            "backend {other:?} is not compiled in (enable the matching cargo feature)"
        ))),
    }
}

/// Run a backend and return its output as a shape plus row-major flat data.
///
/// `DbNet`, `AngleNet`, and `CrnnNet` all post-process a single dense output buffer
/// (some by shape, some by flat data alone); this centralizes the [`Tensor`] ->
/// `(shape, data)` unpacking so each net's inference call site stays a one-liner.
pub(crate) fn run_flat(
    backend: &dyn ModelBackend,
    input: Tensor,
) -> Result<(Vec<usize>, Vec<f32>), OcrError> {
    let output = backend.run(input)?;
    let shape = output.shape().to_vec();
    let data = output.iter().copied().collect();
    Ok((shape, data))
}

#[cfg(test)]
mod tests {
    use ndarray::IxDyn;

    use super::*;

    #[derive(Debug)]
    struct StubBackend {
        output: Tensor,
    }

    impl ModelBackend for StubBackend {
        fn name(&self) -> &str {
            "stub"
        }

        fn run(&self, _input: Tensor) -> Result<Tensor, OcrError> {
            Ok(self.output.clone())
        }
    }

    #[test]
    fn run_flat_unpacks_shape_and_row_major_data() {
        let data: Vec<f32> = (0..6).map(|value| value as f32).collect();
        let output =
            ArrayD::from_shape_vec(IxDyn(&[2, 3]), data.clone()).expect("build the tensor");
        let backend = StubBackend { output };

        let (shape, flat) = run_flat(&backend, ArrayD::from_elem(IxDyn(&[1]), 0.0_f32))
            .expect("run the stub backend");

        assert_eq!(shape, vec![2_usize, 3]);
        assert_eq!(flat, data);
    }

    #[test]
    fn load_backend_errors_when_the_matching_feature_is_not_compiled_in() {
        #[cfg(not(feature = "tract"))]
        {
            let error = load_backend(Backend::Tract, &[], 1, None)
                .err()
                .expect("tract is not compiled in");
            assert!(matches!(error, OcrError::Inference { .. }));
            assert!(error.to_string().contains("not compiled in"));
        }
        #[cfg(not(feature = "ort"))]
        {
            let error = load_backend(Backend::Ort, &[], 1, None)
                .err()
                .expect("ort is not compiled in");
            assert!(matches!(error, OcrError::Inference { .. }));
            assert!(error.to_string().contains("not compiled in"));
        }
    }
}
