use codecraft_selfupdate::{Status, Store, now};
#[test]
fn healthy_clean_exit_breaks_failure_streak() {
    let dir = tempfile::tempdir().unwrap();
    let payload = dir.path().join("payload");
    std::fs::create_dir(&payload).unwrap();
    std::fs::write(payload.join("codecraft"), "fake").unwrap();
    let store = Store::new(dir.path().join("base"), payload.clone());
    let id = store.stage(&payload, "1.0.0", "abcdef", "fake").unwrap();
    store.promote(&id).unwrap();
    store.begin_boot(&id, now()).unwrap();
    store.failed(&id, "first crash").unwrap();
    store.use_version(&id).unwrap();
    store.begin_boot(&id, now()).unwrap();
    store.mark_good(&id).unwrap();
    store.clean_exit(&id).unwrap();
    assert!(store.state().boot.is_none());
    store.begin_boot(&id, now()).unwrap();
    store.failed(&id, "new streak").unwrap();
    assert_ne!(store.meta(&id).unwrap().status, Status::Bad);
    assert_eq!(store.meta(&id).unwrap().failures, 1);
}
