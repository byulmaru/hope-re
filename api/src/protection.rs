use crate::upstream::{algorithms, tiling, types};
use image::DynamicImage;
use ort::session::{builder::GraphOptimizationLevel, Session};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{cell::Cell, collections::HashMap, path::Path, rc::Rc};
use types::{AlgorithmParams, ProtectionState};

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

#[derive(Deserialize)]
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
        let specs: Vec<ModelSpec> = serde_json::from_str(include_str!("../models.json"))
            .map_err(|error| error.to_string())?;
        let mut sessions = HashMap::new();
        for spec in specs {
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
            let session = Session::builder()
                .map_err(|error| error.to_string())?
                .with_optimization_level(GraphOptimizationLevel::Level3)
                .map_err(|error| error.to_string())?
                .with_execution_providers([ort::ep::CPU::default().build().error_on_failure()])
                .map_err(|error| error.to_string())?
                .commit_from_memory(&bytes)
                .map_err(|error| error.to_string())?;
            sessions.insert(spec.algorithm, session);
        }
        Ok(Self(sessions))
    }

    pub fn protect(
        &mut self,
        image: &DynamicImage,
        options: &Options,
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
            &|_| {},
            &ProtectionState::default(),
        )?;
        Ok((image, calls.get()))
    }
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
    }
}
