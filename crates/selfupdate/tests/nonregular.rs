#![cfg(unix)]
#[test]
fn fifo_state_is_absent_instead_of_blocking_the_factory_escape_hatch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let status = std::process::Command::new("mkfifo").arg(&path).status().unwrap();
    assert!(status.success());
    let store = codecraft_selfupdate::Store::new(dir.path().to_path_buf(), dir.path().join("factory"));
    assert!(store.state().current.is_none());
}
