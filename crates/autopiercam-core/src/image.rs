#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BayerPattern {
    Rg,
    Bg,
    Gr,
    Gb,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Color {
    Red = 0,
    Green = 1,
    Blue = 2,
}

/// Bilinear demosaic from an 8-bit Bayer plane to interleaved RGB8.
///
/// Missing channels are averaged from same-color samples in the surrounding
/// 3x3 neighborhood. Edges use the neighbors that actually exist.
pub fn demosaic_bilinear(
    raw: &[u8],
    width: u32,
    height: u32,
    pattern: BayerPattern,
) -> Result<Vec<u8>, ImageError> {
    validate_raw8(raw, width, height)?;

    let mut rgb = allocate_rgb(width, height)?;
    for y in 0..height {
        for x in 0..width {
            let destination_index = (y as usize * width as usize + x as usize) * 3;
            demosaic_pixel(
                raw,
                width,
                height,
                x,
                y,
                pattern,
                &mut rgb[destination_index..destination_index + 3],
            );
        }
    }
    Ok(rgb)
}

/// Bilinearly demosaic a Bayer plane directly into an aspect-fitted RGB8 preview.
///
/// The result never exceeds the source or caller-provided bounds. Each output
/// pixel samples the center of its source region and performs Bayer interpolation
/// there, avoiding a full-resolution RGB allocation before downscaling.
pub fn demosaic_bilinear_preview(
    raw: &[u8],
    width: u32,
    height: u32,
    pattern: BayerPattern,
    max_width: u32,
    max_height: u32,
) -> Result<(u32, u32, Vec<u8>), ImageError> {
    validate_raw8(raw, width, height)?;
    if max_width == 0 || max_height == 0 {
        return Err(ImageError::InvalidPreviewBounds {
            max_width,
            max_height,
        });
    }

    let (preview_width, preview_height) = fit_dimensions(width, height, max_width, max_height);
    let mut data = allocate_rgb(preview_width, preview_height)?;
    for preview_y in 0..preview_height {
        let source_y = source_coordinate(preview_y, height, preview_height);
        for preview_x in 0..preview_width {
            let source_x = source_coordinate(preview_x, width, preview_width);
            let destination_index =
                (preview_y as usize * preview_width as usize + preview_x as usize) * 3;
            demosaic_pixel(
                raw,
                width,
                height,
                source_x,
                source_y,
                pattern,
                &mut data[destination_index..destination_index + 3],
            );
        }
    }

    Ok((preview_width, preview_height, data))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LumaStats {
    pub mean: f32,
    pub p50: u8,
    pub p90: u8,
    pub clipped_fraction: f32,
}

/// Computes intensity statistics directly on a raw 8-bit plane without
/// debayering. This is the low-cost signal used while auto exposure settles.
pub fn raw8_stats(raw: &[u8], sample_stride_pixels: usize) -> Result<LumaStats, ImageError> {
    let stride = sample_stride_pixels.max(1);
    let mut histogram = [0_u64; 256];
    let mut sum = 0_u64;
    let mut count = 0_u64;
    for value in raw.iter().step_by(stride) {
        histogram[*value as usize] += 1;
        sum += *value as u64;
        count += 1;
    }
    stats_from_histogram(&histogram, sum, count)
}

pub fn luma_stats(rgb: &[u8], sample_stride_pixels: usize) -> Result<LumaStats, ImageError> {
    if !rgb.len().is_multiple_of(3) {
        return Err(ImageError::InvalidRgbLength(rgb.len()));
    }
    let stride = sample_stride_pixels.max(1);
    let mut histogram = [0_u64; 256];
    let mut sum = 0_u64;
    let mut count = 0_u64;
    for pixel in rgb.chunks_exact(3).step_by(stride) {
        // Integer Rec. 709 approximation is sufficient for exposure feedback.
        let luma = ((54_u32 * pixel[0] as u32
            + 183_u32 * pixel[1] as u32
            + 19_u32 * pixel[2] as u32)
            >> 8) as u8;
        histogram[luma as usize] += 1;
        sum += luma as u64;
        count += 1;
    }
    stats_from_histogram(&histogram, sum, count)
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum ImageError {
    #[error("image dimensions overflow addressable memory")]
    DimensionsOverflow,
    #[error("image dimensions must be non-zero")]
    EmptyImage,
    #[error("preview bounds must be non-zero, got {max_width}x{max_height}")]
    InvalidPreviewBounds { max_width: u32, max_height: u32 },
    #[error("raw buffer length was {actual}, expected {expected}")]
    BufferLength { expected: usize, actual: usize },
    #[error("RGB buffer length {0} is not divisible by three")]
    InvalidRgbLength(usize),
}

fn validate_raw8(raw: &[u8], width: u32, height: u32) -> Result<(), ImageError> {
    let expected = (width as usize)
        .checked_mul(height as usize)
        .ok_or(ImageError::DimensionsOverflow)?;
    if raw.len() != expected {
        return Err(ImageError::BufferLength {
            expected,
            actual: raw.len(),
        });
    }
    if width == 0 || height == 0 {
        return Err(ImageError::EmptyImage);
    }
    Ok(())
}

fn allocate_rgb(width: u32, height: u32) -> Result<Vec<u8>, ImageError> {
    let length = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or(ImageError::DimensionsOverflow)?;
    Ok(vec![0_u8; length])
}

fn fit_dimensions(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    if width <= max_width && height <= max_height {
        return (width, height);
    }

    if u64::from(max_width) * u64::from(height) <= u64::from(max_height) * u64::from(width) {
        let preview_width = width.min(max_width);
        let preview_height = scaled_dimension(height, width, preview_width).min(max_height);
        (preview_width, preview_height)
    } else {
        let preview_height = height.min(max_height);
        let preview_width = scaled_dimension(width, height, preview_height).min(max_width);
        (preview_width, preview_height)
    }
}

fn scaled_dimension(source: u32, other_source: u32, other_preview: u32) -> u32 {
    let numerator = u64::from(source) * u64::from(other_preview);
    let rounded = numerator.saturating_add(u64::from(other_source) / 2) / u64::from(other_source);
    u32::try_from(rounded).unwrap_or(u32::MAX).max(1)
}

fn source_coordinate(coordinate: u32, source_extent: u32, preview_extent: u32) -> u32 {
    if source_extent == preview_extent {
        return coordinate;
    }
    let centered = (u128::from(coordinate) * 2 + 1) * u128::from(source_extent);
    let coordinate = centered / (u128::from(preview_extent) * 2);
    u32::try_from(coordinate)
        .unwrap_or(u32::MAX)
        .min(source_extent - 1)
}

#[allow(clippy::too_many_arguments)]
fn demosaic_pixel(
    raw: &[u8],
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    pattern: BayerPattern,
    destination: &mut [u8],
) {
    let source_index = y as usize * width as usize + x as usize;
    let native_color = color_at(x, y, pattern);
    for requested in [Color::Red, Color::Green, Color::Blue] {
        destination[requested as usize] = if requested == native_color {
            raw[source_index]
        } else {
            neighbor_average(raw, width, height, x, y, pattern, requested)
        };
    }
}

fn color_at(x: u32, y: u32, pattern: BayerPattern) -> Color {
    let even_x = x.is_multiple_of(2);
    let even_y = y.is_multiple_of(2);
    match (pattern, even_x, even_y) {
        (BayerPattern::Rg, true, true) | (BayerPattern::Gr, false, true) => Color::Red,
        (BayerPattern::Rg, false, false) | (BayerPattern::Gr, true, false) => Color::Blue,
        (BayerPattern::Bg, true, true) | (BayerPattern::Gb, false, true) => Color::Blue,
        (BayerPattern::Bg, false, false) | (BayerPattern::Gb, true, false) => Color::Red,
        _ => Color::Green,
    }
}

fn neighbor_average(
    raw: &[u8],
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    pattern: BayerPattern,
    requested: Color,
) -> u8 {
    let mut sum = 0_u32;
    let mut count = 0_u32;
    let x_min = x.saturating_sub(1);
    let x_max = x.saturating_add(1).min(width - 1);
    let y_min = y.saturating_sub(1);
    let y_max = y.saturating_add(1).min(height - 1);
    for neighbor_y in y_min..=y_max {
        for neighbor_x in x_min..=x_max {
            if color_at(neighbor_x, neighbor_y, pattern) == requested {
                sum += raw[neighbor_y as usize * width as usize + neighbor_x as usize] as u32;
                count += 1;
            }
        }
    }
    (sum + count / 2)
        .checked_div(count)
        .map(|average| average as u8)
        .unwrap_or(raw[y as usize * width as usize + x as usize])
}

fn percentile(histogram: &[u64; 256], count: u64, fraction: f64) -> u8 {
    let target = ((count as f64 * fraction).ceil() as u64).max(1);
    let mut cumulative = 0_u64;
    for (value, occurrences) in histogram.iter().enumerate() {
        cumulative += occurrences;
        if cumulative >= target {
            return value as u8;
        }
    }
    u8::MAX
}

fn stats_from_histogram(
    histogram: &[u64; 256],
    sum: u64,
    count: u64,
) -> Result<LumaStats, ImageError> {
    if count == 0 {
        return Err(ImageError::EmptyImage);
    }
    Ok(LumaStats {
        mean: sum as f32 / count as f32,
        p50: percentile(histogram, count, 0.50),
        p90: percentile(histogram, count, 0.90),
        clipped_fraction: histogram[250..].iter().sum::<u64>() as f32 / count as f32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniform_bayer_image_remains_neutral() {
        let raw = vec![73; 16];
        let rgb = demosaic_bilinear(&raw, 4, 4, BayerPattern::Rg).unwrap();
        assert_eq!(rgb, vec![73; 48]);
    }

    #[test]
    fn native_channels_are_preserved_for_all_patterns() {
        for pattern in [
            BayerPattern::Rg,
            BayerPattern::Bg,
            BayerPattern::Gr,
            BayerPattern::Gb,
        ] {
            let raw = vec![10, 20, 30, 40];
            let rgb = demosaic_bilinear(&raw, 2, 2, pattern).unwrap();
            for y in 0..2 {
                for x in 0..2 {
                    let source = (y * 2 + x) as usize;
                    let channel = color_at(x, y, pattern) as usize;
                    assert_eq!(rgb[source * 3 + channel], raw[source]);
                }
            }
        }
    }

    #[test]
    fn identity_preview_matches_full_demosaic() {
        let raw = (0_u8..48).collect::<Vec<_>>();
        for pattern in [
            BayerPattern::Rg,
            BayerPattern::Bg,
            BayerPattern::Gr,
            BayerPattern::Gb,
        ] {
            let full = demosaic_bilinear(&raw, 8, 6, pattern).unwrap();
            let (width, height, preview) =
                demosaic_bilinear_preview(&raw, 8, 6, pattern, 20, 20).unwrap();
            assert_eq!((width, height), (8, 6));
            assert_eq!(preview, full);
        }
    }

    #[test]
    fn preview_fits_bounds_without_upscaling_and_preserves_aspect() {
        let raw = vec![42; 400 * 200];

        let (width, height, _) =
            demosaic_bilinear_preview(&raw, 400, 200, BayerPattern::Rg, 100, 100).unwrap();
        assert_eq!((width, height), (100, 50));

        let (width, height, _) =
            demosaic_bilinear_preview(&raw, 400, 200, BayerPattern::Rg, 300, 75).unwrap();
        assert_eq!((width, height), (150, 75));

        let (width, height, _) =
            demosaic_bilinear_preview(&raw, 400, 200, BayerPattern::Rg, 800, 800).unwrap();
        assert_eq!((width, height), (400, 200));
    }

    #[test]
    fn downscaled_uniform_preview_is_neutral_for_every_bayer_pattern() {
        let raw = vec![73; 48 * 32];
        for pattern in [
            BayerPattern::Rg,
            BayerPattern::Bg,
            BayerPattern::Gr,
            BayerPattern::Gb,
        ] {
            let (width, height, preview) =
                demosaic_bilinear_preview(&raw, 48, 32, pattern, 13, 11).unwrap();
            assert_eq!((width, height), (13, 9));
            assert_eq!(preview, vec![73; 13 * 9 * 3]);
        }
    }

    #[test]
    fn preview_rejects_invalid_bounds_and_buffer_lengths() {
        assert_eq!(
            demosaic_bilinear_preview(&[0; 4], 2, 2, BayerPattern::Rg, 0, 2),
            Err(ImageError::InvalidPreviewBounds {
                max_width: 0,
                max_height: 2,
            })
        );
        assert_eq!(
            demosaic_bilinear_preview(&[0; 3], 2, 2, BayerPattern::Rg, 2, 2),
            Err(ImageError::BufferLength {
                expected: 4,
                actual: 3,
            })
        );
    }

    #[test]
    fn luminance_statistics_are_stable() {
        let rgb = [0, 0, 0, 100, 100, 100, 200, 200, 200, 255, 255, 255];
        let stats = luma_stats(&rgb, 1).unwrap();
        assert_eq!(stats.p50, 100);
        assert_eq!(stats.p90, 255);
        assert_eq!(stats.clipped_fraction, 0.25);
    }

    #[test]
    fn raw_statistics_can_be_sampled_without_debayering() {
        let raw = [0, 10, 20, 30, 40, 50, 60, 255];
        let stats = raw8_stats(&raw, 2).unwrap();
        assert_eq!(stats.p50, 20);
        assert_eq!(stats.p90, 60);
        assert_eq!(stats.clipped_fraction, 0.0);
    }
}
