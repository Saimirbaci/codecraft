//! The product's name and build: what the window title, the app menu, Help › About and
//! `--version` show.
//!
//! Release builds carry the commit and date they were built from (`CODECRAFT_BUILD_COMMIT`,
//! `CODECRAFT_BUILD_DATE`, set by the release workflow at compile time); local builds have
//! neither and say so.

use serde::Serialize;

/// The name people see (window title, menus, About, packages). Machine names stay `codecraft`.
pub const DISPLAY_NAME: &str = "Code Craft";
/// The workspace version (`[workspace.package] version`, bumped by `cargo xtask version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// SPDX license of the product.
pub const LICENSE: &str = "MIT";
pub const COPYRIGHT: &str = "Copyright (c) 2026 The CodeCraft contributors";
pub const REPOSITORY: &str = "https://github.com/Saimirbaci/codecraft";

/// What was built, from which commit, for which platform.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildInfo {
    pub name: &'static str,
    pub version: &'static str,
    /// Short commit hash (release builds).
    pub commit: Option<String>,
    /// UTC build date `YYYY-MM-DD` (release builds).
    pub date: Option<String>,
    pub os: &'static str,
    pub arch: &'static str,
    pub license: &'static str,
}

/// A commit hash from the build environment: hex only, shortened to 10 characters.
fn clean_commit(raw: Option<&str>) -> Option<String> {
    let c = raw?.trim();
    (c.len() >= 7 && c.chars().all(|ch| ch.is_ascii_hexdigit())).then(|| c.chars().take(10).collect())
}

/// A build date from the build environment: `YYYY-MM-DD` only.
fn clean_date(raw: Option<&str>) -> Option<String> {
    let d = raw?.trim();
    let ok =
        d.len() == 10 && d.char_indices().all(|(i, ch)| if i == 4 || i == 7 { ch == '-' } else { ch.is_ascii_digit() });
    ok.then(|| d.to_string())
}

impl BuildInfo {
    pub fn current() -> Self {
        Self::with(option_env!("CODECRAFT_BUILD_COMMIT"), option_env!("CODECRAFT_BUILD_DATE"))
    }

    fn with(commit: Option<&str>, date: Option<&str>) -> Self {
        Self {
            name: DISPLAY_NAME,
            version: VERSION,
            commit: clean_commit(commit),
            date: clean_date(date),
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            license: LICENSE,
        }
    }

    /// `0.2.0 (build 1a2b3c4d5e, 2026-10-17)`, or `0.2.0 (local build)`.
    pub fn version_line(&self) -> String {
        match (&self.commit, &self.date) {
            (Some(c), Some(d)) => format!("{} (build {c}, {d})", self.version),
            (Some(c), None) => format!("{} (build {c})", self.version),
            (None, Some(d)) => format!("{} ({d})", self.version),
            (None, None) => format!("{} (local build)", self.version),
        }
    }

    /// `macOS aarch64`.
    pub fn platform(&self) -> String {
        format!("{} {}", os_label(self.os), self.arch)
    }

    /// The text About's Copy button puts on the clipboard (for bug reports).
    pub fn report(&self) -> String {
        format!("{} {}\n{}\nLicense: {}\n{REPOSITORY}", self.name, self.version_line(), self.platform(), self.license)
    }
}

fn os_label(os: &str) -> &str {
    match os {
        "macos" => "macOS",
        "linux" => "Linux",
        "windows" => "Windows",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_metadata_is_validated() {
        let b = BuildInfo::with(Some("1a2b3c4d5e6f7a8b"), Some("2026-10-17"));
        assert_eq!(b.commit.as_deref(), Some("1a2b3c4d5e"));
        assert_eq!(b.version_line(), format!("{VERSION} (build 1a2b3c4d5e, 2026-10-17)"));
        let bad = BuildInfo::with(Some("$(rm -rf)"), Some("17/10/2026"));
        assert_eq!((bad.commit, bad.date), (None, None));
        assert_eq!(BuildInfo::with(Some("abc"), None).commit, None);
        let local = BuildInfo::with(None, None);
        assert_eq!(local.version_line(), format!("{VERSION} (local build)"));
        let r = local.report();
        assert!(r.starts_with(&format!("Code Craft {VERSION}")), "{r}");
        assert!(r.contains("License: MIT") && r.contains(REPOSITORY), "{r}");
    }
}
