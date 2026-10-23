//! Structured preflight. Probes are separate from pure policy so tests need no host tools.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const FIRST_BUILD_BYTES: u64 = 15 * 1024 * 1024 * 1024;
pub const WARM_BUILD_BYTES: u64 = 3 * 1024 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    Warn,
    Fail,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub level: Level,
    pub message: String,
    pub fix: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Report {
    pub checks: Vec<Check>,
}
impl Report {
    pub fn ready(&self) -> bool {
        self.checks.iter().all(|c| c.level != Level::Fail)
    }
}
#[derive(Default)]
pub struct Probes {
    pub git: bool,
    pub cargo: Option<String>,
    pub rustc: Option<String>,
    pub linker: bool,
    pub disk_bytes: Option<u64>,
    pub network: Option<bool>,
}
fn version(v: &str) -> Option<(u64, u64, u64)> {
    let token = v.split_whitespace().find(|s| s.starts_with(|c: char| c.is_ascii_digit()))?;
    let mut it = token.split(['.', '-']);
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next().unwrap_or("0").parse().ok()?))
}
pub fn evaluate(p: Probes, minimum: &str, bytes: u64) -> Report {
    let mut checks = Vec::new();
    let mut add = |name: &str, good, message: String, fix: &str| {
        checks.push(Check {
            name: name.into(),
            level: if good { Level::Ok } else { Level::Fail },
            message,
            fix: fix.into(),
        })
    };
    add(
        "git",
        p.git,
        if p.git { "git available" } else { "git is missing" }.into(),
        "Install git using your platform's package manager.",
    );
    for (name, value) in [("cargo", p.cargo), ("rustc", p.rustc)] {
        let good = value.as_deref().and_then(version).zip(version(minimum)).is_some_and(|(v, min)| v >= min);
        add(
            name,
            good,
            format!("{}; requires Rust >= {minimum}", value.unwrap_or_else(|| format!("{name} is missing"))),
            "Install or update Rust: https://rustup.rs (no automatic installation).",
        );
    }
    add(
        "linker",
        p.linker,
        "Platform development toolchain".into(),
        if cfg!(target_os = "macos") {
            "Run xcode-select --install in Terminal."
        } else {
            "Install your platform's C linker and development tools."
        },
    );
    add(
        "disk",
        p.disk_bytes.is_some_and(|n| n >= bytes),
        format!("Available: {:?} bytes; required: {bytes}", p.disk_bytes),
        "Free disk space on the build directory's volume or lower the configured threshold.",
    );
    if let Some(online) = p.network {
        add(
            "network",
            online,
            "Repository reachability (only required for clone/fetch)".into(),
            "Check your network and git credentials; git prompts are disabled.",
        );
    }
    Report { checks }
}
#[derive(Clone, Debug)]
pub struct Tools {
    pub git: PathBuf,
    pub cargo: PathBuf,
    pub rustc: PathBuf,
}
impl Default for Tools {
    fn default() -> Self {
        Self { git: "git".into(), cargo: "cargo".into(), rustc: "rustc".into() }
    }
}
fn output(program: &Path, args: &[&str]) -> Option<String> {
    crate::source::git(program, Path::new("."), args).ok()
}
pub fn probe(build_dir: &Path, tools: &Tools, network_url: Option<&str>) -> Probes {
    let mut volume = build_dir;
    while !volume.exists() {
        let Some(parent) = volume.parent() else { break };
        volume = parent;
    }
    #[cfg(not(windows))]
    let disk_bytes = Command::new("df")
        .arg("-Pk")
        .arg(volume)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .nth(1)
                .and_then(|l| l.split_whitespace().nth(3))
                .and_then(|s| s.parse::<u64>().ok())
        })
        .and_then(|n| n.checked_mul(1024));
    #[cfg(windows)]
    let disk_bytes = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "[System.IO.DriveInfo]::new([System.IO.Path]::GetPathRoot($env:CODECRAFT_BUILD_VOLUME)).AvailableFreeSpace",
        ])
        .env("CODECRAFT_BUILD_VOLUME", volume.canonicalize().unwrap_or_else(|_| volume.into()))
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u64>().ok());
    let network = network_url.map(|url| {
        crate::source::validate_url(url).is_ok()
            && crate::source::git(&tools.git, volume, &["ls-remote", "--exit-code", "--", url, "HEAD"]).is_ok()
    });
    Probes {
        git: output(&tools.git, &["--version"]).is_some(),
        cargo: output(&tools.cargo, &["--version"]),
        rustc: output(&tools.rustc, &["--version"]),
        linker: if cfg!(target_os = "macos") {
            output(Path::new("xcode-select"), &["-p"]).is_some()
        } else if cfg!(windows) {
            output(Path::new("where"), &["link.exe"]).is_some()
        } else {
            output(Path::new("cc"), &["--version"]).is_some()
        },
        disk_bytes,
        network,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn good() -> Probes {
        Probes {
            git: true,
            cargo: Some("cargo 1.95.0".into()),
            rustc: Some("rustc 1.95.0 (abc)".into()),
            linker: true,
            disk_bytes: Some(FIRST_BUILD_BYTES),
            network: None,
        }
    }
    #[test]
    fn matrix() {
        assert!(evaluate(good(), "1.95", FIRST_BUILD_BYTES).ready());
        assert!(!evaluate(Probes::default(), "1.95", FIRST_BUILD_BYTES).ready());
        let mut p = good();
        p.rustc = Some("rustc 1.94.9".into());
        assert!(!evaluate(p, "1.95", 0).ready());
        let mut p = good();
        p.network = Some(false);
        assert!(!evaluate(p, "1.95", 0).ready());
        let mut p = good();
        p.disk_bytes = Some(1);
        assert!(!evaluate(p, "1.95", 2).ready());
        let mut p = good();
        p.cargo = Some("malformed".into());
        assert!(!evaluate(p, "1.95", 0).ready());
    }
}
