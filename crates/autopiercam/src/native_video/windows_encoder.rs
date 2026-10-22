use super::*;
use windows::{
    Win32::{Media::MediaFoundation::*, System::Com::*},
    core::{GUID, Interface},
};

struct Runtime(media_api::Api);
impl Runtime {
    fn start() -> Result<Self> {
        // Dedicated child/test thread, never a UI apartment or capture thread.
        let api = media_api::Api::load()?;
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            if let Err(error) = api.startup() {
                CoUninitialize();
                return Err(error);
            }
        }
        Ok(Self(api))
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        unsafe {
            let _ = self.0.shutdown();
            CoUninitialize();
        }
    }
}

fn read_rgb(path: &Path) -> Result<image::RgbImage> {
    let mut reader = image::ImageReader::open(path)?.with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    Ok(reader.decode()?.to_rgb8())
}

fn media_type(
    api: &media_api::Api,
    width: u32,
    height: u32,
    subtype: &windows::core::GUID,
) -> Result<IMFMediaType> {
    unsafe {
        let media = api.media_type()?;
        media.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        media.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        media.SetUINT64(
            &MF_MT_FRAME_SIZE,
            (u64::from(width) << 32) | u64::from(height),
        )?;
        media.SetUINT64(&MF_MT_FRAME_RATE, (30_u64 << 32) | 1)?;
        media.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1_u64 << 32) | 1)?;
        Ok(media)
    }
}

pub(super) fn encode(directory: &Path, samples: &[Sample], output: &Path) -> Result<()> {
    let first = read_rgb(&directory.join(&samples[0].file))?;
    let (width, height) = ((first.width() + 1) & !1, (first.height() + 1) & !1);
    ensure!(
        width >= 16 && height >= 16 && width <= 4096 && height <= 4096,
        "unsupported video dimensions"
    );
    drop(first);
    let runtime = Runtime::start()?;
    let api = &runtime.0;
    // Only this process owns these COM objects. WriteSample supplies backpressure
    // (throttling is NOT disabled); one decoded image/buffer is produced at a time.
    unsafe {
        let attributes = api.attributes()?;
        attributes.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
        attributes.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
        // Exclusive creation: even the private output must never overwrite a file.
        let stream = api
            .file(output)
            .context("creating exclusive MP4 byte stream")?;
        let writer = api
            .writer(&stream, &attributes)
            .context("creating MP4 sink writer")?;
        let target = media_type(api, width, height, &MFVideoFormat_H264)?;
        target.SetUINT32(
            &MF_MT_AVG_BITRATE,
            (width * height * 3).clamp(1_000_000, 12_000_000),
        )?;
        let index = writer
            .AddStream(&target)
            .context("adding H.264 output stream")?;
        let input = media_type(api, width, height, &MFVideoFormat_RGB32)?;
        input.SetUINT32(&MF_MT_DEFAULT_STRIDE, width * 4)?;
        writer
            .SetInputMediaType(index, &input, None)
            .context("setting RGB32 encoder input")?;
        let extended: IMFSinkWriterEx = writer.cast()?;
        for transform_index in 0..16 {
            let mut transform = None;
            let mut category = GUID::zeroed();
            match extended.GetTransformForStream(
                index,
                transform_index,
                Some(&mut category),
                &mut transform,
            ) {
                Ok(()) => {
                    if category == MFT_CATEGORY_VIDEO_PROCESSOR {
                        transform
                            .context("missing video processor")?
                            .GetAttributes()?
                            .SetUINT32(&MF_XVP_DISABLE_FRC, 1)?;
                    }
                }
                Err(error) if error.code() == MF_E_INVALIDINDEX => break,
                Err(error) => return Err(error).context("configuring native video timing"),
            }
        }
        writer.BeginWriting().context("starting MP4 writer")?;
        let mut time = 0;
        // The Windows H.264 transform reports nominal duration for its final
        // sample. One terminal hold sample places the final boundary explicitly
        // (like a concat end marker), without converting sparse video to CFR.
        const TAIL: i64 = 10_000_000 / 30;
        let last = samples.last().context("empty native video job")?;
        ensure!(
            last.duration >= TAIL,
            "final video frame is shorter than the encoder's nominal period"
        );
        let terminal = Sample {
            file: last.file.clone(),
            duration: TAIL,
        };
        let add_terminal = last.duration > TAIL;
        for (ordinal, sample) in samples
            .iter()
            .chain(add_terminal.then_some(&terminal))
            .enumerate()
        {
            if ordinal == samples.len() {
                time -= TAIL;
            }
            let image = read_rgb(&directory.join(&sample.file))?;
            ensure!(
                image.width() <= width
                    && image.height() <= height
                    && image.width() >= width - 1
                    && image.height() >= height - 1,
                "video dimensions changed during segment"
            );
            let size = width * height * 4;
            let buffer = api.buffer(size)?;
            let mut pointer = std::ptr::null_mut();
            buffer.Lock(&mut pointer, None, None)?;
            // Lock provides a writable size-byte region until Unlock. Dimensions
            // and multiplications are bounded above; RGB bytes are owned locally.
            let bytes = std::slice::from_raw_parts_mut(pointer, size as usize);
            bytes.fill(0);
            for (x, y, rgb) in image.enumerate_pixels() {
                let offset = ((y * width + x) * 4) as usize;
                bytes[offset..offset + 4].copy_from_slice(&[rgb[2], rgb[1], rgb[0], 255]);
            }
            buffer.Unlock()?;
            buffer.SetCurrentLength(size)?;
            let frame = api.sample()?;
            frame.AddBuffer(&buffer)?;
            frame.SetSampleTime(time)?;
            frame.SetSampleDuration(sample.duration)?;
            if ordinal == 0 {
                frame.SetUINT32(&MFSampleExtension_Discontinuity, 1)?;
            }
            writer
                .WriteSample(index, &frame)
                .with_context(|| format!("encoding frame {ordinal}"))?;
            time += sample.duration;
        }
        writer.Finalize().context("finalizing MP4")?;
        // Finalize completes the sink. Release its byte stream before syncing
        // the finalized regular file (Flush on the finalized stream is invalid).
    }
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(output)?
        .sync_all()?;
    Ok(())
}

// Small summaries only, not whole decoded movies. Colour/orientation checks use
// representative top/bottom pixels, plus timestamp/duration and dimensions.
#[allow(dead_code)] // Pixel/layout summaries are inspected by the synthetic QA test.
pub(super) struct Decoded {
    pub width: u32,
    pub height: u32,
    pub time: i64,
    pub duration: i64,
    pub top: [u8; 3],
    pub bottom: [u8; 3],
}

pub(super) fn decode(path: &Path) -> Result<Vec<Decoded>> {
    let runtime = Runtime::start()?;
    let api = &runtime.0;
    unsafe {
        let attributes = api.attributes()?;
        attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;
        let reader = api.reader(path, &attributes)?;
        let index = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        ensure!(
            reader
                .GetNativeMediaType(index, 0)?
                .GetGUID(&MF_MT_SUBTYPE)?
                == MFVideoFormat_H264,
            "movie video stream is not H.264"
        );
        let output = api.media_type()?;
        output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        reader.SetCurrentMediaType(index, None, &output)?;
        let media = reader.GetCurrentMediaType(index)?;
        let dimensions = media.GetUINT64(&MF_MT_FRAME_SIZE)?;
        let (width, height) = ((dimensions >> 32) as u32, dimensions as u32);
        let mut result = Vec::new();
        loop {
            let mut flags = 0;
            let mut time = 0;
            let mut sample = None;
            reader.ReadSample(
                index,
                0,
                None,
                Some(&mut flags),
                Some(&mut time),
                Some(&mut sample),
            )?;
            ensure!(
                flags & MF_SOURCE_READERF_ERROR.0 as u32 == 0,
                "native video decoder failed"
            );
            if let Some(sample) = sample {
                ensure!(result.len() < 2000, "too many decoded samples");
                let buffer = sample.ConvertToContiguousBuffer()?;
                let mut pointer = std::ptr::null_mut();
                let mut length = 0;
                buffer.Lock(&mut pointer, None, Some(&mut length))?;
                let bytes = std::slice::from_raw_parts(pointer, length as usize);
                let stride = media.GetUINT32(&MF_MT_DEFAULT_STRIDE).unwrap_or(width * 4) as i32;
                let row = stride.unsigned_abs() as usize;
                let valid = width >= 4
                    && height >= 4
                    && row >= width as usize * 4
                    && bytes.len() >= row * height as usize;
                let pixel = |y: u32| {
                    let y = if stride < 0 { height - 1 - y } else { y };
                    let offset = y as usize * row + (width / 2) as usize * 4;
                    [bytes[offset + 2], bytes[offset + 1], bytes[offset]]
                };
                let (top, bottom) = if valid {
                    (pixel(height / 4), pixel(3 * height / 4))
                } else {
                    ([0; 3], [0; 3])
                };
                buffer.Unlock()?;
                ensure!(valid, "invalid decoded video layout");
                result.push(Decoded {
                    width,
                    height,
                    time,
                    duration: sample.GetSampleDuration()?,
                    top,
                    bottom,
                });
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
        }
        ensure!(!result.is_empty(), "movie has no decodable samples");
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn daily_playback_is_24_source_frames_per_second_with_exact_end_and_order() {
        let root = tempfile::tempdir().unwrap();
        for (name, color) in [("day.jpg", [190, 60, 30]), ("night.jpg", [30, 60, 190])] {
            image::RgbImage::from_pixel(1280, 720, image::Rgb(color))
                .save(root.path().join(name))
                .unwrap();
        }
        let samples = (0..24)
            .map(|i| Sample {
                file: if i < 12 { "day.jpg" } else { "night.jpg" }.into(),
                duration: (i + 1) * 10_000_000 / 24 - i * 10_000_000 / 24,
            })
            .collect::<Vec<_>>();
        let output = root.path().join("daily.mp4");
        encode(root.path(), &samples, &output).unwrap();
        let decoded = decode(&output).unwrap();
        assert_eq!(decoded.len(), 25); // 24 source frames plus terminal hold
        assert_eq!((decoded[0].width, decoded[0].height), (1280, 720));
        for (i, frame) in decoded[..24].iter().enumerate() {
            assert!((frame.time - i as i64 * 10_000_000 / 24).abs() < 10_000);
            if i < 12 {
                assert!(frame.top[0] > frame.top[2] + 100);
            } else {
                assert!(frame.top[2] > frame.top[0] + 100);
            }
        }
        let last = decoded.last().unwrap();
        assert!((last.time + last.duration - 10_000_000).abs() < 10_000);
    }

    #[test]
    fn native_h264_round_trip_preserves_pixels_orientation_and_variable_timing() {
        let root = tempfile::tempdir().unwrap();
        let image = image::RgbImage::from_fn(66, 48, |_, y| {
            if y < 24 {
                image::Rgb([210, 30, 20])
            } else {
                image::Rgb([20, 40, 210])
            }
        });
        image.save(root.path().join("0.jpg")).unwrap();
        let samples = vec![
            Sample {
                file: "0.jpg".into(),
                duration: 10_000_000,
            },
            Sample {
                file: "0.jpg".into(),
                duration: 30_000_000,
            },
        ];
        let path = root.path().join("test.mp4");
        encode(root.path(), &samples, &path).unwrap();
        let original = fs::read(&path).unwrap();
        let stsz = original.windows(4).position(|x| x == b"stsz").unwrap();
        assert_eq!(
            u32::from_be_bytes(original[stsz + 12..stsz + 16].try_into().unwrap()),
            3
        );
        let decoded = decode(&path).unwrap();
        assert_eq!(decoded.len(), 3); // two source samples plus one terminal hold
        assert_eq!((decoded[0].width, decoded[0].height), (66, 48));
        assert_eq!(decoded[0].time, 0);
        assert!((decoded[1].time - 10_000_000).abs() < 10_000);
        assert!((decoded[2].time + decoded[2].duration - 40_000_000).abs() < 10_000);
        assert!((decoded[1].duration + decoded[2].duration - 30_000_000).abs() < 10_000);
        for (actual, expected) in [
            (decoded[0].top, [210, 30, 20]),
            (decoded[0].bottom, [20, 40, 210]),
        ] {
            for (a, e) in actual.into_iter().zip(expected) {
                assert!(
                    a.abs_diff(e) < 25,
                    "colour/orientation mismatch: {actual:?} expected {expected:?}"
                );
            }
        }
        assert!(encode(root.path(), &samples, &path).is_err());
        assert_eq!(fs::read(path).unwrap(), original);
    }
}
