use codecraft_selfupdate::{State, Store};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
#[test]
fn readers_never_observe_partial_pointer_writes() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf(), dir.path().join("factory"));
    let mut state = State { current: Some("1.0.0-abcd-1".into()), ..Default::default() };
    store.save_state(&state).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_reader = stop.clone();
    let reader = store.clone();
    let worker = std::thread::spawn(move || {
        let mut reads = 0;
        while !stop_reader.load(Ordering::Relaxed) {
            assert!(reader.state().current.is_some());
            reads += 1;
        }
        reads
    });
    for n in 1..101 {
        state.current = Some(format!("1.0.0-abcd-{n}"));
        store.save_state(&state).unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    assert!(worker.join().unwrap() > 0);
}
