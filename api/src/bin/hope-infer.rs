#[allow(dead_code)]
#[path = "../image_io.rs"]
mod image_io;
#[allow(dead_code)]
#[path = "../protection.rs"]
mod protection;
#[allow(dead_code, clippy::all)]
#[path = "../upstream.rs"]
mod upstream;

use image::ImageFormat;
use serde::{Deserialize, Serialize};
use signal_hook::{
    consts::signal::{SIGINT, SIGTERM},
    iterator::Signals,
};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::Path,
    sync::{atomic::Ordering, Arc},
};
use upstream::types::ProtectionState;

const MAX_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    mode: String,
    input_path: String,
    output_path: Option<String>,
    content_type: String,
    max_bytes: u64,
    max_dimension: u32,
    max_edge: u32,
    hope: Option<Hope>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Hope {
    algorithm: String,
    iterations: u32,
    intensity: f32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Failure {
    r#type: &'static str,
    message: &'static str,
    non_retryable: bool,
}

fn failure(kind: &'static str, message: &'static str, non_retryable: bool) -> Failure {
    Failure {
        r#type: kind,
        message,
        non_retryable,
    }
}

fn input_error() -> Failure {
    failure(
        "UploadProcessingInputError",
        "Hope processing input is invalid",
        true,
    )
}

fn validation_error() -> Failure {
    failure(
        "UploadValidationError",
        "Hope source image is invalid",
        true,
    )
}

fn processing_error(retryable: bool) -> Failure {
    failure("HopeProcessingError", "Hope processing failed", !retryable)
}

fn read_image(request: &Request) -> Result<Vec<u8>, Failure> {
    if request.max_bytes == 0
        || request.max_bytes > MAX_BYTES
        || request.max_dimension == 0
        || request.max_dimension > 8192
        || request.max_edge != 1024
    {
        return Err(input_error());
    }
    let file = File::open(&request.input_path).map_err(|_| processing_error(true))?;
    if file.metadata().map_err(|_| processing_error(true))?.len() > request.max_bytes {
        return Err(validation_error());
    }
    let mut bytes = Vec::new();
    file.take(request.max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| processing_error(true))?;
    if bytes.is_empty() || bytes.len() as u64 > request.max_bytes {
        return Err(validation_error());
    }
    let expected = match request.content_type.as_str() {
        "image/jpeg" => ImageFormat::Jpeg,
        "image/png" => ImageFormat::Png,
        "image/webp" => ImageFormat::WebP,
        _ => return Err(input_error()),
    };
    if image::guess_format(&bytes).ok() != Some(expected) {
        return Err(validation_error());
    }
    Ok(bytes)
}

fn run(request: Request, state: &ProtectionState) -> Result<serde_json::Value, Failure> {
    if request.mode != "protect" && request.mode != "validate" {
        return Err(input_error());
    }
    let bytes = read_image(&request)?;
    let dimension = if request.mode == "validate" {
        1024
    } else {
        request.max_dimension
    };
    let image = image_io::decode_limited(&bytes, 1024, dimension, 16 * 1024 * 1024)
        .map_err(|_| validation_error())?;
    if request.mode == "validate" {
        if request.content_type != "image/webp" {
            return Err(validation_error());
        }
        return Ok(
            serde_json::json!({"modelUsed": false, "calls": 0, "width": image.width(), "height": image.height()}),
        );
    }
    let hope = request.hope.ok_or_else(input_error)?;
    let options = protection::Options {
        algorithm: hope.algorithm,
        iterations: hope.iterations,
        intensity: hope.intensity,
        max_edge: 1024,
        quality: 80.0,
        glaze_style: None,
        nightshade_target: None,
    };
    options.validate().map_err(|_| input_error())?;
    let output_path = request.output_path.ok_or_else(input_error)?;
    let lock_path = std::env::var("HOPE_INFERENCE_LOCK").map_err(|_| processing_error(false))?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .map_err(|_| processing_error(true))?;
    lock.try_lock().map_err(|_| processing_error(true))?;
    let model_dir = std::env::var("HOPE_MODEL_DIR").map_err(|_| processing_error(false))?;
    let mut models =
        protection::Models::load(Path::new(&model_dir)).map_err(|_| processing_error(false))?;
    let (protected, calls) =
        models
            .protect_with_state(&image, &options, state)
            .map_err(|error| {
                processing_error(
                    ["memory", "allocation", "resource", "unavailable", "timeout"]
                        .iter()
                        .any(|word| error.to_lowercase().contains(word)),
                )
            })?;
    if state.is_cancelled.load(Ordering::SeqCst) || calls == 0 {
        return Err(processing_error(true));
    }
    let output = image_io::encode(&protected, 80.0).map_err(|_| processing_error(true))?;
    if output.len() as u64 > MAX_BYTES {
        return Err(processing_error(false));
    }
    let checked = image_io::decode_limited(&output, 1024, 1024, 1024 * 1024)
        .map_err(|_| processing_error(false))?;
    let mut file = File::create_new(output_path).map_err(|_| processing_error(true))?;
    file.write_all(&output)
        .and_then(|()| file.sync_all())
        .map_err(|_| processing_error(true))?;
    Ok(
        serde_json::json!({"modelUsed": true, "calls": calls, "width": checked.width(), "height": checked.height()}),
    )
}

fn main() {
    let state = Arc::new(ProtectionState::default());
    let signal_state = Arc::clone(&state);
    let result = (|| {
        let mut signals = Signals::new([SIGTERM, SIGINT]).map_err(|_| processing_error(false))?;
        std::thread::spawn(move || {
            if signals.forever().next().is_some() {
                signal_state.is_cancelled.store(true, Ordering::SeqCst);
            }
        });
        let mut input = Vec::new();
        std::io::stdin()
            .take(8193)
            .read_to_end(&mut input)
            .map_err(|_| input_error())?;
        if input.len() > 8192 {
            return Err(input_error());
        }
        let request = serde_json::from_slice(&input).map_err(|_| input_error())?;
        run(request, &state)
    })();
    let (output, status) = match result {
        Ok(value) if !state.is_cancelled.load(Ordering::SeqCst) => (value, 0),
        Ok(_) => (serde_json::json!(processing_error(true)), 130),
        Err(error) => (serde_json::json!(error), 1),
    };
    println!("{output}");
    std::process::exit(status);
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, RgbImage};
    use std::io::Cursor;

    #[test]
    fn media_limits_are_stricter_than_http_and_do_not_mask_large_results() {
        let source = DynamicImage::ImageRgb8(RgbImage::new(1025, 3));
        let bytes = image_io::encode(&source, 80.0).unwrap();
        assert!(image_io::decode_limited(&bytes, 1024, 1024, 1024 * 1024).is_err());
        let decoded = image_io::decode_limited(&bytes, 1024, 8192, 16 * 1024 * 1024).unwrap();
        assert_eq!(decoded.width(), 1024);
        assert!(image_io::decode_limited(&bytes, 1024, 8192, 100).is_err());
        assert!(image_io::decode(&bytes, 1280).is_ok());
    }

    #[test]
    fn exif_orientation_is_applied_without_upscaling() {
        let source = DynamicImage::ImageRgb8(RgbImage::new(32, 16));
        let mut encoded = Cursor::new(Vec::new());
        source.write_to(&mut encoded, ImageFormat::Jpeg).unwrap();
        let jpeg = encoded.into_inner();
        let mut oriented = jpeg[..2].to_vec();
        oriented.extend_from_slice(b"\xff\xe1\x00\x22Exif\x00\x00II\x2a\x00\x08\x00\x00\x00\x01\x00\x12\x01\x03\x00\x01\x00\x00\x00\x06\x00\x00\x00\x00\x00\x00\x00");
        oriented.extend_from_slice(&jpeg[2..]);
        let result = image_io::decode_limited(&oriented, 1024, 8192, 16 * 1024 * 1024).unwrap();
        assert_eq!((result.width(), result.height()), (16, 32));
    }

    #[test]
    fn failures_never_serialize_paths_or_provider_details() {
        assert_eq!(
            serde_json::to_value(processing_error(true)).unwrap(),
            serde_json::json!({
                "type": "HopeProcessingError", "message": "Hope processing failed", "nonRetryable": false
            })
        );
    }

    #[test]
    fn host_inference_lock_survives_a_second_file_handle() {
        let path = std::env::temp_dir().join(format!("hope-lock-test-{}", std::process::id()));
        let first = File::create(&path).unwrap();
        first.try_lock().unwrap();
        let second = File::open(&path).unwrap();
        assert!(second.try_lock().is_err());
        drop(first);
        assert!(second.try_lock().is_ok());
        drop(second);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    #[ignore = "requires pinned real models and HOPE_TEST_IMAGE PNG"]
    fn real_models_change_pixels_and_honor_cancellation() {
        let input_path = std::env::var("HOPE_TEST_IMAGE").unwrap();
        let source = std::fs::read(&input_path).unwrap();
        let baseline = image_io::decode_limited(&source, 1024, 8192, 16 * 1024 * 1024).unwrap();
        let baseline = baseline.to_rgba8();
        let state = ProtectionState::default();
        for algorithm in protection::ALGORITHMS {
            let output_path = std::env::temp_dir()
                .join(format!("hope-real-{}-{algorithm}.webp", std::process::id()));
            let request = Request {
                mode: "protect".into(),
                input_path: input_path.clone(),
                output_path: Some(output_path.to_str().unwrap().into()),
                content_type: "image/png".into(),
                max_bytes: MAX_BYTES,
                max_dimension: 8192,
                max_edge: 1024,
                hope: Some(Hope {
                    algorithm: algorithm.into(),
                    iterations: 2,
                    intensity: 0.3,
                }),
            };
            let started = std::time::Instant::now();
            let report = run(request, &state).unwrap();
            let output = std::fs::read(&output_path).unwrap();
            let decoded = image_io::decode_limited(&output, 1024, 1024, 1024 * 1024)
                .unwrap()
                .to_rgba8();
            assert_eq!(decoded.dimensions(), baseline.dimensions());
            let changed = decoded
                .pixels()
                .zip(baseline.pixels())
                .filter(|(left, right)| left != right)
                .count();
            assert!(changed > 0);
            assert!(report["calls"].as_u64().unwrap() > 0);
            assert_eq!(report["modelUsed"], true);
            println!(
                "{}",
                serde_json::json!({"algorithm": algorithm, "report": report, "changedPixels": changed, "bytes": output.len(), "ms": started.elapsed().as_millis()})
            );
            std::fs::remove_file(output_path).unwrap();
        }
        let model_dir = std::env::var("HOPE_MODEL_DIR").unwrap();
        let mut models = protection::Models::load(Path::new(&model_dir)).unwrap();
        state.is_cancelled.store(true, Ordering::SeqCst);
        let options = protection::Options {
            algorithm: "noise".into(),
            iterations: 2,
            intensity: 0.3,
            max_edge: 1024,
            quality: 80.0,
            glaze_style: None,
            nightshade_target: None,
        };
        let image = DynamicImage::ImageRgba8(baseline);
        assert!(models.protect_with_state(&image, &options, &state).is_err());
    }
}
