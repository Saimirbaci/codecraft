//! macOS code signing and notarization: the exact `codesign`, `xcrun notarytool`, `xcrun stapler`
//! and `spctl` command lines (pure functions, unit-tested), and a [`Runner`] that runs them or, with
//! `--dry-run`, only prints them.
//!
//! Credentials never pass through here: signing identities live in a keychain (the login keychain,
//! or a temporary one in CI) and notarization uses a `notarytool` keychain profile the user created
//! with `xcrun notarytool store-credentials`. The commands name the identity and the profile, which
//! are not secrets, so printing them is safe. See docs/release.md.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The entitlements CodeCraft is signed with (relative to the workspace root).
pub const ENTITLEMENTS: &str = "apps/codecraft/macos/CodeCraft.entitlements";

/// Every entitlement in [`ENTITLEMENTS`], and why (also in docs/release.md). Hardened-runtime
/// exceptions (JIT, unsigned executable memory, DYLD variables, disabled library validation,
/// `cs.debugger`, `get-task-allow`) are deliberately absent: CodeCraft is a static Rust binary that
/// links only system frameworks, and the debugger is lldb-dap/debugserver, which carry their own
/// entitlements (CodeCraft never calls `task_for_pid`).
pub const ENTITLEMENT_KEYS: &[(&str, &str)] = &[(
    "com.apple.security.automation.apple-events",
    "programs run from the Terminal tool window and run configurations are attributed to CodeCraft \
     by macOS; without it the hardened runtime denies their Apple Events (osascript, open -a … \
     scripting) without asking",
)];

/// `codesign --sign -`: an ad-hoc signature (no identity, no keychain, runs on this Mac only).
pub const AD_HOC: &str = "-";

/// Overrides the signing identity when `--sign` is not given (kept from M9).
pub const IDENTITY_ENV: &str = "CODECRAFT_SIGN_IDENTITY";
/// The notarytool keychain profile when `--profile` is not given.
pub const PROFILE_ENV: &str = "CODECRAFT_NOTARY_PROFILE";

// ---------------------------------------------------------------- commands

/// A command line: program and arguments, displayed shell-quoted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
}

impl Cmd {
    pub fn new(program: &str) -> Self {
        Self { program: program.to_string(), args: Vec::new() }
    }

    pub fn arg(mut self, a: impl AsRef<std::ffi::OsStr>) -> Self {
        self.args.push(a.as_ref().to_string_lossy().into_owned());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        for a in args {
            self = self.arg(a);
        }
        self
    }

    pub fn command(&self) -> Command {
        let mut c = Command::new(&self.program);
        c.args(&self.args);
        c
    }
}

impl fmt::Display for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&shell_quote(&self.program))?;
        for a in &self.args {
            write!(f, " {}", shell_quote(a))?;
        }
        Ok(())
    }
}

/// POSIX shell quoting: bare when only safe characters, else single-quoted.
pub fn shell_quote(s: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if !s.is_empty() && s.chars().all(safe) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Runs commands, or prints them (`dry_run`). Every command is echoed before it runs, so CI logs
/// show exactly what was signed and submitted.
#[derive(Clone, Copy, Debug, Default)]
pub struct Runner {
    pub dry_run: bool,
}

/// What a captured command printed.
#[derive(Debug, Default)]
pub struct Captured {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

impl Runner {
    pub fn run(&self, cmd: &Cmd) -> Result<(), String> {
        println!("{}{cmd}", if self.dry_run { "" } else { "$ " });
        if self.dry_run {
            return Ok(());
        }
        let status = cmd.command().status().map_err(|e| format!("{cmd}: {e}"))?;
        if status.success() { Ok(()) } else { Err(format!("{cmd} failed: {status}")) }
    }

    /// Runs and captures the output (`None` in a dry run).
    pub fn capture(&self, cmd: &Cmd) -> Result<Option<Captured>, String> {
        println!("{}{cmd}", if self.dry_run { "" } else { "$ " });
        if self.dry_run {
            return Ok(None);
        }
        let out = cmd.command().output().map_err(|e| format!("{cmd}: {e}"))?;
        Ok(Some(Captured {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }))
    }
}

// ---------------------------------------------------------------- signing

/// A signing identity (`-` for ad hoc, else a keychain identity such as
/// `Developer ID Application: Name (TEAMID)` or its SHA-1 hash) and an optional keychain to find it in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signer {
    pub identity: String,
    pub keychain: Option<PathBuf>,
}

impl Signer {
    /// `--sign` if given, else `$CODECRAFT_SIGN_IDENTITY`, else ad hoc.
    pub fn resolve(sign: Option<&str>, keychain: Option<&Path>) -> Self {
        let env = std::env::var(IDENTITY_ENV).ok();
        let identity = sign
            .or(env.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(AD_HOC)
            .to_string();
        Self { identity, keychain: keychain.map(Path::to_path_buf) }
    }

    pub fn is_ad_hoc(&self) -> bool {
        self.identity == AD_HOC
    }

    fn base(&self) -> Cmd {
        let mut c = Cmd::new("codesign").args(["--force", "--sign", &self.identity]);
        if let Some(k) = &self.keychain {
            c = c.arg("--keychain").arg(k);
        }
        // A secure timestamp is required for notarization; ad-hoc signatures cannot have one.
        c.arg(if self.is_ad_hoc() { "--timestamp=none" } else { "--timestamp" })
    }

    /// Sign one piece of code with the hardened runtime (and entitlements for the app itself).
    pub fn sign_code(&self, path: &Path, entitlements: Option<&Path>) -> Cmd {
        let mut c = self.base().args(["--options", "runtime"]);
        if let Some(e) = entitlements {
            c = c.arg("--entitlements").arg(e);
        }
        c.arg(path)
    }

    /// Sign a disk image (no runtime or entitlements: those are for executables).
    pub fn sign_disk_image(&self, dmg: &Path) -> Cmd {
        self.base().arg(dmg)
    }

    /// Inside-out: nested code deepest first, then the bundle with the entitlements, then verify
    /// and show the signature (whose flags are checked for the hardened runtime).
    pub fn app_commands(&self, app: &Path, nested: &[PathBuf], entitlements: &Path) -> Vec<Cmd> {
        let mut nested = nested.to_vec();
        nested.sort_by(|a, b| depth(b).cmp(&depth(a)).then_with(|| a.cmp(b)));
        let mut cmds: Vec<Cmd> = nested.iter().map(|p| self.sign_code(p, None)).collect();
        cmds.push(self.sign_code(app, Some(entitlements)));
        cmds.push(verify_app(app));
        cmds.push(display(app));
        cmds
    }
}

fn depth(p: &Path) -> usize {
    p.components().count()
}

/// `codesign --verify --deep --strict --verbose=2`: the whole bundle, nested code included.
pub fn verify_app(app: &Path) -> Cmd {
    Cmd::new("codesign").args(["--verify", "--deep", "--strict", "--verbose=2"]).arg(app)
}

pub fn verify_disk_image(dmg: &Path) -> Cmd {
    Cmd::new("codesign").args(["--verify", "--strict", "--verbose=2"]).arg(dmg)
}

/// `codesign --display --verbose=2` (prints the CodeDirectory flags on stderr).
pub fn display(path: &Path) -> Cmd {
    Cmd::new("codesign").args(["--display", "--verbose=2"]).arg(path)
}

/// Whether `codesign --display` output shows the hardened runtime
/// (`CodeDirectory v=20500 size=… flags=0x10002(adhoc,runtime) …`).
pub fn has_hardened_runtime(display_output: &str) -> bool {
    display_output.lines().filter(|l| l.starts_with("CodeDirectory")).any(|l| {
        l.split_whitespace()
            .filter_map(|w| w.strip_prefix("flags="))
            .any(|f| f.split(['(', ')', ',']).any(|flag| flag == "runtime"))
    })
}

/// Code inside `CodeCraft.app` that must be signed before the bundle: nested bundles
/// (`.framework`, `.app`, `.xpc`, `.appex`, `.bundle`, `.plugin`) and Mach-O files, except the
/// bundle's main executable (signed with the bundle). Symbolic links are skipped. Empty today
/// (CodeCraft is one binary); this keeps signing correct when helpers or dylibs are added.
pub fn nested_code(app: &Path, main_executable: &Path) -> Result<Vec<PathBuf>, String> {
    let mut found = Vec::new();
    let contents = app.join("Contents");
    if contents.is_dir() {
        walk(&contents, main_executable, &mut found)?;
    }
    Ok(found)
}

const BUNDLE_EXTENSIONS: &[&str] = &["framework", "app", "xpc", "appex", "bundle", "plugin"];

fn walk(dir: &Path, main_executable: &Path, found: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        let Ok(meta) = std::fs::symlink_metadata(&p) else { continue };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            walk(&p, main_executable, found)?;
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or_default();
            if BUNDLE_EXTENSIONS.contains(&ext) {
                found.push(p);
            }
        } else if meta.is_file() && p != main_executable && is_mach_o(&p) {
            found.push(p);
        }
    }
    Ok(())
}

/// Mach-O (32/64-bit, either byte order) or a universal binary.
pub fn is_mach_o(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else { return false };
    let mut magic = [0u8; 4];
    if f.read_exact(&mut magic).is_err() {
        return false;
    }
    matches!(
        u32::from_be_bytes(magic),
        0xfeed_face | 0xfeed_facf | 0xcefa_edfe | 0xcffa_edfe | 0xcafe_babe | 0xbeba_feca
    )
}

/// Sign `app` inside-out with the entitlements, verify it, and check the hardened runtime.
pub fn sign_app(runner: &Runner, signer: &Signer, app: &Path, main_executable: &Path, entitlements: &Path) -> Result<(), String> {
    if !runner.dry_run && !entitlements.is_file() {
        return Err(format!("{}: entitlements file missing", entitlements.display()));
    }
    let nested = if app.is_dir() { nested_code(app, main_executable)? } else { Vec::new() };
    let cmds = signer.app_commands(app, &nested, entitlements);
    let Some((show, rest)) = cmds.split_last() else { return Ok(()) };
    for c in rest {
        runner.run(c)?;
    }
    if let Some(out) = runner.capture(show)? {
        if !out.success {
            return Err(format!("{show} failed: {}", out.stderr.trim()));
        }
        if !has_hardened_runtime(&out.stderr) {
            return Err(format!("{} is not signed with the hardened runtime:\n{}", app.display(), out.stderr.trim()));
        }
        eprint!("{}", out.stderr);
    }
    Ok(())
}

/// Sign and verify a disk image.
pub fn sign_dmg(runner: &Runner, signer: &Signer, dmg: &Path) -> Result<(), String> {
    runner.run(&signer.sign_disk_image(dmg))?;
    runner.run(&verify_disk_image(dmg))
}

// ---------------------------------------------------------------- notarization

/// `xcrun notarytool` with a keychain profile (created by the user with
/// `xcrun notarytool store-credentials <profile>`); the credentials stay in the keychain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notary {
    pub profile: String,
    pub keychain: Option<PathBuf>,
}

impl Notary {
    fn auth(&self, c: Cmd) -> Cmd {
        let c = c.args(["--keychain-profile", &self.profile]);
        match &self.keychain {
            Some(k) => c.arg("--keychain").arg(k),
            None => c,
        }
    }

    /// Upload and wait for the verdict, as JSON (`{"id": …, "status": "Accepted", …}`).
    pub fn submit(&self, file: &Path) -> Cmd {
        self.auth(Cmd::new("xcrun").args(["notarytool", "submit"]).arg(file))
            .args(["--wait", "--output-format", "json"])
    }

    /// The notary service's log for a submission (why it was rejected).
    pub fn log(&self, id: &str) -> Cmd {
        self.auth(Cmd::new("xcrun").args(["notarytool", "log", id]))
    }
}

pub fn staple(path: &Path) -> Cmd {
    Cmd::new("xcrun").args(["stapler", "staple"]).arg(path)
}

pub fn validate_staple(path: &Path) -> Cmd {
    Cmd::new("xcrun").args(["stapler", "validate"]).arg(path)
}

/// Gatekeeper's verdict: `-t install` for the disk image, `-t exec` for an app.
pub fn assess(path: &Path, kind: &str) -> Cmd {
    Cmd::new("spctl").args(["-a", "-vv", "-t", kind]).arg(path)
}

/// `ditto -c -k --keepParent`: the zip notarytool takes for an app.
pub fn zip_app(app: &Path, zip: &Path) -> Cmd {
    Cmd::new("ditto").args(["-c", "-k", "--keepParent"]).arg(app).arg(zip)
}

/// notarytool's JSON answer.
#[derive(Debug, Default, PartialEq, Eq, serde::Deserialize)]
pub struct Submission {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub message: String,
}

impl Submission {
    pub fn accepted(&self) -> bool {
        self.status == "Accepted"
    }
}

/// The JSON object in notarytool's output (it may print progress lines around it).
pub fn parse_submission(stdout: &str) -> Result<Submission, String> {
    let start = stdout.find('{').ok_or_else(|| format!("no JSON in notarytool output: {}", stdout.trim()))?;
    let end = stdout.rfind('}').filter(|e| *e >= start).ok_or_else(|| "unterminated JSON".to_string())?;
    let json = stdout.get(start..=end).ok_or_else(|| "bad JSON bounds".to_string())?;
    serde_json::from_str(json).map_err(|e| format!("notarytool output: {e}: {}", stdout.trim()))
}

/// What to notarize.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Dmg(PathBuf),
    /// Zipped for upload; the ticket is stapled to the app.
    App(PathBuf),
}

/// `cargo xtask notarize` options.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NotarizeOptions {
    pub profile: Option<String>,
    pub dmg: Option<PathBuf>,
    pub app: Option<PathBuf>,
    pub keychain: Option<PathBuf>,
    pub dry_run: bool,
}

impl NotarizeOptions {
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut o = Self::default();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            let (flag, inline) = match a.split_once('=') {
                Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
                _ => (a.as_str(), None),
            };
            let mut value = |name: &str| -> Result<String, String> {
                inline.clone().or_else(|| it.next().cloned()).ok_or_else(|| format!("{name} needs a value"))
            };
            match flag {
                "--profile" => o.profile = Some(value("--profile")?),
                "--dmg" => o.dmg = Some(PathBuf::from(value("--dmg")?)),
                "--app" => o.app = Some(PathBuf::from(value("--app")?)),
                "--keychain" => o.keychain = Some(PathBuf::from(value("--keychain")?)),
                "--dry-run" => o.dry_run = true,
                other => {
                    return Err(format!(
                        "unknown option {other} (expected --profile <name>, --dmg <path>, --app <path>, --keychain <path>, --dry-run)"
                    ));
                }
            }
        }
        if o.dmg.is_some() && o.app.is_some() {
            return Err("give --dmg or --app, not both".into());
        }
        Ok(o)
    }

    /// `--profile`, else `$CODECRAFT_NOTARY_PROFILE`.
    pub fn notary(&self) -> Result<Notary, String> {
        let env = std::env::var(PROFILE_ENV).ok();
        let profile = self.profile.as_deref().or(env.as_deref()).map(str::trim).filter(|s| !s.is_empty());
        let profile = profile.ok_or_else(|| {
            format!(
                "no notarytool keychain profile: pass --profile <name> or set {PROFILE_ENV} \
                 (create one with `xcrun notarytool store-credentials <name>`; see docs/release.md)"
            )
        })?;
        Ok(Notary { profile: profile.to_string(), keychain: self.keychain.clone() })
    }

    pub fn target(&self, default_dmg: PathBuf) -> Target {
        match (&self.app, &self.dmg) {
            (Some(app), _) => Target::App(app.clone()),
            (None, Some(dmg)) => Target::Dmg(dmg.clone()),
            (None, None) => Target::Dmg(default_dmg),
        }
    }
}

/// The commands before and after the submission: (what is uploaded, the zip step if any, the
/// steps after acceptance).
pub fn notarize_plan(target: &Target) -> (PathBuf, Option<Cmd>, Vec<Cmd>) {
    match target {
        Target::Dmg(dmg) => (dmg.clone(), None, vec![staple(dmg), validate_staple(dmg), assess(dmg, "install")]),
        Target::App(app) => {
            let zip = app.with_extension("notarize.zip");
            (zip.clone(), Some(zip_app(app, &zip)), vec![staple(app), validate_staple(app), assess(app, "exec")])
        }
    }
}

/// Submit, wait, staple and assess. On rejection, prints the notary log and fails.
pub fn notarize(runner: &Runner, notary: &Notary, target: &Target) -> Result<(), String> {
    let signed = match target {
        Target::Dmg(p) | Target::App(p) => p,
    };
    if !runner.dry_run && !signed.exists() {
        return Err(format!("{}: not found (build it with `cargo xtask dmg --sign <identity>`)", signed.display()));
    }
    let (upload, zip, after) = notarize_plan(target);
    if let Some(z) = &zip {
        runner.run(z)?;
    }
    let submitted = runner.capture(&notary.submit(&upload));
    if zip.is_some() && !runner.dry_run {
        let _ = std::fs::remove_file(&upload);
    }
    if let Some(out) = submitted? {
        let parsed = parse_submission(&out.stdout);
        match parsed {
            Ok(s) if s.accepted() => println!("notarization accepted ({})", s.id),
            Ok(s) => {
                eprintln!("notarization {}: {}", s.status, s.message);
                if !s.id.is_empty() {
                    let _ = runner.run(&notary.log(&s.id));
                }
                return Err(format!("notarization {} (submission {})", s.status, s.id));
            }
            Err(e) => {
                let detail = [out.stdout.trim(), out.stderr.trim()].join("\n");
                return Err(format!("{e}\n{}", detail.trim()));
            }
        }
    } else {
        println!("# on rejection: {}", notary.log("<submission-id>"));
    }
    for c in &after {
        runner.run(c)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    fn strings(c: &Cmd) -> Vec<&str> {
        std::iter::once(c.program.as_str()).chain(c.args.iter().map(String::as_str)).collect()
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(shell_quote("plain/path-1.0.dmg"), "plain/path-1.0.dmg");
        assert_eq!(shell_quote("Developer ID Application: A (X1)"), "'Developer ID Application: A (X1)'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
        let c = Cmd::new("codesign").args(["--sign", "Dev ID"]).arg(p("/a b/C.app"));
        assert_eq!(c.to_string(), "codesign --sign 'Dev ID' '/a b/C.app'");
    }

    #[test]
    fn ad_hoc_signing_commands() {
        let s = Signer { identity: AD_HOC.into(), keychain: None };
        assert!(s.is_ad_hoc());
        let c = s.sign_code(&p("/b/CodeCraft.app"), Some(&p("/r/e.plist")));
        assert_eq!(
            strings(&c),
            [
                "codesign",
                "--force",
                "--sign",
                "-",
                "--timestamp=none",
                "--options",
                "runtime",
                "--entitlements",
                "/r/e.plist",
                "/b/CodeCraft.app"
            ]
        );
        assert!(!c.args.iter().any(|a| a == "--deep"), "signing never uses --deep");
    }

    #[test]
    fn developer_id_signing_commands_have_a_timestamp_and_keychain() {
        let s = Signer {
            identity: "Developer ID Application: Jane Doe (ABCDE12345)".into(),
            keychain: Some(p("/tmp/ci.keychain-db")),
        };
        let c = s.sign_code(&p("/b/CodeCraft.app"), None);
        assert_eq!(
            strings(&c),
            [
                "codesign",
                "--force",
                "--sign",
                "Developer ID Application: Jane Doe (ABCDE12345)",
                "--keychain",
                "/tmp/ci.keychain-db",
                "--timestamp",
                "--options",
                "runtime",
                "/b/CodeCraft.app"
            ]
        );
        let d = s.sign_disk_image(&p("/b/CodeCraft-1.dmg"));
        assert!(!d.args.iter().any(|a| a == "runtime" || a == "--entitlements"));
        assert_eq!(d.args.last().map(String::as_str), Some("/b/CodeCraft-1.dmg"));
        assert!(d.args.iter().any(|a| a == "--timestamp"));
        assert_eq!(strings(&verify_disk_image(&p("/x.dmg"))), ["codesign", "--verify", "--strict", "--verbose=2", "/x.dmg"]);
    }

    #[test]
    fn app_is_signed_inside_out_then_verified() {
        let s = Signer { identity: AD_HOC.into(), keychain: None };
        let app = p("/b/CodeCraft.app");
        let nested = [
            p("/b/CodeCraft.app/Contents/Helpers/helper"),
            p("/b/CodeCraft.app/Contents/Frameworks/X.framework"),
            p("/b/CodeCraft.app/Contents/Frameworks/X.framework/Versions/A/Helpers/deep"),
        ];
        let cmds = s.app_commands(&app, &nested, &p("/e.plist"));
        let targets: Vec<&str> = cmds.iter().filter_map(|c| c.args.last()).map(String::as_str).collect();
        assert_eq!(
            targets,
            [
                "/b/CodeCraft.app/Contents/Frameworks/X.framework/Versions/A/Helpers/deep",
                "/b/CodeCraft.app/Contents/Frameworks/X.framework",
                "/b/CodeCraft.app/Contents/Helpers/helper",
                "/b/CodeCraft.app",
                "/b/CodeCraft.app",
                "/b/CodeCraft.app",
            ]
        );
        // Only the app itself gets the entitlements.
        let with_ent: Vec<usize> =
            cmds.iter().enumerate().filter(|(_, c)| c.args.iter().any(|a| a == "--entitlements")).map(|(i, _)| i).collect();
        assert_eq!(with_ent, [3]);
        assert_eq!(strings(&cmds[4]), ["codesign", "--verify", "--deep", "--strict", "--verbose=2", "/b/CodeCraft.app"]);
        assert_eq!(strings(&cmds[5]), ["codesign", "--display", "--verbose=2", "/b/CodeCraft.app"]);
    }

    #[test]
    fn hardened_runtime_flag_is_read_from_codesign_display() {
        let ok = "Executable=/b/CodeCraft.app/Contents/MacOS/codecraft\nIdentifier=dev.codecraft.CodeCraft\n\
                  Format=app bundle with Mach-O thin (arm64)\n\
                  CodeDirectory v=20500 size=1234 flags=0x10002(adhoc,runtime) hashes=30+7 location=embedded\n";
        assert!(has_hardened_runtime(ok));
        let dev = "CodeDirectory v=20500 size=1 flags=0x10000(runtime) hashes=1+1 location=embedded\n";
        assert!(has_hardened_runtime(dev));
        let no = "CodeDirectory v=20400 size=1 flags=0x2(adhoc) hashes=1+1 location=embedded\n";
        assert!(!has_hardened_runtime(no));
        assert!(!has_hardened_runtime("Identifier=runtime\nflags=runtime\n"));
        assert!(!has_hardened_runtime(""));
    }

    #[test]
    fn signer_resolution() {
        let s = Signer::resolve(Some("  Developer ID Application: X  "), None);
        assert_eq!(s.identity, "Developer ID Application: X");
        assert!(!s.is_ad_hoc());
        // An explicit ad-hoc request wins over the environment.
        assert!(Signer::resolve(Some("-"), None).is_ad_hoc());
    }

    #[test]
    fn notarytool_commands_use_only_the_keychain_profile() {
        let n = Notary { profile: "codecraft".into(), keychain: None };
        assert_eq!(
            strings(&n.submit(&p("/b/CodeCraft-0.1.0.dmg"))),
            [
                "xcrun",
                "notarytool",
                "submit",
                "/b/CodeCraft-0.1.0.dmg",
                "--keychain-profile",
                "codecraft",
                "--wait",
                "--output-format",
                "json"
            ]
        );
        assert_eq!(strings(&n.log("abc-123")), ["xcrun", "notarytool", "log", "abc-123", "--keychain-profile", "codecraft"]);
        let ci = Notary { profile: "ci".into(), keychain: Some(p("/t/k.keychain-db")) };
        let s = ci.submit(&p("/x.dmg"));
        assert!(s.to_string().contains("--keychain-profile ci --keychain /t/k.keychain-db --wait"));
        for c in [n.submit(&p("/x.dmg")), n.log("i"), s] {
            for forbidden in ["--password", "--apple-id", "--key", "--team-id", "store-credentials"] {
                assert!(!c.args.iter().any(|a| a == forbidden), "{c} passes {forbidden}");
            }
        }
    }

    #[test]
    fn notarize_plans() {
        let (upload, zip, after) = notarize_plan(&Target::Dmg(p("/b/C.dmg")));
        assert_eq!(upload, p("/b/C.dmg"));
        assert!(zip.is_none());
        let after: Vec<String> = after.iter().map(ToString::to_string).collect();
        assert_eq!(after, ["xcrun stapler staple /b/C.dmg", "xcrun stapler validate /b/C.dmg", "spctl -a -vv -t install /b/C.dmg"]);

        let (upload, zip, after) = notarize_plan(&Target::App(p("/b/CodeCraft.app")));
        assert_eq!(upload, p("/b/CodeCraft.notarize.zip"));
        assert_eq!(zip.map(|z| z.to_string()).as_deref(), Some("ditto -c -k --keepParent /b/CodeCraft.app /b/CodeCraft.notarize.zip"));
        assert_eq!(after.last().map(ToString::to_string).as_deref(), Some("spctl -a -vv -t exec /b/CodeCraft.app"));
        assert_eq!(after.first().map(ToString::to_string).as_deref(), Some("xcrun stapler staple /b/CodeCraft.app"));
    }

    #[test]
    fn notarytool_output_parses() {
        let ok = r#"{"id":"2efe2717-52ef-43a5-96dc-0797e4ca1041","status":"Accepted","message":"Processing complete"}"#;
        let s = parse_submission(ok).unwrap();
        assert!(s.accepted());
        assert_eq!(s.id, "2efe2717-52ef-43a5-96dc-0797e4ca1041");
        let bad = format!("Conducting pre-submission checks…\n{}\n", ok.replace("Accepted", "Invalid"));
        let s = parse_submission(&bad).unwrap();
        assert!(!s.accepted());
        assert_eq!(s.status, "Invalid");
        assert!(parse_submission("Error: HTTP status code: 401").is_err());
        assert!(parse_submission("} {").is_err());
        assert!(parse_submission("{not json}").is_err());
    }

    #[test]
    fn dry_run_notarize_prints_and_touches_nothing() {
        let n = Notary { profile: "p".into(), keychain: None };
        let r = Runner { dry_run: true };
        notarize(&r, &n, &Target::Dmg(p("/definitely/missing.dmg"))).unwrap();
        notarize(&r, &n, &Target::App(p("/definitely/missing.app"))).unwrap();
        // A real run refuses a missing file before contacting anything.
        let real = Runner { dry_run: false };
        let e = notarize(&real, &n, &Target::Dmg(p("/definitely/missing.dmg"))).unwrap_err();
        assert!(e.contains("not found"), "{e}");
    }

    #[test]
    fn notarize_options() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let o = NotarizeOptions::parse(&args(&["--profile", "codecraft", "--dmg=/x.dmg", "--dry-run"])).unwrap();
        assert_eq!(o.profile.as_deref(), Some("codecraft"));
        assert_eq!(o.target(p("/default.dmg")), Target::Dmg(p("/x.dmg")));
        assert!(o.dry_run);
        assert_eq!(o.notary().unwrap().profile, "codecraft");
        let o = NotarizeOptions::parse(&args(&["--app", "/a.app", "--keychain", "/k"])).unwrap();
        assert_eq!(o.target(p("/d.dmg")), Target::App(p("/a.app")));
        assert_eq!(o.keychain, Some(p("/k")));
        assert_eq!(NotarizeOptions::default().target(p("/d.dmg")), Target::Dmg(p("/d.dmg")));
        assert!(NotarizeOptions::parse(&args(&["--profile"])).is_err());
        assert!(NotarizeOptions::parse(&args(&["--bogus"])).is_err());
        assert!(NotarizeOptions::parse(&args(&["--dmg", "a", "--app", "b"])).is_err());
    }

    #[test]
    fn mach_o_detection_and_nested_code() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("X.app");
        let macos = app.join("Contents/MacOS");
        let fw = app.join("Contents/Frameworks/Y.framework/Versions/A");
        std::fs::create_dir_all(&macos).unwrap();
        std::fs::create_dir_all(&fw).unwrap();
        std::fs::create_dir_all(app.join("Contents/Resources")).unwrap();
        let macho = [0xcf, 0xfa, 0xed, 0xfe, 0, 0, 0, 0];
        std::fs::write(macos.join("main"), macho).unwrap();
        std::fs::write(macos.join("helper"), macho).unwrap();
        std::fs::write(fw.join("Y"), macho).unwrap();
        std::fs::write(app.join("Contents/Resources/X.icns"), b"icns").unwrap();
        std::fs::write(app.join("Contents/Resources/tiny"), b"ab").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("A", app.join("Contents/Frameworks/Y.framework/Versions/Current")).unwrap();
        assert!(is_mach_o(&macos.join("main")));
        assert!(!is_mach_o(&app.join("Contents/Resources/X.icns")));
        assert!(!is_mach_o(&app.join("Contents/Resources/tiny")));
        assert!(!is_mach_o(&dir.path().join("missing")));
        let found = nested_code(&app, &macos.join("main")).unwrap();
        let rel: Vec<String> =
            found.iter().map(|f| f.strip_prefix(&app).unwrap().to_string_lossy().replace('\\', "/")).collect();
        assert_eq!(
            rel,
            ["Contents/Frameworks/Y.framework/Versions/A/Y", "Contents/Frameworks/Y.framework", "Contents/MacOS/helper"]
        );
        assert!(nested_code(&dir.path().join("none.app"), &macos).unwrap().is_empty());
    }

    #[test]
    fn entitlements_file_holds_exactly_the_documented_keys() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let text = std::fs::read_to_string(root.join(ENTITLEMENTS)).unwrap();
        let keys: Vec<&str> = text
            .lines()
            .filter_map(|l| l.trim().strip_prefix("<key>").and_then(|k| k.strip_suffix("</key>")))
            .collect();
        let documented: Vec<&str> = ENTITLEMENT_KEYS.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, documented);
        for forbidden in ["allow-jit", "allow-unsigned-executable-memory", "disable-library-validation", "get-task-allow"] {
            assert!(!keys.iter().any(|k| k.contains(forbidden)), "{forbidden}");
        }
        // Every entitlement is true, and documented in docs/release.md.
        assert_eq!(text.matches("<true/>").count(), keys.len());
        let release = std::fs::read_to_string(root.join("docs/release.md")).unwrap();
        for k in keys {
            assert!(release.contains(k), "docs/release.md does not explain {k}");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn entitlements_file_lints() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let out = Command::new("plutil").arg("-lint").arg(root.join(ENTITLEMENTS)).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    }

    /// Ad-hoc signs a tiny bundle (a real Mach-O main executable and a nested helper) the way
    /// `cargo xtask bundle` does, then checks `codesign --verify --deep --strict`, the hardened
    /// runtime on both, and the embedded entitlements. No keychain identity is involved.
    #[cfg(target_os = "macos")]
    #[test]
    fn ad_hoc_signs_and_verifies_a_tiny_bundle() {
        if !Path::new("/usr/bin/codesign").exists() {
            eprintln!("codesign not found; skipping");
            return;
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let icns = dir.path().join("x.icns");
        std::fs::write(&icns, "icns").unwrap();
        // Any Mach-O executable will do: /usr/bin/true is small.
        let layout = crate::package::write_app_bundle(dir.path(), Some(Path::new("/usr/bin/true")), Some(&icns)).unwrap();
        let helpers = layout.app.join("Contents/Helpers");
        std::fs::create_dir_all(&helpers).unwrap();
        std::fs::copy("/usr/bin/true", helpers.join("codecraft-helper")).unwrap();
        let signer = Signer { identity: AD_HOC.into(), keychain: None };
        sign_app(&Runner::default(), &signer, &layout.app, &layout.executable, &root.join(ENTITLEMENTS)).unwrap();

        let out = verify_app(&layout.app).command().output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        for code in [layout.app.clone(), helpers.join("codecraft-helper")] {
            let out = display(&code).command().output().unwrap();
            assert!(has_hardened_runtime(&String::from_utf8_lossy(&out.stderr)), "{}", code.display());
        }
        let ent = Command::new("codesign").args(["--display", "--entitlements", "-", "--xml"]).arg(&layout.app).output().unwrap();
        let ent = String::from_utf8_lossy(&ent.stdout);
        assert!(ent.contains("com.apple.security.automation.apple-events"), "{ent}");

        // Tampering after signing is caught.
        std::fs::write(layout.app.join("Contents/Resources/CodeCraft.icns"), "changed").unwrap();
        let out = verify_app(&layout.app).command().output().unwrap();
        assert!(!out.status.success());
    }
}
