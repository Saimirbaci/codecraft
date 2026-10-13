//! Packaging: `bundle` (macOS `.app`), `dmg`, `package-linux` (tar.gz layout with a `.desktop`
//! file) and `package-windows` (zip layout). Each builds the app with Cargo first, then lays the
//! files out under `<target dir>/bundle` or `<target dir>/package`.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::icon;

pub const BUNDLE_ID: &str = "dev.codecraft.CodeCraft";
pub const APP_NAME: &str = "CodeCraft";
/// The binary inside the bundle (`Contents/MacOS/codecraft`).
pub const EXECUTABLE: &str = "codecraft";
/// Oldest macOS the bundle declares (eframe/wgpu need Metal 2; Apple silicon starts at 11).
pub const MIN_MACOS: &str = "11.0";

/// Workspace version (xtask inherits it).
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Options shared by the packaging tasks.
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub release: bool,
    /// Lay out the package without building or signing (CI dry runs, tests).
    pub layout_only: bool,
}

impl Options {
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut o = Self::default();
        for a in args {
            match a.as_str() {
                "--release" => o.release = true,
                "--layout-only" => o.layout_only = true,
                other => return Err(format!("unknown option {other} (expected --release, --layout-only)")),
            }
        }
        Ok(o)
    }

    fn profile(&self) -> &'static str {
        if self.release { "release" } else { "debug" }
    }
}

/// Cargo's target directory: `CARGO_TARGET_DIR` (relative to the workspace) or `<root>/target`.
pub fn target_dir(root: &Path) -> PathBuf {
    match std::env::var_os("CARGO_TARGET_DIR") {
        Some(d) if !d.is_empty() => root.join(d),
        _ => root.join("target"),
    }
}

fn run(cmd: &mut Command) -> Result<(), String> {
    let shown = format!("{cmd:?}");
    let status = cmd.status().map_err(|e| format!("{shown}: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("{shown} failed: {status}")) }
}

fn cargo_build(root: &Path, opts: &Options, packages: &[&str]) -> Result<(), String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.current_dir(root).arg("build");
    for p in packages {
        cmd.args(["-p", p]);
    }
    if opts.release {
        cmd.arg("--release");
    }
    run(&mut cmd)
}

fn create_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))
}

fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> Result<(), String> {
    let path = path.as_ref();
    std::fs::write(path, contents).map_err(|e| format!("{}: {e}", path.display()))
}

fn copy(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::copy(from, to).map(|_| ()).map_err(|e| format!("{} → {}: {e}", from.display(), to.display()))
}

fn remove_dir(dir: &Path) -> Result<(), String> {
    if dir.exists() {
        std::fs::remove_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?.permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(path, perm).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

// ---------------------------------------------------------------- macOS

/// `Contents/Info.plist` for the bundle.
pub fn info_plist(version: &str) -> String {
    let v = xml_escape(version);
    let strings = [
        ("CFBundleDevelopmentRegion", "en"),
        ("CFBundleDisplayName", APP_NAME),
        ("CFBundleExecutable", EXECUTABLE),
        ("CFBundleIconFile", APP_NAME),
        ("CFBundleIdentifier", BUNDLE_ID),
        ("CFBundleInfoDictionaryVersion", "6.0"),
        ("CFBundleName", APP_NAME),
        ("CFBundlePackageType", "APPL"),
        ("CFBundleShortVersionString", &v),
        ("CFBundleSignature", "????"),
        ("CFBundleVersion", &v),
        ("LSApplicationCategoryType", "public.app-category.developer-tools"),
        ("LSMinimumSystemVersion", MIN_MACOS),
        ("NSHumanReadableCopyright", "MIT OR Apache-2.0"),
    ];
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n",
    );
    for (k, val) in strings {
        out.push_str(&format!("\t<key>{k}</key>\n\t<string>{val}</string>\n"));
    }
    for k in ["NSHighResolutionCapable", "NSSupportsAutomaticGraphicsSwitching"] {
        out.push_str(&format!("\t<key>{k}</key>\n\t<true/>\n"));
    }
    out.push_str("</dict>\n</plist>\n");
    out
}

/// The paths inside `CodeCraft.app`.
pub struct AppLayout {
    pub app: PathBuf,
    pub executable: PathBuf,
    pub info_plist: PathBuf,
    pub icns: PathBuf,
}

impl AppLayout {
    pub fn new(bundle_dir: &Path) -> Self {
        let app = bundle_dir.join(format!("{APP_NAME}.app"));
        let contents = app.join("Contents");
        Self {
            executable: contents.join("MacOS").join(EXECUTABLE),
            info_plist: contents.join("Info.plist"),
            icns: contents.join("Resources").join(format!("{APP_NAME}.icns")),
            app,
        }
    }
}

/// Lay out `CodeCraft.app` in `bundle_dir` from a built binary (`None`: lay out without one) and
/// an `.icns` file (`None`: generate the icon set and convert it with `iconutil` when present).
/// Replaces an existing bundle.
pub fn write_app_bundle(bundle_dir: &Path, binary: Option<&Path>, icns: Option<&Path>) -> Result<AppLayout, String> {
    let layout = AppLayout::new(bundle_dir);
    remove_dir(&layout.app)?;
    let contents = layout.app.join("Contents");
    let macos = contents.join("MacOS");
    let resources = contents.join("Resources");
    create_dir(&macos)?;
    create_dir(&resources)?;
    write(&layout.info_plist, info_plist(version()))?;
    write(contents.join("PkgInfo"), "APPL????")?;
    if let Some(bin) = binary {
        copy(bin, &layout.executable)?;
        make_executable(&layout.executable)?;
    }
    match icns {
        Some(src) => copy(src, &layout.icns)?,
        None => {
            let iconset = bundle_dir.join(format!("{APP_NAME}.iconset"));
            remove_dir(&iconset)?;
            icon::write_iconset(&iconset)?;
            if Path::new("/usr/bin/iconutil").exists() {
                run(Command::new("/usr/bin/iconutil").arg("-c").arg("icns").arg(&iconset).arg("-o").arg(&layout.icns))?;
                remove_dir(&iconset)?;
            } else {
                // No iconutil (not macOS): keep the PNGs so the layout is still inspectable.
                eprintln!("iconutil not found; left {} unconverted", iconset.display());
            }
        }
    }
    Ok(layout)
}

/// Sign the bundle: `CODECRAFT_SIGN_IDENTITY` if set, else ad hoc (`-`); then verify.
pub fn codesign(app: &Path) -> Result<(), String> {
    let identity = std::env::var("CODECRAFT_SIGN_IDENTITY").ok().filter(|s| !s.trim().is_empty());
    let identity = identity.as_deref().unwrap_or("-");
    let mut sign = Command::new("codesign");
    sign.args(["--force", "--deep", "--sign", identity]);
    if identity != "-" {
        // Real identities get the hardened runtime and a timestamp (needed for notarization).
        sign.args(["--options", "runtime", "--timestamp"]);
    }
    run(sign.arg(app))?;
    run(Command::new("codesign").args(["--verify", "--deep", "--strict"]).arg(app))
}

/// `cargo xtask bundle`: build, lay out, sign. Returns the bundle path.
pub fn bundle(root: &Path, opts: &Options) -> Result<PathBuf, String> {
    let target = target_dir(root);
    let binary = target.join(opts.profile()).join(EXECUTABLE);
    if !opts.layout_only {
        cargo_build(root, opts, &["codecraft"])?;
    }
    let bundle_dir = target.join("bundle");
    let bin = (!opts.layout_only).then_some(binary.as_path());
    let layout = write_app_bundle(&bundle_dir, bin, None)?;
    if !opts.layout_only {
        if cfg!(target_os = "macos") {
            codesign(&layout.app)?;
        } else {
            eprintln!("not on macOS: the bundle is not signed");
        }
    }
    println!("{}", layout.app.display());
    Ok(layout.app)
}

/// `cargo xtask dmg`: the bundle plus an `/Applications` link in a compressed disk image.
pub fn dmg(root: &Path, opts: &Options) -> Result<PathBuf, String> {
    if !cfg!(target_os = "macos") {
        return Err("dmg needs macOS (hdiutil)".into());
    }
    let app = bundle(root, opts)?;
    let bundle_dir = target_dir(root).join("bundle");
    let staging = bundle_dir.join("dmg-staging");
    remove_dir(&staging)?;
    create_dir(&staging)?;
    run(Command::new("ditto").arg(&app).arg(staging.join(format!("{APP_NAME}.app"))))?;
    #[cfg(unix)]
    std::os::unix::fs::symlink("/Applications", staging.join("Applications"))
        .map_err(|e| format!("Applications link: {e}"))?;
    let image = bundle_dir.join(format!("{APP_NAME}-{}.dmg", version()));
    run(Command::new("hdiutil")
        .args(["create", "-volname", APP_NAME, "-fs", "HFS+", "-format", "UDZO", "-ov", "-srcfolder"])
        .arg(&staging)
        .arg(&image))?;
    remove_dir(&staging)?;
    println!("{}", image.display());
    Ok(image)
}

// ---------------------------------------------------------------- Linux

/// The freedesktop `.desktop` entry.
pub fn desktop_entry() -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={APP_NAME}\n\
         GenericName=Rust IDE\n\
         Comment=A native Rust IDE\n\
         Exec=codecraft %f\n\
         Icon={BUNDLE_ID}\n\
         Terminal=false\n\
         Categories=Development;IDE;TextEditor;\n\
         Keywords=rust;cargo;ide;editor;\n\
         StartupNotify=true\n"
    )
}

const LINUX_README: &str = "CodeCraft for Linux\n\
\n\
Install for the current user:\n\
\n\
    cp -r bin share ~/.local/\n\
\n\
or system-wide with `sudo cp -r bin share /usr/local/`. The desktop entry and icon go to\n\
share/applications and share/icons; `codecraft` and `codecraft-cli` go to bin.\n";

fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => other,
    }
}

/// Lay out the Linux package in `dir` (replaced). `bin_dir`: where the built binaries are
/// (`None`: no binaries, a layout dry run).
pub fn write_linux_layout(dir: &Path, root: &Path, bin_dir: Option<&Path>) -> Result<(), String> {
    remove_dir(dir)?;
    let bin = dir.join("bin");
    let apps = dir.join("share").join("applications");
    create_dir(&bin)?;
    create_dir(&apps)?;
    write(apps.join(format!("{BUNDLE_ID}.desktop")), desktop_entry())?;
    for size in [64u32, 128, 256, 512] {
        let icons = dir.join(format!("share/icons/hicolor/{size}x{size}/apps"));
        create_dir(&icons)?;
        icon::write_png(&icons.join(format!("{BUNDLE_ID}.png")), size)?;
    }
    if let Some(src) = bin_dir {
        for exe in ["codecraft", "codecraft-cli"] {
            copy(&src.join(exe), &bin.join(exe))?;
            make_executable(&bin.join(exe))?;
        }
    }
    write(dir.join("README.txt"), LINUX_README)?;
    copy_docs(root, dir)
}

fn copy_docs(root: &Path, dir: &Path) -> Result<(), String> {
    for f in ["README.md", "LICENSE-MIT", "LICENSE-APACHE"] {
        let src = root.join(f);
        if src.exists() {
            copy(&src, &dir.join(f))?;
        }
    }
    Ok(())
}

/// `cargo xtask package-linux`: on Linux, build and write `codecraft-<version>-linux-<arch>.tar.gz`;
/// elsewhere (no cross-compiling), only the layout without binaries.
pub fn package_linux(root: &Path, opts: &Options) -> Result<PathBuf, String> {
    let target = target_dir(root);
    let name = format!("codecraft-{}-linux-{}", version(), arch());
    let out = target.join("package");
    let dir = out.join(&name);
    let build = cfg!(target_os = "linux") && !opts.layout_only;
    if build {
        cargo_build(root, opts, &["codecraft", "codecraft-cli"])?;
    } else {
        eprintln!("layout only (binaries are built on Linux)");
    }
    let bins = target.join(opts.profile());
    write_linux_layout(&dir, root, build.then_some(bins.as_path()))?;
    if !build {
        println!("{}", dir.display());
        return Ok(dir);
    }
    let tarball = out.join(format!("{name}.tar.gz"));
    run(Command::new("tar").arg("-czf").arg(&tarball).arg("-C").arg(&out).arg(&name))?;
    println!("{}", tarball.display());
    Ok(tarball)
}

// ---------------------------------------------------------------- Windows

/// Lay out the Windows package in `dir` (replaced): `codecraft.exe`, `codecraft-cli.exe`,
/// `codecraft.png`, README and licenses.
pub fn write_windows_layout(dir: &Path, root: &Path, bin_dir: Option<&Path>) -> Result<(), String> {
    remove_dir(dir)?;
    create_dir(dir)?;
    if let Some(src) = bin_dir {
        for exe in ["codecraft.exe", "codecraft-cli.exe"] {
            copy(&src.join(exe), &dir.join(exe))?;
        }
    }
    icon::write_png(&dir.join("codecraft.png"), 256)?;
    copy_docs(root, dir)
}

/// `cargo xtask package-windows`: on Windows, build and zip `codecraft-<version>-windows-<arch>.zip`
/// (with the `tar` that ships with Windows 10+); elsewhere only the layout. Not exercised on
/// Windows yet.
pub fn package_windows(root: &Path, opts: &Options) -> Result<PathBuf, String> {
    let target = target_dir(root);
    let name = format!("codecraft-{}-windows-{}", version(), arch());
    let out = target.join("package");
    let dir = out.join(&name);
    let build = cfg!(windows) && !opts.layout_only;
    if build {
        cargo_build(root, opts, &["codecraft", "codecraft-cli"])?;
    } else {
        eprintln!("layout only (binaries are built on Windows)");
    }
    let bins = target.join(opts.profile());
    write_windows_layout(&dir, root, build.then_some(bins.as_path()))?;
    if !build {
        println!("{}", dir.display());
        return Ok(dir);
    }
    let zip = out.join(format!("{name}.zip"));
    run(Command::new("tar").arg("-a").arg("-c").arg("-f").arg(&zip).arg("-C").arg(&out).arg(&name))?;
    println!("{}", zip.display());
    Ok(zip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_plist_has_the_required_keys() {
        let p = info_plist("1.2.3");
        for needle in [
            "<key>CFBundleIdentifier</key>\n\t<string>dev.codecraft.CodeCraft</string>",
            "<key>CFBundleExecutable</key>\n\t<string>codecraft</string>",
            "<key>CFBundleShortVersionString</key>\n\t<string>1.2.3</string>",
            "<key>CFBundleVersion</key>\n\t<string>1.2.3</string>",
            "<key>CFBundleIconFile</key>\n\t<string>CodeCraft</string>",
            "<key>LSMinimumSystemVersion</key>\n\t<string>11.0</string>",
            "<key>NSHighResolutionCapable</key>\n\t<true/>",
            "<key>CFBundlePackageType</key>\n\t<string>APPL</string>",
        ] {
            assert!(p.contains(needle), "missing {needle}");
        }
        assert!(p.starts_with("<?xml"));
        assert!(p.trim_end().ends_with("</plist>"));
        assert!(info_plist("1<2").contains("1&lt;2"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn info_plist_lints() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Info.plist");
        std::fs::write(&path, info_plist(version())).unwrap();
        let out = Command::new("plutil").arg("-lint").arg(&path).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    }

    #[test]
    fn options_parse() {
        let o = Options::parse(&["--release".into()]).unwrap();
        assert!(o.release && !o.layout_only);
        assert!(Options::parse(&["--bogus".into()]).is_err());
    }

    #[test]
    fn app_bundle_layout() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fake-binary");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        let icns = dir.path().join("fake.icns");
        std::fs::write(&icns, "icns").unwrap();
        let bundle_dir = dir.path().join("bundle");
        let layout = write_app_bundle(&bundle_dir, Some(&bin), Some(&icns)).unwrap();
        assert_eq!(layout.app, bundle_dir.join("CodeCraft.app"));
        assert!(layout.executable.ends_with("CodeCraft.app/Contents/MacOS/codecraft"));
        assert!(layout.executable.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&layout.executable).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111);
        }
        assert_eq!(std::fs::read_to_string(&layout.info_plist).unwrap(), info_plist(version()));
        assert_eq!(std::fs::read_to_string(layout.app.join("Contents/PkgInfo")).unwrap(), "APPL????");
        assert!(layout.icns.ends_with("Contents/Resources/CodeCraft.icns"));
        assert!(layout.icns.is_file());
        // Re-bundling replaces the old bundle (no stale files).
        std::fs::write(layout.app.join("Contents/stale"), "x").unwrap();
        write_app_bundle(&bundle_dir, Some(&bin), Some(&icns)).unwrap();
        assert!(!layout.app.join("Contents/stale").exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn generated_icon_converts_to_icns() {
        let dir = tempfile::tempdir().unwrap();
        let layout = write_app_bundle(dir.path(), None, None).unwrap();
        let bytes = std::fs::read(&layout.icns).unwrap();
        assert_eq!(&bytes[..4], b"icns");
        assert!(!dir.path().join("CodeCraft.iconset").exists());
    }

    #[test]
    fn linux_layout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("README.md"), "# hi").unwrap();
        let bins = dir.path().join("bins");
        std::fs::create_dir_all(&bins).unwrap();
        for b in ["codecraft", "codecraft-cli"] {
            std::fs::write(bins.join(b), "bin").unwrap();
        }
        let out = dir.path().join("pkg");
        write_linux_layout(&out, &root, Some(&bins)).unwrap();
        let desktop = std::fs::read_to_string(out.join("share/applications/dev.codecraft.CodeCraft.desktop")).unwrap();
        assert!(desktop.starts_with("[Desktop Entry]\n"));
        assert!(desktop.contains("\nExec=codecraft %f\n"));
        assert!(desktop.contains("\nIcon=dev.codecraft.CodeCraft\n"));
        assert!(out.join("share/icons/hicolor/256x256/apps/dev.codecraft.CodeCraft.png").is_file());
        assert!(out.join("bin/codecraft").is_file());
        assert!(out.join("bin/codecraft-cli").is_file());
        assert!(out.join("README.md").is_file());
        assert!(out.join("README.txt").is_file());
        // Dry run: no binaries.
        write_linux_layout(&out, &root, None).unwrap();
        assert!(!out.join("bin/codecraft").exists());
        assert!(out.join("bin").is_dir());
    }

    #[test]
    fn windows_layout() {
        let dir = tempfile::tempdir().unwrap();
        let bins = dir.path().join("bins");
        std::fs::create_dir_all(&bins).unwrap();
        for b in ["codecraft.exe", "codecraft-cli.exe"] {
            std::fs::write(bins.join(b), "exe").unwrap();
        }
        let out = dir.path().join("win");
        write_windows_layout(&out, dir.path(), Some(&bins)).unwrap();
        assert!(out.join("codecraft.exe").is_file());
        assert!(out.join("codecraft-cli.exe").is_file());
        assert!(out.join("codecraft.png").is_file());
    }
}
