#![cfg(windows)]
use autopiercam::native_video::{encode_atomic, verify};
use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

fn fixture(directory: &Path) {
    image::RgbImage::from_fn(64, 48, |x, y| image::Rgb([40 + x as u8, 60 + y as u8, 150]))
        .save(directory.join("sample.jpg"))
        .unwrap();
    fs::write(directory.join("samples.json"), r#"[{"file":"sample.jpg","duration":416666},{"file":"sample.jpg","duration":416667},{"file":"sample.jpg","duration":416667}]"#).unwrap();
}

#[test]
fn ordinary_capture_binary_does_not_eagerly_import_optional_media_dlls() {
    // Inspect the PE import table, not strings (dynamic library names correctly
    // remain in the binary). This protects Windows N capture/--help startup.
    let bytes = fs::read(env!("CARGO_BIN_EXE_autopiercam")).unwrap();
    let u16_at = |at| u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap()) as usize;
    let u32_at = |at| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
    let pe = u32_at(0x3c);
    assert_eq!(&bytes[pe..pe + 4], b"PE\0\0");
    let optional = pe + 24;
    assert_eq!(u16_at(optional), 0x20b); // PE32+ (Windows x64)
    let sections = optional + u16_at(pe + 20);
    let offset = |rva: usize| {
        for i in 0..u16_at(pe + 6) {
            let section = sections + i * 40;
            let start = u32_at(section + 12);
            let size = u32_at(section + 8).max(u32_at(section + 16));
            if (start..start + size).contains(&rva) {
                return u32_at(section + 20) + rva - start;
            }
        }
        panic!("unmapped import RVA");
    };
    let mut descriptor = offset(u32_at(optional + 120));
    let mut imports = Vec::new();
    while u32_at(descriptor + 12) != 0 {
        let name = offset(u32_at(descriptor + 12));
        let end = bytes[name..].iter().position(|byte| *byte == 0).unwrap() + name;
        imports.push(String::from_utf8_lossy(&bytes[name..end]).to_ascii_lowercase());
        descriptor += 20;
    }
    assert!(!imports.is_empty());
    assert!(
        !imports
            .iter()
            .any(|name| ["mfplat.dll", "mfreadwrite.dll", "mf.dll"].contains(&name.as_str())),
        "optional media dependency imported at startup: {imports:?}"
    );
}

#[test]
fn native_child_encodes_daily_frames_without_sdk_or_external_encoder_and_never_overwrites() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    let output = root.path().join("clip.mp4");
    let exe = Path::new(env!("CARGO_BIN_EXE_autopiercam"));
    encode_atomic(exe, root.path(), &output, Duration::from_secs(20)).unwrap();
    assert_eq!(verify(&output).unwrap(), 4); // source frames + terminal duration marker
    let before = fs::read(&output).unwrap();
    assert!(encode_atomic(exe, root.path(), &output, Duration::from_secs(20)).is_err());
    assert_eq!(fs::read(&output).unwrap(), before);
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 3);
}

#[test]
fn bad_frame_and_deadline_never_publish_or_leave_partial_movie() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    let exe = Path::new(env!("CARGO_BIN_EXE_autopiercam"));
    let output = root.path().join("clip.mp4");
    let start = Instant::now();
    let error = encode_atomic(exe, root.path(), &output, Duration::ZERO).unwrap_err();
    assert!(error.to_string().contains("deadline"));
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(!output.exists());
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 2);
    // A valid first frame followed by corrupt data exercises partial-write cleanup.
    fs::write(root.path().join("bad.jpg"), b"not a jpeg").unwrap();
    fs::write(
        root.path().join("samples.json"),
        r#"[{"file":"sample.jpg","duration":1000000},{"file":"bad.jpg","duration":1000000}]"#,
    )
    .unwrap();
    assert!(encode_atomic(exe, root.path(), &output, Duration::from_secs(20)).is_err());
    assert!(!output.exists());
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 3);
}
