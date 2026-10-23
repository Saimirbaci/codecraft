use codecraft_selfupdate::{FACTORY, Store, choose, now};
#[test]
fn manual_factory_rollback_persists_when_current_is_also_last_good() {
    let dir = tempfile::tempdir().unwrap();
    let payload = dir.path().join("payload");
    std::fs::create_dir(&payload).unwrap();
    std::fs::write(payload.join("codecraft"), "fake").unwrap();
    let s = Store::new(dir.path().join("home"), payload.clone());
    let id = s.stage(&payload, "1.0.0", "abcdef", "fake").unwrap();
    s.promote(&id).unwrap();
    s.mark_good(&id).unwrap();
    assert_eq!(s.rollback().unwrap().id, FACTORY);
    assert_eq!(choose(&s.boot_state(), now()).id, FACTORY);
    assert!(s.state().last_good.is_none());
    assert!(s.take_notice().unwrap().unwrap().contains("manually to factory"));
}
