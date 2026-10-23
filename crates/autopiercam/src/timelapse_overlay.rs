//! Memory-only GDI text rendering using the installed Windows font. No desktop
//! window, camera, font download or extra bundled font license is involved.
use anyhow::Result;
use autopiercam_chatstronomy::observation::Overlay;
use image::RgbImage;

#[cfg(not(windows))]
pub(crate) fn draw(_canvas: &mut RgbImage, _overlay: &Overlay) -> Result<()> {
    anyhow::bail!("Timelapse overlays require Windows")
}

#[cfg(windows)]
pub(crate) fn draw(canvas: &mut RgbImage, overlay: &Overlay) -> Result<()> {
    use anyhow::ensure;
    use std::ptr::null_mut;
    use windows_sys::Win32::{Foundation::RECT, Graphics::Gdi::*};
    if overlay.lines.is_empty() {
        return Ok(());
    }
    ensure!(
        overlay.lines.len() <= 2 && overlay.lines.iter().all(|s| s.chars().count() <= 160),
        "oversized overlay"
    );
    let (width, height) = (canvas.width(), 72_u32);
    ensure!(
        width == 1280 && canvas.height() >= height,
        "invalid overlay canvas"
    );
    // Drop the memory DC first to deselect objects, then free the objects.
    struct Resources {
        dc: HDC,
        bitmap: HBITMAP,
        font: HFONT,
    }
    impl Drop for Resources {
        fn drop(&mut self) {
            unsafe {
                if !self.dc.is_null() {
                    DeleteDC(self.dc);
                }
                if !self.bitmap.is_null() {
                    DeleteObject(self.bitmap);
                }
                if !self.font.is_null() {
                    DeleteObject(self.font);
                }
            }
        }
    }
    // SAFETY: the DIB allocation is exactly width*height*4, all GDI handles
    // live under one RAII owner, and no borrowed pixel slice survives a GDI call.
    unsafe {
        let mut resources = Resources {
            dc: CreateCompatibleDC(null_mut()),
            bitmap: null_mut(),
            font: null_mut(),
        };
        ensure!(!resources.dc.is_null(), "could not create overlay DC");
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = width as i32;
        info.bmiHeader.biHeight = -(height as i32);
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB;
        let mut bits = null_mut();
        resources.bitmap = CreateDIBSection(
            resources.dc,
            &info,
            DIB_RGB_COLORS,
            &mut bits,
            null_mut(),
            0,
        );
        ensure!(
            !resources.bitmap.is_null() && !bits.is_null(),
            "could not allocate overlay DIB"
        );
        let face: Vec<u16> = "Segoe UI\0".encode_utf16().collect();
        resources.font = CreateFontW(
            -23,
            0,
            0,
            0,
            FW_SEMIBOLD as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            ANTIALIASED_QUALITY as u32,
            0,
            face.as_ptr(),
        );
        ensure!(!resources.font.is_null(), "could not create overlay font");
        ensure!(
            !SelectObject(resources.dc, resources.bitmap).is_null(),
            "could not select overlay bitmap"
        );
        ensure!(
            !SelectObject(resources.dc, resources.font).is_null(),
            "could not select overlay font"
        );
        let length = (width * height * 4) as usize;
        let top = canvas.height() - height;
        {
            let pixels = std::slice::from_raw_parts_mut(bits.cast::<u8>(), length);
            for y in 0..height {
                for x in 0..width {
                    let rgb = canvas.get_pixel(x, top + y).0;
                    let offset = ((y * width + x) * 4) as usize;
                    pixels[offset..offset + 4].copy_from_slice(&[
                        rgb[2] / 3,
                        rgb[1] / 3,
                        rgb[0] / 3,
                        0,
                    ]);
                }
            }
        }
        SetBkMode(resources.dc, TRANSPARENT as i32);
        SetTextColor(resources.dc, 0x00ffffff);
        for (index, line) in overlay.lines.iter().enumerate() {
            let text: Vec<u16> = line.encode_utf16().collect();
            let mut rect = RECT {
                left: 16,
                top: 6 + index as i32 * 30,
                right: width as i32 - 16,
                bottom: 36 + index as i32 * 30,
            };
            ensure!(
                DrawTextW(
                    resources.dc,
                    text.as_ptr(),
                    text.len() as i32,
                    &mut rect,
                    DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX
                ) != 0,
                "could not draw overlay text"
            );
        }
        ensure!(GdiFlush() != 0, "could not finish overlay drawing");
        let pixels = std::slice::from_raw_parts(bits.cast::<u8>(), length);
        for y in 0..height {
            for x in 0..width {
                let i = ((y * width + x) * 4) as usize;
                canvas.put_pixel(
                    x,
                    top + y,
                    image::Rgb([pixels[i + 2], pixels[i + 1], pixels[i]]),
                );
            }
        }
    }
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn unicode_overlay_is_bounded_and_does_not_change_the_source() {
        let original = image::RgbImage::from_pixel(1280, 720, image::Rgb([90, 120, 150]));
        let mut sample = original.clone();
        super::draw(
            &mut sample,
            &super::Overlay {
                lines: vec![
                    "Target: Nébuleuse 星雲".into(),
                    "Saved LIGHT exposures: 505 · External: 12.3 °C".into(),
                ],
            },
        )
        .unwrap();
        assert_eq!(sample.get_pixel(20, 20), original.get_pixel(20, 20));
        assert_ne!(sample, original);
        assert!(
            sample
                .rows()
                .skip(648)
                .flatten()
                .any(|p| p.0.iter().all(|c| *c > 200))
        );
        assert_eq!(original.get_pixel(20, 700), &image::Rgb([90, 120, 150]));
    }
}
