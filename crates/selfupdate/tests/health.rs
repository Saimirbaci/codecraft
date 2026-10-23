//! Environment-dependent protocol tests use fresh processes, never global environment mutation.
use codecraft_selfupdate::health;
#[test]
fn child_handshake() {
    if std::env::var_os("SELFUPDATE_HEALTH_CHILD").is_none() {
        return;
    }
    let path = std::path::PathBuf::from(std::env::var_os("CODECRAFT_HEALTH_FILE").unwrap());
    assert!(!health::report_healthy().unwrap());
    assert!(!path.exists());
    health::report_started();
    assert!(health::report_healthy().unwrap());
    assert!(health::is_reported());
    assert_eq!(std::fs::read(path).unwrap(), b"healthy\n");
}
#[test]
fn atomic_health_protocol_in_isolated_process() {
    let dir = tempfile::tempdir().unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_handshake"])
        .env("SELFUPDATE_HEALTH_CHILD", "1")
        .env("CODECRAFT_LAUNCHER", "1")
        .env("CODECRAFT_HEALTHY_AFTER", "0")
        .env("CODECRAFT_HEALTH_FILE", dir.path().join("health/1.ok"))
        .status()
        .unwrap();
    assert!(status.success());
}
