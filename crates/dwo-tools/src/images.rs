//! Image normalization for model context: downscale previews to the provider
//! budget and re-encode as lossless WebP before the bytes enter a request.

use anyhow::{Context, Result, bail};

/// Providers downscale previews to roughly 1300x1300 pixels of total area;
/// anything larger is invisible to the model while still riding along in
/// every request body.
pub const MAX_IMAGE_PIXELS: u64 = 1300 * 1300;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedImage {
    pub mime_type: &'static str,
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub original_width: u32,
    pub original_height: u32,
    pub resized: bool,
}

/// Decode an image, downscale it into the preview budget when needed, and
/// re-encode it as lossless WebP.
pub fn normalize_image(bytes: &[u8]) -> Result<NormalizedImage> {
    let decoded = image::load_from_memory(bytes).context("decode image")?;
    let original_width = decoded.width();
    let original_height = decoded.height();
    if original_width == 0 || original_height == 0 {
        bail!("image has no pixels");
    }

    let (resized, preview) =
        if u64::from(original_width) * u64::from(original_height) > MAX_IMAGE_PIXELS {
            let scale = (MAX_IMAGE_PIXELS as f64
                / (f64::from(original_width) * f64::from(original_height)))
            .sqrt();
            let width = ((f64::from(original_width) * scale).round() as u32).max(1);
            let height = ((f64::from(original_height) * scale).round() as u32).max(1);
            (
                true,
                decoded.resize(width, height, image::imageops::FilterType::Lanczos3),
            )
        } else {
            (false, decoded)
        };

    let (data, width, height) = encode_lossless_webp(&preview)?;
    Ok(NormalizedImage {
        mime_type: "image/webp",
        data,
        width,
        height,
        original_width,
        original_height,
        resized,
    })
}

fn encode_lossless_webp(source: &image::DynamicImage) -> Result<(Vec<u8>, u32, u32)> {
    let mut data = Vec::new();
    if source.color().has_alpha() {
        let rgba = source.to_rgba8();
        let (width, height) = (rgba.width(), rgba.height());
        image_webp::WebPEncoder::new(&mut data)
            .encode(rgba.as_raw(), width, height, image_webp::ColorType::Rgba8)
            .map_err(|error| anyhow::anyhow!("encode webp: {error}"))?;
        Ok((data, width, height))
    } else {
        let rgb = source.to_rgb8();
        let (width, height) = (rgb.width(), rgb.height());
        image_webp::WebPEncoder::new(&mut data)
            .encode(rgb.as_raw(), width, height, image_webp::ColorType::Rgb8)
            .map_err(|error| anyhow::anyhow!("encode webp: {error}"))?;
        Ok((data, width, height))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_png(width: u32, height: u32) -> Vec<u8> {
        let source = image::RgbaImage::from_fn(width, height, |x, y| {
            image::Rgba([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8, 255])
        });
        let mut png = Vec::new();
        source
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        png
    }

    #[test]
    fn large_images_are_downscaled_within_the_preview_budget() {
        let png = encode_png(2600, 1300);
        let normalized = normalize_image(&png).unwrap();

        assert_eq!(normalized.mime_type, "image/webp");
        assert!(normalized.resized);
        assert!(
            u64::from(normalized.width) * u64::from(normalized.height) <= MAX_IMAGE_PIXELS + 4096
        );
        assert_eq!(&normalized.data[0..4], b"RIFF");
        assert_eq!(&normalized.data[8..12], b"WEBP");

        let decoded = image::load_from_memory(&normalized.data).unwrap();
        assert_eq!(decoded.width(), normalized.width);
        assert_eq!(decoded.height(), normalized.height);
    }

    #[test]
    fn small_images_keep_their_dimensions_and_become_webp() {
        let png = encode_png(64, 48);
        let normalized = normalize_image(&png).unwrap();

        assert!(!normalized.resized);
        assert_eq!((normalized.width, normalized.height), (64, 48));
        assert_eq!(
            (normalized.original_width, normalized.original_height),
            (64, 48)
        );
        assert_eq!(&normalized.data[8..12], b"WEBP");
    }

    #[test]
    fn non_image_bytes_are_rejected() {
        assert!(normalize_image(b"not an image at all").is_err());
    }
}
