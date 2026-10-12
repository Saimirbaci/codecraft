//! Render the IDE offscreen to a PNG (no window, no focus stealing).
//!
//! `cargo run -p codecraft-ui-egui --example snapshot -- <file-or-project> <out.png> [command-id ...]`
//! Each extra argument is a command id run before rendering (e.g. `view.toolWindows.problems`),
//! optionally `id=<json params>`, `wait=<ms>` to let background work (debugger, servers) progress,
//! or `lsp` to start rust-analyzer (off by default in snapshots).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use serde_json::Value;

use codecraft_ui_egui::App;

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let open = PathBuf::from(args.next().ok_or("usage: snapshot <file-or-project> <out.png> [command ...]")?);
    let out = PathBuf::from(args.next().ok_or("missing output path")?);
    let commands: Vec<String> = args.collect();
    let mut h = Harness::builder()
        .with_size(egui::vec2(1440.0, 900.0))
        .with_pixels_per_point(2.0)
        .build_eframe(move |_| App::ephemeral(Some(open)));
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(120) {
        h.run_steps(1);
        if !h.state().session.jobs_running() {
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    for c in commands {
        // `wait=<ms>`: keep the app running (pumping services) for a while, e.g. until a debug
        // session reaches a breakpoint.
        if let Some(ms) = c.strip_prefix("wait=").and_then(|v| v.parse::<u64>().ok()) {
            let t = Instant::now();
            while t.elapsed() < Duration::from_millis(ms) {
                h.run_steps(1);
                std::thread::sleep(Duration::from_millis(30));
            }
            continue;
        }
        // `lsp`: enable language servers (snapshots run without them by default) and start
        // rust-analyzer.
        if c == "lsp" {
            h.state_mut().session.lsp_enabled = true;
            if let Err(e) = h.state_mut().session.execute("lsp.restart", &serde_json::json!({ "server": "rust" })) {
                eprintln!("lsp: {e}");
            }
            continue;
        }
        let (id, params) = match c.split_once('=') {
            Some((id, json)) => (id.to_string(), serde_json::from_str(json).map_err(|e| e.to_string())?),
            None => (c, Value::Null),
        };
        if let Err(e) = h.state_mut().session.execute(&id, &params) {
            eprintln!("{id}: {e}");
        }
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(300) {
            h.run_steps(1);
            if !h.state().session.jobs_running() {
                break;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }
    h.run_steps(3);
    let image = h.render()?;
    image.save(&out).map_err(|e| e.to_string())?;
    println!("wrote {}", out.display());
    Ok(())
}
