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

    let mut rgb = vec![0_u8; expected * 3];
    for y in 0..height {
        for x in 0..width {
            let source_index = (y * width + x) as usize;
            let destination_index = source_index * 3;
            let native_color = color_at(x, y, pattern);
            for requested in [Color::Red, Color::Green, Color::Blue] {
                rgb[destination_index + requested as usize] = if requested == native_color {
                    raw[source_index]
                } else {
                    neighbor_average(raw, width, height, x, y, pattern, requested)
                };
            }
        }
    }
    Ok(rgb)
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
    #[error("raw buffer length was {actual}, expected {expected}")]
    BufferLength { expected: usize, actual: usize },
    #[error("RGB buffer length {0} is not divisible by three")]
    InvalidRgbLength(usize),
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
    let x_max = (x + 1).min(width - 1);
    let y_min = y.saturating_sub(1);
    let y_max = (y + 1).min(height - 1);
    for neighbor_y in y_min..=y_max {
        for neighbor_x in x_min..=x_max {
            if color_at(neighbor_x, neighbor_y, pattern) == requested {
                sum += raw[(neighbor_y * width + neighbor_x) as usize] as u32;
                count += 1;
            }
        }
    }
    (sum + count / 2)
        .checked_div(count)
        .map(|average| average as u8)
        .unwrap_or(raw[(y * width + x) as usize])
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
