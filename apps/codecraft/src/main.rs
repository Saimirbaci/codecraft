//! CodeCraft desktop app.

use std::path::PathBuf;

fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let open = std::env::args_os().nth(1).map(PathBuf::from);
    codecraft_ui_egui::run(open)
}
