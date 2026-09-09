use super::{auto_exposure_limit_us, auto_exposure_limit_value};
use autopiercam_asi::{ControlCaps, ControlType};

fn caps(name: &str) -> ControlCaps {
    ControlCaps {
        name: name.to_owned(),
        description: "Maximum automatic exposure".to_owned(),
        min_value: 1,
        max_value: 60_000,
        default_value: 100,
        auto_supported: false,
        writable: true,
        control_type: ControlType::AUTO_MAX_EXPOSURE,
    }
}

#[test]
fn runtime_millisecond_names_convert_thirty_and_sixty_second_limits() {
    for name in ["AutoExpMaxExpMS", "AutoExpMaxExpMs", "autoexpmaxexpms"] {
        let caps = caps(name);
        for (microseconds, milliseconds) in [(30_000_000, 30_000), (60_000_000, 60_000)] {
            assert_eq!(auto_exposure_limit_value(&caps, microseconds), milliseconds);
            assert_eq!(auto_exposure_limit_us(&caps, milliseconds), microseconds);
        }
        // The SDK's 60,000 ms ceiling is distinct from the camera's much
        // longer manual exposure capability.
        assert_eq!(auto_exposure_limit_us(&caps, caps.max_value), 60_000_000);
    }
}

#[test]
fn microsecond_controls_preserve_microsecond_resolution() {
    for name in ["AutoExpMaxExp", "AutoExpMaxExpUS"] {
        let caps = caps(name);
        for exposure_us in [1, 32, 999, 1_001, 30_000_000, 60_000_000, 2_000_000_000] {
            assert_eq!(auto_exposure_limit_value(&caps, exposure_us), exposure_us);
            assert_eq!(auto_exposure_limit_us(&caps, exposure_us), exposure_us);
        }
    }
}

#[test]
fn millisecond_quantization_is_reported_as_the_effective_limit() {
    let caps = caps("AutoExpMaxExpMS");
    for (requested_us, sdk_value, effective_us) in [
        (1, 1, 1_000),
        (999, 1, 1_000),
        (1_000, 1, 1_000),
        (1_001, 2, 2_000),
        (59_999_001, 60_000, 60_000_000),
        (60_000_000, 60_000, 60_000_000),
    ] {
        assert_eq!(auto_exposure_limit_value(&caps, requested_us), sdk_value);
        assert_eq!(auto_exposure_limit_us(&caps, sdk_value), effective_us);
    }
}

#[test]
fn extreme_capability_values_do_not_overflow_conversion() {
    let caps = caps("AutoExpMaxExpMS");
    assert_eq!(auto_exposure_limit_us(&caps, i64::MAX), i64::MAX);
    assert_eq!(auto_exposure_limit_value(&caps, i64::MAX), i64::MAX / 1_000);
}
