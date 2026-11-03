use crate::upstream::{algorithms, tiling, types};
use image::DynamicImage;
use ort::session::{builder::GraphOptimizationLevel, Session};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{cell::Cell, collections::HashMap, path::Path, rc::Rc};
use types::{AlgorithmParams, ProtectionProgress, ProtectionState};

pub const ALGORITHMS: [&str; 3] = ["noise", "glaze", "nightshade"];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Options {
    pub algorithm: String,
    pub iterations: u32,
    #[serde(default = "default_intensity")]
    pub intensity: f32,
    #[serde(default = "default_edge")]
    pub max_edge: u32,
    #[serde(default = "default_quality")]
    pub quality: f32,
    pub glaze_style: Option<String>,
    pub nightshade_target: Option<String>,
}

fn default_intensity() -> f32 {
    0.3
}
fn default_edge() -> u32 {
    1280
}
fn default_quality() -> f32 {
    80.0
}

impl Options {
    pub fn validate(&self) -> Result<AlgorithmParams, String> {
        if !self.intensity.is_finite()
            || !(0.0..=1.0).contains(&self.intensity)
            || self.intensity == 0.0
        {
            return Err("intensity must be finite and in (0, 1]".into());
        }
        let params = match self.algorithm.as_str() {
            "noise" => algorithms::get_noise_params(self.intensity),
            "glaze" => algorithms::get_glaze_params(self.intensity),
            "nightshade" => algorithms::get_nightshade_params(self.intensity),
            _ => return Err("algorithm must be noise, glaze, or nightshade".into()),
        };
        if self.iterations == 0 || self.iterations > params.max_iterations {
            return Err(format!(
                "iterations must be in 1..={}",
                params.max_iterations
            ));
        }
        if !(1..=4096).contains(&self.max_edge)
            || !self.quality.is_finite()
            || !(0.0..=100.0).contains(&self.quality)
        {
            return Err("max_edge must be in 1..=4096 and quality in 0..=100".into());
        }
        if !(params.epsilon * 0.1 / (self.iterations as f32).powf(0.101)).is_normal() {
            return Err("intensity is too small for stable f32 SPSA".into());
        }
        if let Some(style) = &self.glaze_style {
            if self.algorithm != "glaze"
                || ![
                    "abstract",
                    "impressionist",
                    "cubist",
                    "sketch",
                    "watercolor",
                ]
                .contains(&style.as_str())
            {
                return Err("glaze_style is invalid for this algorithm".into());
            }
        }
        if let Some(target) = &self.nightshade_target {
            if self.algorithm != "nightshade"
                || ![
                    "dog",
                    "cat",
                    "car",
                    "landscape",
                    "person",
                    "building",
                    "food",
                    "abstract",
                ]
                .contains(&target.as_str())
            {
                return Err("nightshade_target is invalid for this algorithm".into());
            }
        }
        Ok(params)
    }
}

#[derive(Clone, Deserialize)]
struct ModelSpec {
    algorithm: String,
    filename: String,
    bytes: u64,
    sha256: String,
}

pub struct Models(HashMap<String, Session>);

impl Models {
    #[cfg(test)]
    pub fn empty_for_test() -> Self {
        Self(HashMap::new())
    }

    pub fn load(directory: &Path) -> Result<Self, String> {
        Self::load_selected(directory, None)
    }

    pub fn load_for(directory: &Path, algorithm: &str) -> Result<Self, String> {
        Self::load_selected(directory, Some(algorithm))
    }

    pub fn load_glaze_int8(directory: &Path) -> Result<Self, String> {
        let spec = parse_glaze_int8_spec(include_str!("../models-int8.json"))?;
        let bytes = verified_model_bytes(directory, &spec)?;
        let session = load_model_session(&bytes)?;
        eprintln!(
            "Hope API selected Glaze variant=int8 model={} sha256={}",
            spec.filename, spec.sha256
        );
        Ok(Self(HashMap::from([("glaze".to_string(), session)])))
    }

    fn load_selected(directory: &Path, selected_algorithm: Option<&str>) -> Result<Self, String> {
        let specs = parse_model_specs(include_str!("../models.json"))?;
        if let Some(algorithm) = selected_algorithm {
            if !specs
                .iter()
                .any(|spec| spec.algorithm.as_str() == algorithm)
            {
                return Err(format!("Unknown model algorithm: {algorithm}"));
            }
        }
        let mut sessions = HashMap::new();
        for spec in specs {
            let bytes = verified_model_bytes(directory, &spec)?;
            if selected_algorithm.is_some_and(|algorithm| algorithm != spec.algorithm.as_str()) {
                continue;
            }
            let session = load_model_session(&bytes)?;
            sessions.insert(spec.algorithm, session);
        }
        Ok(Self(sessions))
    }

    pub fn protect_with_state(
        &mut self,
        image: &DynamicImage,
        options: &Options,
        state: &ProtectionState,
    ) -> Result<(DynamicImage, u64), String> {
        self.protect_with_state_and_progress(image, options, state, &|_| {})
    }

    pub fn protect_with_state_and_progress(
        &mut self,
        image: &DynamicImage,
        options: &Options,
        state: &ProtectionState,
        emit: &dyn Fn(ProtectionProgress),
    ) -> Result<(DynamicImage, u64), String> {
        let params = options.validate()?;
        let session = self
            .0
            .get_mut(&options.algorithm)
            .ok_or("Model not loaded")?;
        let algorithm = options.algorithm.clone();
        let style =
            algorithms::get_glaze_style_index(options.glaze_style.as_deref().unwrap_or("abstract"));
        let target = algorithms::get_nightshade_target_index(
            options.nightshade_target.as_deref().unwrap_or("dog"),
        );
        let calls = Rc::new(Cell::new(0_u64));
        let inference_calls = Rc::clone(&calls);
        let image = tiling::apply_model_protection(
            image,
            session,
            &params,
            options.iterations,
            &mut move |session, input| {
                inference_calls.set(inference_calls.get() + 1);
                match algorithm.as_str() {
                    "noise" => algorithms::run_noise_model(session, input),
                    "glaze" => algorithms::run_glaze_model(session, input, style),
                    _ => algorithms::run_nightshade_model(session, input, target),
                }
                .and_then(|loss| {
                    if loss.is_finite() {
                        Ok(loss)
                    } else {
                        Err("Non-finite model loss".into())
                    }
                })
            },
            emit,
            state,
        )?;
        Ok((image, calls.get()))
    }
}

fn parse_model_specs(manifest: &str) -> Result<Vec<ModelSpec>, String> {
    serde_json::from_str(manifest).map_err(|error| error.to_string())
}

fn parse_glaze_int8_spec(manifest: &str) -> Result<ModelSpec, String> {
    let mut specs = parse_model_specs(manifest)?;
    if specs.len() != 1 {
        return Err("INT8 model manifest must contain exactly one Glaze model".into());
    }
    let spec = specs.remove(0);
    if spec.algorithm != "glaze" || spec.filename != "glaze_algorithm.int8.onnx" {
        return Err("INT8 model manifest contains an invalid Glaze model pin".into());
    }
    Ok(spec)
}

fn verified_model_bytes(directory: &Path, spec: &ModelSpec) -> Result<Vec<u8>, String> {
    let path = directory.join(&spec.filename);
    if std::fs::metadata(&path)
        .map_err(|error| format!("{}: {error}", path.display()))?
        .len()
        != spec.bytes
    {
        return Err(format!(
            "{}: model size mismatch; LFS pointers are not weights",
            path.display()
        ));
    }
    let bytes = std::fs::read(&path).map_err(|error| error.to_string())?;
    if format!("{:x}", Sha256::digest(&bytes)) != spec.sha256 {
        return Err(format!("{}: model SHA-256 mismatch", path.display()));
    }
    Ok(bytes)
}

fn load_model_session(bytes: &[u8]) -> Result<Session, String> {
    Session::builder()
        .map_err(|error| error.to_string())?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(|error| error.to_string())?
        .with_execution_providers([ort::ep::CPU::default().build().error_on_failure()])
        .map_err(|error| error.to_string())?
        .commit_from_memory(bytes)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_options_and_missing_models() {
        let mut options: Options =
            serde_json::from_str(r#"{"algorithm":"noise","iterations":2}"#).unwrap();
        assert!(options.validate().is_ok());
        options.glaze_style = Some("abstract".into());
        assert!(options.validate().is_err());
        options.glaze_style = None;
        for intensity in [0.0, f32::NAN, f32::INFINITY, 1e-40] {
            options.intensity = intensity;
            assert!(options.validate().is_err());
        }
        options.intensity = 0.3;
        options.iterations = 501;
        assert!(options.validate().is_err());
        assert!(Models::load(Path::new("nonexistent-model-directory")).is_err());
        assert_eq!(
            Models::load_for(Path::new("nonexistent-model-directory"), "unknown")
                .err()
                .as_deref(),
            Some("Unknown model algorithm: unknown")
        );
    }

    #[test]
    fn int8_loader_fails_closed_when_selected_model_is_unavailable() {
        assert!(Models::load_glaze_int8(Path::new("nonexistent-model-directory")).is_err());
    }

    #[test]
    fn glaze_int8_manifest_requires_one_pinned_glaze_model() {
        let manifest = r#"[{"algorithm":"glaze","filename":"glaze_algorithm.int8.onnx","bytes":2048,"sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]"#;
        let spec = parse_glaze_int8_spec(manifest).unwrap();
        assert_eq!(spec.algorithm, "glaze");
        assert_eq!(spec.filename, "glaze_algorithm.int8.onnx");
        assert!(parse_glaze_int8_spec("[]").is_err());
        assert!(parse_glaze_int8_spec(
            r#"[{"algorithm":"noise","filename":"glaze_algorithm.int8.onnx","bytes":2048,"sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]"#
        )
        .is_err());
    }

    #[test]
    fn verified_model_bytes_rejects_missing_and_mismatched_pins() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let directory = std::env::temp_dir().join(format!(
            "hope-api-model-pin-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("model.onnx");
        let expected = b"pinned model bytes";
        std::fs::write(&path, expected).unwrap();
        let spec = ModelSpec {
            algorithm: "glaze".into(),
            filename: "model.onnx".into(),
            bytes: expected.len() as u64,
            sha256: format!("{:x}", Sha256::digest(expected)),
        };
        assert_eq!(verified_model_bytes(&directory, &spec).unwrap(), expected);
        let bad_size_spec = ModelSpec {
            bytes: spec.bytes + 1,
            ..spec.clone()
        };
        assert!(verified_model_bytes(&directory, &bad_size_spec).is_err());
        let bad_spec = ModelSpec {
            sha256: "0".repeat(64),
            ..spec
        };
        assert!(verified_model_bytes(&directory, &bad_spec).is_err());
        std::fs::remove_file(path).unwrap();
        assert!(verified_model_bytes(&directory, &bad_spec).is_err());
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    #[ignore = "requires official weights in HOPE_MODEL_DIR"]
    fn loads_only_the_requested_model_session() {
        let directory = std::env::var("HOPE_MODEL_DIR").unwrap();
        let models = Models::load_for(Path::new(&directory), "nightshade").unwrap();
        assert_eq!(models.0.len(), 1);
        assert!(models.0.contains_key("nightshade"));
    }
}
