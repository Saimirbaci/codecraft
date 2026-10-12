//! The real rust-analyzer against a throwaway Cargo project. Skips (passes without doing
//! anything) when no working rust-analyzer is installed.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use codecraft_lsp::{LspClient, ServerConfig, ServerEvent};

struct TempProject(PathBuf);

impl Drop for TempProject {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn hover_against_real_rust_analyzer() {
    let Some(rust_analyzer) = codecraft_lsp::find_rust_analyzer() else {
        eprintln!("rust-analyzer is not installed; skipping");
        return;
    };
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let project = TempProject(std::env::temp_dir().join(format!("codecraft-ra-{}-{nanos}", std::process::id())));
    let src = project.0.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        project.0.join("Cargo.toml"),
        "[package]\nname = \"ra_probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    let main = src.join("main.rs");
    let text = "fn main() {\n    let answer: u32 = 42;\n    println!(\"{answer}\");\n}\n";
    std::fs::write(&main, text).unwrap();

    let (client, events) = LspClient::spawn(ServerConfig::rust_analyzer(rust_analyzer, project.0.clone())).unwrap();
    let wait_for = |what: &str, timeout: Duration, mut matches: Box<dyn FnMut(&ServerEvent) -> bool + '_>| {
        let started = Instant::now();
        loop {
            let left = timeout.saturating_sub(started.elapsed());
            let event = events.recv_timeout(left).unwrap_or_else(|_| panic!("timed out waiting for {what}"));
            if let ServerEvent::Exited { .. } = event {
                panic!("rust-analyzer exited while waiting for {what}: {:?}", client.stderr_tail());
            }
            if matches(&event) {
                return event;
            }
        }
    };

    wait_for("initialization", Duration::from_secs(60), Box::new(|event| matches!(event, ServerEvent::Initialized(_))));
    assert!(client.capabilities().unwrap().hover_provider.is_some());
    client.did_open(&main, "rust", 1, text).unwrap();
    let id = client.hover(&main, 1, 9).unwrap();
    // Before indexing finishes the answer may be null or "content modified"; it must arrive.
    wait_for(
        "the hover response",
        Duration::from_secs(120),
        Box::new(|event| matches!(event, ServerEvent::Response { id: got, .. } if *got == id)),
    );
    client.shutdown(Duration::from_secs(10)).unwrap();
    let started = Instant::now();
    loop {
        match events.recv_timeout(Duration::from_secs(15).saturating_sub(started.elapsed())) {
            Ok(ServerEvent::Exited { .. }) => break,
            Ok(_) => {}
            Err(_) => panic!("rust-analyzer did not exit"),
        }
    }
}
