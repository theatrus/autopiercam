//! All subprocesses use Regain's --simulate; never loads an SDK or opens USB.
use autopiercam_camera::{Camera, ControlType, Driver, ImageType, Roi};
use autopiercam_core::config::CameraDriver;
use serde_json::json;
use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

fn driver(
    backend: CameraDriver,
    settings: serde_json::Value,
    serial: Option<String>,
) -> Arc<Driver> {
    Arc::new(Driver::simulated(
        Path::new(env!("CARGO_BIN_EXE_regain-device"))
            .parent()
            .unwrap()
            .into(),
        backend,
        settings,
        serial,
    ))
}
fn setup(driver: &Arc<Driver>) -> Camera {
    let info = driver
        .cameras()
        .unwrap()
        .into_iter()
        .find(|i| i.is_color)
        .unwrap();
    let mut camera = driver.open(info).unwrap();
    camera
        .set_control(ControlType::EXPOSURE, 1000, false)
        .unwrap();
    camera.set_control(ControlType::GAIN, 200, false).unwrap();
    camera
        .set_roi(Roi {
            width: 64,
            height: 64,
            bin: 1,
            image_type: ImageType::Raw16,
        })
        .unwrap();
    camera.start_capture().unwrap();
    camera
}
fn frame(camera: &mut Camera) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut pixels = Vec::new();
    loop {
        match camera.poll_frame(&mut pixels, 50) {
            Ok(meta) => {
                assert_eq!((meta.width, meta.height), (64, 64));
                return pixels;
            }
            Err(e) if e.is_timeout() => assert!(Instant::now() < deadline),
            Err(e) => panic!("{e}"),
        }
    }
}
#[test]
fn sdk_and_direct_capture_without_discovery_while_owned() {
    for backend in [CameraDriver::ZwoSdk, CameraDriver::ZwoDirect] {
        let driver = driver(backend, json!({"instant":true}), None);
        let mut camera = setup(&driver);
        assert!(
            driver
                .cameras()
                .unwrap_err()
                .to_string()
                .contains("cached inventory")
        );
        assert!(driver.open(camera.info().clone()).is_err());
        assert_eq!(frame(&mut camera).len(), 64 * 64 * 2);
        camera.set_control(ControlType::GAIN, 300, false).unwrap();
        assert_eq!(camera.control_value(ControlType::GAIN).unwrap().value, 300);
        assert_eq!(frame(&mut camera).len(), 64 * 64 * 2);
        camera.stop_capture().unwrap();
        drop(camera);
        assert!(!driver.cameras().unwrap().is_empty());
    }
}
#[test]
fn pending_frames_keep_metadata_and_apply_edits_to_next_exposure() {
    let driver = driver(CameraDriver::ZwoSdk, json!({}), None);
    let mut camera = setup(&driver);
    camera
        .set_control(ControlType::EXPOSURE, 300_000, false)
        .unwrap();
    assert!(
        camera
            .poll_frame(&mut Vec::new(), 1)
            .unwrap_err()
            .is_timeout()
    );
    camera.set_control(ControlType::GAIN, 250, false).unwrap();
    assert_eq!(camera.control_value(ControlType::GAIN).unwrap().value, 200);
    frame(&mut camera);
    frame(&mut camera);
    assert_eq!(camera.control_value(ControlType::GAIN).unwrap().value, 250);
}
#[test]
fn refuses_serial_mismatch_and_unsupported_direct_exposure() {
    let wrong = driver(CameraDriver::ZwoSdk, json!({}), Some("wrong-serial".into()));
    assert!(wrong.open(wrong.cameras().unwrap().remove(0)).is_err());
    let direct = driver(CameraDriver::ZwoDirect, json!({}), None);
    let mut camera = setup(&direct);
    assert!(
        camera
            .set_control(ControlType::EXPOSURE, 60_000_000, false)
            .is_err()
    );
    assert!(camera.set_control(ControlType::GAIN, 200, true).is_err());
}
#[test]
fn failed_download_faults_instead_of_retrying_or_reopening() {
    for fault in ["download", "crash", "hang"] {
        let driver = driver(
            CameraDriver::ZwoSdk,
            json!({"instant":true,"fault":fault}),
            None,
        );
        let mut camera = setup(&driver);
        let started = Instant::now();
        let error = camera.poll_frame(&mut Vec::new(), 50).unwrap_err();
        assert!(
            !error.is_timeout(),
            "only pending exposures may return a poll timeout"
        );
        assert!(camera.start_capture().is_err());
        drop(camera);
        assert!(started.elapsed() < Duration::from_secs(15));
    }
}
#[test]
fn dropping_an_exposing_camera_is_bounded() {
    let driver = driver(CameraDriver::ZwoSdk, json!({}), None);
    let mut camera = setup(&driver);
    camera
        .set_control(ControlType::EXPOSURE, 120_000_000, false)
        .unwrap();
    assert!(
        camera
            .poll_frame(&mut Vec::new(), 1)
            .unwrap_err()
            .is_timeout()
    );
    let started = Instant::now();
    drop(camera);
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[test]
fn production_pipeline_settles_and_saves_a_simulated_frame() {
    let driver = driver(
        CameraDriver::ZwoSdk,
        json!({"instant":true,"width":64,"height":64}),
        None,
    );
    let temp = tempfile::tempdir().unwrap();
    let mut config = autopiercam_core::config::Config::default();
    config.camera.min_exposure_us = 1000;
    config.camera.max_exposure_us = 1000;
    config.camera.min_gain = 200;
    config.camera.max_gain = 200;
    config.camera.exposure_control = autopiercam_core::config::ExposureControl::Adaptive;
    config.camera.settle_frames = 1;
    config.capture.preview_max_fps = 30;
    config.capture.interval_ms = 1;
    config.capture.directory = temp.path().join("captures");
    // Explicitly local-only. No upload, video or sharing service is started.
    assert!(!config.upload.enabled && !config.video.enabled);
    let path = temp.path().join("config.toml");
    std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
    let control = autopiercam::AgentControl::new();
    let monitor = autopiercam::AgentMonitor::new();
    let preview = autopiercam::PreviewHub::new();
    autopiercam::run_agent_with_monitor_and_preview(
        &driver,
        &path,
        Some(1),
        &control,
        &monitor,
        &preview.begin_session(),
    )
    .unwrap();
    assert!(monitor.snapshot().frames_captured > 0);
    assert_eq!(monitor.snapshot().frames_saved, 1);
    let saved = monitor.snapshot().last_artifact.unwrap();
    assert!(Path::new(&saved).is_file());
    assert_eq!(Path::new(&saved).extension().unwrap(), "jpg");
}
