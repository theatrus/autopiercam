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
    setup_named(driver, None)
}
fn setup_named(driver: &Arc<Driver>, name: Option<&str>) -> Camera {
    let info = driver
        .cameras()
        .unwrap()
        .into_iter()
        .find(|i| i.is_color && name.is_none_or(|name| i.name == name))
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
fn sdk_failed_exposure_retries_once_without_reopening_or_delivering_bad_pixels() {
    let driver = driver(
        CameraDriver::ZwoSdk,
        json!({"instant":true,"failedStatuses":1,"legacyProtocol":true}),
        None,
    );
    let mut camera = setup(&driver);
    let mut pixels = vec![123; 7];
    assert!(camera.poll_frame(&mut pixels, 50).unwrap_err().is_timeout());
    assert_eq!(pixels, vec![123; 7]);
    assert!(driver.cameras().is_err());
    assert_eq!(frame(&mut camera).len(), 64 * 64 * 2);
    assert_eq!(frame(&mut camera).len(), 64 * 64 * 2);
}

#[test]
fn sdk_repeated_failed_exposure_is_bounded_and_can_stop_after_first_failure() {
    let driver = driver(
        CameraDriver::ZwoSdk,
        json!({"instant":true,"failedStatuses":2,"legacyProtocol":true}),
        None,
    );
    let mut camera = setup(&driver);
    let mut pixels = vec![123; 7];
    assert!(camera.poll_frame(&mut pixels, 50).unwrap_err().is_timeout());
    let error = camera.poll_frame(&mut pixels, 50).unwrap_err();
    assert!(!error.is_timeout());
    assert!(error.to_string().contains("state Some(3)"));
    assert_eq!(pixels, vec![123; 7]);
    drop(camera);
    let driver = self::driver(
        CameraDriver::ZwoSdk,
        json!({"failedStatuses":1,"legacyProtocol":true}),
        None,
    );
    let mut camera = setup(&driver);
    assert!(camera.poll_frame(&mut pixels, 50).unwrap_err().is_timeout());
    camera.stop_capture().unwrap();
}

#[test]
fn asi662_video_preserves_fps_across_idle_and_exposure_changes() {
    video_preserves_fps_across_idle_and_exposure_changes("ZWO ASI662MC");
}
#[test]
fn asi676_video_preserves_fps_across_idle_and_exposure_changes() {
    video_preserves_fps_across_idle_and_exposure_changes("ZWO ASI676MC");
}
fn video_preserves_fps_across_idle_and_exposure_changes(name: &str) {
    let driver = driver(CameraDriver::ZwoDirect, json!({}), None);
    let info = driver
        .cameras()
        .unwrap()
        .into_iter()
        .find(|c| c.name == name)
        .unwrap();
    let mut camera = driver.open(info).unwrap();
    camera
        .set_control(ControlType::EXPOSURE, 1000, false)
        .unwrap();
    camera
        .set_roi(Roi {
            width: 64,
            height: 64,
            bin: 1,
            image_type: ImageType::Raw16,
        })
        .unwrap();
    camera.set_max_fps(0.5).unwrap();
    for invalid in [0.0, -1.0, 31.0, f64::NAN, f64::INFINITY] {
        assert!(camera.set_max_fps(invalid).is_err());
    }
    assert!(camera.uses_video());
    assert_eq!(camera.pacing_allowance(), Duration::from_secs(47));
    camera.start_capture().unwrap();
    frame(&mut camera);
    let first = Instant::now();
    camera.pause_between_frames().unwrap();
    camera
        .set_control(ControlType::EXPOSURE, 2000, false)
        .unwrap();
    camera.start_capture().unwrap();
    frame(&mut camera);
    assert!(first.elapsed() >= Duration::from_millis(1950));
    camera.stop_capture().unwrap();
    camera
        .set_control(ControlType::EXPOSURE, 30_000_001, false)
        .unwrap();
    assert!(
        !camera.uses_video(),
        "long exposures must use still capture"
    );
    assert_eq!(camera.pacing_allowance(), Duration::from_secs(47));
    camera
        .set_control(ControlType::EXPOSURE, 1000, false)
        .unwrap();
    camera.set_max_fps(0.01).unwrap();
    camera.start_capture().unwrap();
    frame(&mut camera);
    assert!(
        camera
            .poll_frame(&mut Vec::new(), 1)
            .unwrap_err()
            .is_timeout()
    );
    // Delivery cadence is adjustable without a 100-second acquisition sleep.
    camera.set_max_fps(30.0).unwrap();
    assert!(camera.pacing_allowance() < Duration::from_secs(46));
    let stopping = Instant::now();
    camera.stop_capture().unwrap();
    assert!(stopping.elapsed() < Duration::from_secs(3));
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
fn asi662_direct_full_frame_and_centered_roi() {
    let driver = driver(CameraDriver::ZwoDirect, json!({}), None);
    let info = driver
        .cameras()
        .unwrap()
        .into_iter()
        .find(|info| info.name == "ZWO ASI662MC")
        .expect("pinned Regain must advertise ASI662MC Direct USB");
    assert_eq!((info.max_width, info.max_height), (1920, 1080));
    assert!(info.is_color);
    assert!(
        info.serial.is_none(),
        "inventory must not probe device serials"
    );
    assert_eq!(info.bayer_pattern, autopiercam_camera::BayerPattern::Rg);
    assert_eq!(info.supported_bins, vec![1]);
    let mut camera = driver.open(info).unwrap();
    let caps = camera.controls().unwrap();
    let exposure = caps
        .iter()
        .find(|c| c.control_type == ControlType::EXPOSURE)
        .unwrap();
    assert_eq!(
        (exposure.min_value, exposure.max_value),
        (32, 2_000_000_000)
    );
    let gain = caps
        .iter()
        .find(|c| c.control_type == ControlType::GAIN)
        .unwrap();
    assert_eq!((gain.min_value, gain.max_value), (0, 600));
    // Validate the ceiling without waiting for a long simulated exposure.
    camera
        .set_control(ControlType::EXPOSURE, 2_000_000_000, false)
        .unwrap();
    assert!(
        camera
            .set_control(ControlType::EXPOSURE, 2_000_000_001, false)
            .is_err()
    );
    camera
        .set_control(ControlType::EXPOSURE, 32, false)
        .unwrap();
    camera.set_control(ControlType::GAIN, 200, false).unwrap();
    assert!(camera.set_control(ControlType::GAIN, 601, false).is_err());
    assert!(
        camera
            .set_roi(Roi {
                width: 960,
                height: 540,
                bin: 2,
                image_type: ImageType::Raw16
            })
            .is_err()
    );

    for (width, height, image_type) in [
        (1920, 1080, ImageType::Raw16),
        // Center y=508 is even but NOT 8-aligned; the adapter must round to 504.
        (64, 64, ImageType::Raw16),
        (64, 64, ImageType::Raw8),
    ] {
        camera
            .set_roi(Roi {
                width,
                height,
                bin: 1,
                image_type,
            })
            .unwrap();
        camera.start_capture().unwrap();
        assert!(driver.cameras().is_err());
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut pixels = Vec::new();
        loop {
            match camera.poll_frame(&mut pixels, 50) {
                Ok(meta) => {
                    assert_eq!(
                        (meta.width, meta.height, meta.image_type),
                        (width, height, image_type)
                    );
                    assert_eq!(
                        pixels.len(),
                        width as usize * height as usize * image_type.bytes_per_pixel().unwrap()
                    );
                    break;
                }
                Err(e) if e.is_timeout() => assert!(Instant::now() < deadline),
                Err(e) => panic!("{e}"),
            }
        }
        camera.stop_capture().unwrap();
    }
}

#[test]
fn selected_serials_accept_case_variants_and_reject_mismatches() {
    for (backend, serial) in [
        (CameraDriver::ZwoSdk, "SIM00001"),
        (CameraDriver::ZwoDirect, "DIRECT-SIMULATOR"),
    ] {
        let driver = driver(backend, json!({}), Some(serial.into()));
        let info = driver.cameras().unwrap().remove(0);
        assert!(info.serial.is_none());
        let camera = driver.open(info.clone()).unwrap();
        assert!(
            camera
                .info()
                .serial
                .as_deref()
                .unwrap()
                .eq_ignore_ascii_case(serial)
        );
        let verified = camera.info().clone();
        drop(camera);
        // A new worker reopens the cached selected ID/interface without list.
        drop(driver.open(verified.clone()).unwrap());
        if backend == CameraDriver::ZwoDirect {
            let mut missing = verified;
            missing.locator = Some("missing-interface".into());
            assert!(driver.open(missing).is_err());
        }
        let mut changed = info;
        changed.serial = Some("different-device".into());
        assert!(driver.open(changed).is_err());
    }
}

#[test]
fn direct_model_candidate_checks_saved_serial_before_capture() {
    for (serial, should_open) in [("DIRECT-SIMULATOR", true), ("wrong-serial", false)] {
        let driver = driver(CameraDriver::ZwoDirect, json!({}), Some(serial.into()));
        let mut candidate = driver
            .cameras()
            .unwrap()
            .into_iter()
            .find(|camera| camera.name == "ZWO ASI662MC")
            .unwrap();
        assert!(candidate.serial.is_none());
        // The simulated list omits locators; real metadata discovery supplies one.
        // Use the simulator's exact interface to exercise the selected-open path.
        candidate.locator = Some("simulated-interface".into());
        match driver.open(candidate) {
            Ok(camera) => {
                assert!(should_open, "must not accept a mismatching saved serial");
                assert_eq!(camera.info().serial.as_deref(), Some("direct-simulator"));
                drop(camera);
            }
            Err(error) => {
                assert!(!should_open, "{error:#}");
                assert!(format!("{error:#}").contains("camera identity differs"));
            }
        }
        // A failed verification must not leave ownership latched either.
        assert!(driver.cameras().is_ok());
    }
}

#[test]
fn blank_serial_filters_open_a_single_matching_camera() {
    for backend in [CameraDriver::ZwoSdk, CameraDriver::ZwoDirect] {
        for serial in ["", "  "] {
            let driver = driver(backend, json!({}), Some(serial.into()));
            assert!(driver.selected_serial().is_none());
            let info = driver.cameras().unwrap().remove(0);
            drop(driver.open(info).unwrap());
        }
    }
}

#[test]
fn blank_config_retains_verified_identity_across_driver_replacement() {
    for backend in [CameraDriver::ZwoSdk, CameraDriver::ZwoDirect] {
        let first = driver(backend, json!({}), None);
        let camera = first.open(first.cameras().unwrap().remove(0)).unwrap();
        let mut verified = camera.info().clone();
        assert!(verified.serial.is_some());
        drop(camera);
        drop(first);
        let replacement = driver(backend, json!({}), None);
        drop(replacement.open(verified.clone()).unwrap());
        verified.serial = Some("different-camera".into());
        assert!(replacement.open(verified).is_err());
        assert!(
            replacement.selected_serial().is_none(),
            "must not mutate saved selection"
        );
    }
}

#[test]
fn managed_white_balance_is_shared_and_precedes_raw8_conversion() {
    use autopiercam_core::config::{WhiteBalanceConfig, WhiteBalanceMode};
    use regain_core::white_balance::{Gains, Geometry, Mode, Output, Settings, WhiteBalance};
    for backend in [CameraDriver::ZwoSdk, CameraDriver::ZwoDirect] {
        // This exact per-frame AWB expectation uses the retained still path.
        // Continuous AWB runs on every acquired frame, including replaced frames.
        let driver = driver(backend, json!({"instant":true,"legacyProtocol":true}), None);
        let mut camera = setup(&driver);
        let original = frame(&mut camera);
        assert!(
            camera
                .configure_white_balance(&WhiteBalanceConfig::default())
                .is_err()
        );
        camera.stop_capture().unwrap();
        assert!(
            camera
                .configure_white_balance(&WhiteBalanceConfig {
                    red: 9.0,
                    ..WhiteBalanceConfig::default()
                })
                .is_err()
        );
        for (mode, upstream) in [
            (WhiteBalanceMode::Manual, Mode::Manual),
            (WhiteBalanceMode::Once, Mode::Once),
            (WhiteBalanceMode::Continuous, Mode::Continuous),
        ] {
            let config = WhiteBalanceConfig {
                mode,
                red: 2.0,
                blue: 0.5,
            };
            let mut engine = WhiteBalance::default();
            engine
                .configure(Settings {
                    mode: upstream,
                    gains: Gains {
                        red: config.red,
                        blue: config.blue,
                    },
                    output: Output::Corrected,
                })
                .unwrap();
            camera.configure_white_balance(&config).unwrap();
            camera.start_capture().unwrap();
            for _ in 0..2 {
                let mut expected = original.clone();
                engine
                    .process(
                        &mut expected,
                        Geometry {
                            width: 64,
                            height: 64,
                            x: 0,
                            y: 0,
                            bayer: 0,
                            dark: false,
                        },
                    )
                    .unwrap();
                assert_eq!(frame(&mut camera), expected);
            }
            camera.stop_capture().unwrap();
        }
        camera
            .configure_white_balance(&WhiteBalanceConfig {
                red: 2.0,
                blue: 0.5,
                ..WhiteBalanceConfig::default()
            })
            .unwrap();
        camera
            .set_roi(Roi {
                width: 64,
                height: 64,
                bin: 1,
                image_type: ImageType::Raw8,
            })
            .unwrap();
        camera.start_capture().unwrap();
        let mut engine = WhiteBalance::default();
        engine
            .configure(Settings {
                mode: Mode::Manual,
                gains: Gains {
                    red: 2.0,
                    blue: 0.5,
                },
                output: Output::Corrected,
            })
            .unwrap();
        let mut expected = original.clone();
        engine
            .process(
                &mut expected,
                Geometry {
                    width: 64,
                    height: 64,
                    x: 0,
                    y: 0,
                    bayer: 0,
                    dark: false,
                },
            )
            .unwrap();
        assert_eq!(
            frame(&mut camera),
            expected
                .as_chunks::<2>()
                .0
                .iter()
                .map(|p| p[1])
                .collect::<Vec<_>>()
        );
        camera.stop_capture().unwrap();
        drop(camera);
        let mut camera = setup(&driver);
        assert_eq!(
            frame(&mut camera),
            original,
            "reopen without opting in must remain unmanaged"
        );
    }
}
#[test]
fn continuous_manual_white_balance_precedes_delivery_decimation() {
    use autopiercam_core::config::{WhiteBalanceConfig, WhiteBalanceMode};
    use regain_core::white_balance::{Gains, Geometry, Mode, Output, Settings, WhiteBalance};
    for backend in [CameraDriver::ZwoSdk, CameraDriver::ZwoDirect] {
        let driver = driver(backend, json!({"instant":true}), None);
        let mut camera = setup(&driver);
        let mut expected = frame(&mut camera);
        camera.stop_capture().unwrap();
        camera
            .configure_white_balance(&WhiteBalanceConfig {
                mode: WhiteBalanceMode::Manual,
                red: 2.0,
                blue: 0.5,
            })
            .unwrap();
        let mut engine = WhiteBalance::default();
        engine
            .configure(Settings {
                mode: Mode::Manual,
                gains: Gains {
                    red: 2.0,
                    blue: 0.5,
                },
                output: Output::Corrected,
            })
            .unwrap();
        engine
            .process(
                &mut expected,
                Geometry {
                    width: 64,
                    height: 64,
                    x: 0,
                    y: 0,
                    bayer: 0,
                    dark: false,
                },
            )
            .unwrap();
        camera.set_max_fps(2.0).unwrap();
        camera.start_capture().unwrap();
        assert!(frame(&mut camera) == expected);
        std::thread::sleep(Duration::from_millis(600));
        assert!(frame(&mut camera) == expected);
        camera.stop_capture().unwrap();
    }
}

#[test]
fn continuous_transition_frames_reach_preview_on_both_backends() {
    for backend in [CameraDriver::ZwoSdk, CameraDriver::ZwoDirect] {
        let driver = driver(backend, json!({"instant":true}), None);
        let direct = backend == CameraDriver::ZwoDirect;
        let mut camera = setup_named(&driver, direct.then_some("ZWO ASI662MC"));
        frame(&mut camera);
        camera
            .set_control(
                ControlType::EXPOSURE,
                if direct { 1_000_000 } else { 20_000_000 },
                false,
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut pixels = Vec::new();
        loop {
            match camera.poll_frame(&mut pixels, 50) {
                Ok(meta) => {
                    assert!(
                        !meta.settings_settled,
                        "transition must not claim requested settings"
                    );
                    assert_eq!(pixels.len(), 64 * 64 * 2);
                    break;
                }
                Err(error) if error.is_timeout() => assert!(Instant::now() < deadline),
                Err(error) => panic!("{error}"),
            }
        }
        camera.stop_capture().unwrap();
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
    // Desired controls update locally; the worker fences old-generation frames.
    assert_eq!(camera.control_value(ControlType::GAIN).unwrap().value, 250);
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
    // Control validation only: these simulator settings do not start an exposure.
    for exposure_us in [60_000_000, 120_000_000, 2_000_000_000] {
        camera
            .set_control(ControlType::EXPOSURE, exposure_us, false)
            .unwrap();
    }
    assert!(
        camera
            .set_control(ControlType::EXPOSURE, 2_000_000_001, false)
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
        let error = loop {
            let error = camera.poll_frame(&mut Vec::new(), 50).unwrap_err();
            if !error.is_timeout() {
                break error;
            }
            assert!(
                started.elapsed() < Duration::from_secs(12),
                "owner fault was hidden by cached IPC: {fault}"
            );
        };
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
    config.camera.white_balance = Some(autopiercam_core::config::WhiteBalanceConfig {
        red: 2.0,
        blue: 0.5,
        ..Default::default()
    });
    config.camera.settle_frames = 1;
    config.capture.preview_max_fps = 30.0;
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

#[test]
fn asi662_direct_pipeline_settles_and_saves_without_network() {
    let driver = driver(CameraDriver::ZwoDirect, json!({}), None);
    let temp = tempfile::tempdir().unwrap();
    let mut config = autopiercam_core::config::Config::default();
    config.camera.driver = CameraDriver::ZwoDirect;
    config.camera.name_contains = Some("ASI662MC".into());
    config.camera.raw16 = true;
    config.camera.white_balance = Some(autopiercam_core::config::WhiteBalanceConfig {
        mode: autopiercam_core::config::WhiteBalanceMode::Once,
        ..Default::default()
    });
    config.camera.min_exposure_us = 1000;
    config.camera.max_exposure_us = 1000;
    config.camera.min_gain = 200;
    config.camera.max_gain = 200;
    config.camera.settle_frames = 1;
    config.capture.preview_max_fps = 30.0;
    config.capture.interval_ms = 1;
    config.capture.directory = temp.path().join("captures");
    assert!(!config.upload.enabled && !config.video.enabled);
    let path = temp.path().join("config.toml");
    std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
    let monitor = autopiercam::AgentMonitor::new();
    autopiercam::run_agent_with_monitor_and_preview(
        &driver,
        &path,
        Some(1),
        &autopiercam::AgentControl::new(),
        &monitor,
        &autopiercam::PreviewHub::new().begin_session(),
    )
    .unwrap();
    assert_eq!(monitor.snapshot().frames_saved, 1);
    let saved = monitor.snapshot().last_artifact.unwrap();
    assert!(Path::new(&saved).is_file());
    assert_eq!(Path::new(&saved).extension().unwrap(), "png");
}

#[test]
fn fractional_preview_idle_does_not_fault_the_production_pipeline() {
    let driver = driver(
        CameraDriver::ZwoSdk,
        json!({"instant":true,"width":64,"height":64}),
        None,
    );
    let temp = tempfile::tempdir().unwrap();
    let mut config = autopiercam_core::config::Config::default();
    config.camera.min_exposure_us = 1000;
    config.camera.max_exposure_us = 1000;
    config.camera.settle_frames = 1;
    config.capture.preview_max_fps = 0.1;
    // Longer than the 5-second short-exposure timeout, sooner than the preview.
    config.capture.interval_ms = 7000;
    config.capture.directory = temp.path().join("captures");
    assert!(!config.upload.enabled && !config.video.enabled);
    let path = temp.path().join("config.toml");
    std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
    let monitor = autopiercam::AgentMonitor::new();
    autopiercam::run_agent_with_monitor_and_preview(
        &driver,
        &path,
        Some(2),
        &autopiercam::AgentControl::new(),
        &monitor,
        &autopiercam::PreviewHub::new().begin_session(),
    )
    .unwrap();
    assert_eq!(monitor.snapshot().frames_saved, 2);
}

#[test]
fn simulated_usb_recovery_requires_release_and_consumes_the_binding_once() {
    let serial = "1234567890abcdef";
    let driver = driver(
        CameraDriver::ZwoSdk,
        json!({"instant":true,"serial":serial}),
        Some(serial.into()),
    );
    let info = driver
        .cameras()
        .unwrap()
        .into_iter()
        .find(|info| info.is_color)
        .unwrap();
    driver.bind_usb_recovery(&info).unwrap();
    let camera = setup(&driver);
    assert!(driver.bind_usb_recovery(&info).is_err());
    assert!(driver.reset_bound_usb(|| false).is_err());
    assert!(driver.has_usb_recovery_target());
    drop(camera);
    assert!(driver.reset_bound_usb(|| true).is_err());
    assert!(driver.has_usb_recovery_target());
    driver.reset_bound_usb(|| false).unwrap();
    assert!(!driver.has_usb_recovery_target());
    assert!(driver.reset_bound_usb(|| false).is_err());
    assert!(!frame(&mut setup(&driver)).is_empty());
}

#[test]
fn simulated_capture_fault_resets_only_when_opted_in_and_respects_cooldown() {
    let serial = "1234567890abcdef";
    let temp = tempfile::tempdir().unwrap();
    let mut config = autopiercam_core::config::Config::default();
    config.camera.serial = Some(serial.into());
    config.camera.min_exposure_us = 1000;
    config.camera.max_exposure_us = 1000;
    config.capture.directory = temp.path().join("captures");
    let path = temp.path().join("config.toml");
    let monitor = autopiercam::AgentMonitor::new();
    for (enabled, reset) in [(false, false), (true, true), (true, false)] {
        config.camera.usb_reset_on_fault = enabled;
        std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
        let driver = driver(
            CameraDriver::ZwoSdk,
            json!({"instant":true,"serial":serial,"fault":"download"}),
            Some(serial.into()),
        );
        let error = autopiercam::run_agent_with_monitor_and_preview(
            &driver,
            &path,
            Some(1),
            &autopiercam::AgentControl::new(),
            &monitor,
            &autopiercam::PreviewHub::new().begin_session(),
        )
        .unwrap_err();
        assert_eq!(
            format!("{error:#}").contains("USB port reset completed"),
            reset
        );
        assert_eq!(monitor.snapshot().frames_saved, 0);
    }
}
