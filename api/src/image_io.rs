use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
use std::io::Cursor;

pub fn decode(bytes: &[u8], max_edge: u32) -> Result<DynamicImage, String> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| error.to_string())?;
    match reader.format() {
        Some(ImageFormat::Png) => {
            let decoder = image::codecs::png::PngDecoder::new(Cursor::new(bytes))
                .map_err(|error| error.to_string())?;
            if decoder.is_apng().map_err(|error| error.to_string())? {
                return Err("Animated PNG is not supported".into());
            }
        }
        Some(ImageFormat::WebP) => {
            let decoder = image::codecs::webp::WebPDecoder::new(Cursor::new(bytes))
                .map_err(|error| error.to_string())?;
            if decoder.has_animation() {
                return Err("Animated WebP is not supported".into());
            }
        }
        Some(ImageFormat::Jpeg) => {}
        _ => return Err("Input must be a still JPEG, PNG, or WebP".into()),
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(|error| error.to_string())?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 40_000_000 {
        return Err("Input must contain 1..=40 million pixels".into());
    }
    let orientation = decoder.orientation().map_err(|error| error.to_string())?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(|error| error.to_string())?;
    image.apply_orientation(orientation);
    if image.width() > max_edge || image.height() > max_edge {
        image = image.resize(max_edge, max_edge, image::imageops::FilterType::Lanczos3);
    }
    Ok(image)
}

pub fn encode(image: &DynamicImage, quality: f32) -> Result<Vec<u8>, String> {
    let rgba = image.to_rgba8();
    webp::Encoder::from_rgba(rgba.as_raw(), rgba.width(), rgba.height())
        .encode_simple(false, quality)
        .map(|encoded| encoded.to_vec())
        .map_err(|error| format!("WebP encoding failed: {error:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webp_preserves_alpha_and_resize_never_upscales() {
        let image = DynamicImage::ImageRgba8(image::RgbaImage::from_fn(64, 48, |column, row| {
            image::Rgba([column as u8, row as u8, 180, 200])
        }));
        let webp = encode(&image, 80.0).unwrap();
        assert_eq!(&webp[8..12], b"WEBP");
        let decoded = decode(&webp, 128).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (64, 48));
        assert!(decoded.to_rgba8().pixels().all(|pixel| pixel[3] == 200));
        let resized = decode(&webp, 32).unwrap();
        assert_eq!((resized.width(), resized.height()), (32, 24));
        assert!(decode(b"invalid image", 1280).is_err());
    }
}
