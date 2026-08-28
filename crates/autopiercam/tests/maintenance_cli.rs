use std::process::Command;

#[test]
fn upload_ledger_dispatches_before_loading_the_camera_sdk() {
    let root = tempfile::tempdir().unwrap();
    let missing_config = root.path().join("missing.toml");
    let missing_sdk = root.path().join("missing-ASICamera2.dll");
    let output = Command::new(env!("CARGO_BIN_EXE_autopiercam"))
        .arg("--sdk")
        .arg(&missing_sdk)
        .args(["upload-ledger", "migrate", "--config"])
        .arg(&missing_config)
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("could not read configuration"), "{stderr}");
    assert!(!stderr.contains("loading ZWO ASI SDK"), "{stderr}");
    assert!(
        !stderr.contains("could not load the ZWO ASI SDK"),
        "{stderr}"
    );
}
