#[test]
fn provenance_is_bounded_before_copy_or_metadata_allocation() {
    let dir = tempfile::tempdir().unwrap();
    let s = codecraft_selfupdate::Store::new(dir.path().join("home"), dir.path().join("factory"));
    assert!(s.stage(dir.path(), "1.0.0", &"a".repeat(100000), "branch").is_err());
    assert!(s.stage(dir.path(), "1.0.0", "abcdef", &"b".repeat(100000)).is_err());
    assert!(!s.base.exists());
}
