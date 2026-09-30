use crate::{MAX_IMAGE_BYTES, model::ModelSpec};
use anyhow::{Result, ensure};
use image::{
    DynamicImage, ImageReader, RgbImage,
    imageops::{self, FilterType},
};
use serde::{Deserialize, Serialize};
use std::io::Cursor;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Roi {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}
impl Default for Roi {
    fn default() -> Self {
        Self {
            left: 0.0,
            top: 0.0,
            right: 1.0,
            bottom: 1.0,
        }
    }
}
impl Roi {
    pub fn validate(self) -> Result<()> {
        ensure!(
            [self.left, self.top, self.right, self.bottom]
                .iter()
                .all(|v| v.is_finite() && (0.0..=1.0).contains(v))
                && self.left < self.right
                && self.top < self.bottom,
            "ROI must be a nonempty normalized rectangle"
        );
        Ok(())
    }
}

pub fn decode(bytes: &[u8]) -> Result<DynamicImage> {
    ensure!(bytes.len() as u64 <= MAX_IMAGE_BYTES, "Image too large");
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    ensure!(
        matches!(
            reader.format(),
            Some(image::ImageFormat::Jpeg | image::ImageFormat::Png)
        ),
        "Only JPEG and PNG are supported"
    );
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    Ok(reader.decode()?)
}

pub fn prepare(bytes: &[u8], spec: &ModelSpec, roi: Roi) -> Result<Vec<f32>> {
    spec.validate()?;
    roi.validate()?;
    let decoded = decode(bytes)?;
    let x = (roi.left * decoded.width() as f32).floor() as u32;
    let y = (roi.top * decoded.height() as f32).floor() as u32;
    let right = (roi.right * decoded.width() as f32).ceil() as u32;
    let bottom = (roi.bottom * decoded.height() as f32).ceil() as u32;
    let resized = decoded
        .crop_imm(x, y, right - x, bottom - y)
        .resize(spec.width, spec.height, FilterType::Triangle)
        .to_rgb8();
    let mut padded = RgbImage::new(spec.width, spec.height);
    imageops::replace(
        &mut padded,
        &resized,
        i64::from((spec.width - resized.width()) / 2),
        i64::from((spec.height - resized.height()) / 2),
    );
    let plane = (spec.width * spec.height) as usize;
    let mut input = vec![0.0; plane * 3];
    for (i, pixel) in padded.pixels().enumerate() {
        for c in 0..3 {
            input[c * plane + i] = (f32::from(pixel[c]) / 255.0 - spec.mean[c]) / spec.std[c];
        }
    }
    ensure!(
        input.iter().all(|v| v.is_finite()),
        "Normalization produced non-finite input"
    );
    Ok(input)
}

#[cfg(test)]
pub(crate) fn png(width: u32, height: u32, color: [u8; 3]) -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    RgbImage::from_pixel(width, height, image::Rgb(color))
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rgb_nchw_and_letterbox_match_contract() {
        let spec = crate::model::test_spec();
        let input = prepare(&png(32, 16, [255, 128, 0]), &spec, Roi::default()).unwrap();
        assert_eq!(input.len(), 3 * 16 * 16);
        assert_eq!(input[0], 0.0); // black top padding
        assert_eq!(input[4 * 16], 1.0);
        assert!((input[256 + 4 * 16] - 128.0 / 255.0).abs() < 1e-6);
        assert_eq!(input[512 + 4 * 16], 0.0);
    }
    #[test]
    fn bounded_decode_and_roi_validation() {
        let spec = crate::model::test_spec();
        assert!(decode(b"partial image").is_err());
        assert!(
            prepare(
                &png(1, 1, [0; 3]),
                &spec,
                Roi {
                    right: 0.0,
                    ..Default::default()
                }
            )
            .is_err()
        );
        assert!(
            Roi {
                left: f32::NAN,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            prepare(
                &png(1, 1, [255; 3]),
                &spec,
                Roi {
                    left: 0.99,
                    ..Default::default()
                }
            )
            .is_ok()
        );
    }
}
