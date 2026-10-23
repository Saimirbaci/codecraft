//! Packaging: `bundle` (macOS `.app`), `dmg`, `package-linux` (tar.gz layout with a `.desktop`
//! file), `package-deb` (Debian/Ubuntu package), `package-windows` (zip layout) and
//! `package-windows-installer` (Inno Setup installer). Each builds the app with Cargo first, then
//! lays the files out under `<target dir>/bundle` or `<target dir>/package`.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::icon;
use crate::sign::{self, Cmd, Runner, Signer};

pub const BUNDLE_ID: &str = "dev.codecraft.CodeCraft";
/// File names (`CodeCraft.app`, `CodeCraft-<version>.dmg`).
pub const APP_NAME: &str = "CodeCraft";
/// The name people see (menu bar, Dock, launchers, installer); equal to the engine's
/// `about::DISPLAY_NAME` (a test checks).
pub const DISPLAY_NAME: &str = "Code Craft";
pub const COPYRIGHT: &str = "Copyright (c) 2026 The CodeCraft contributors. MIT License.";
pub const HOMEPAGE: &str = "https://github.com/Saimirbaci/codecraft";
/// Inno Setup's AppId: identifies the installed app for upgrades and uninstall. Never change it.
pub const WINDOWS_APP_ID: &str = "{8E0C5B7A-3F2D-4C1E-9A6B-2D7F4E8C1A35}";
/// The binary inside the bundle (`Contents/MacOS/codecraft`).
pub const EXECUTABLE: &str = "codecraft";
/// Oldest macOS the bundle declares (eframe/wgpu need Metal 2; Apple silicon starts at 11).
pub const MIN_MACOS: &str = "11.0";

/// Why programs run from CodeCraft may control other apps (the Apple Events consent prompt).
pub const APPLE_EVENTS_USAGE: &str =
    "Programs you run from CodeCraft's terminal or run configurations may control other apps.";

/// Workspace version (xtask inherits it).
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Options shared by the packaging tasks.
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub release: bool,
    pub build_profile: Option<String>,
    pub version_payload: Option<PathBuf>,
    /// Lay out the package without building or signing (CI dry runs, tests).
    pub layout_only: bool,
    /// Print the build, signing and packaging commands without running them or writing files.
    pub dry_run: bool,
    /// `--sign <identity>`: the codesign identity (`-` ad hoc, the default; else e.g.
    /// `Developer ID Application: Name (TEAMID)`). Falls back to `$CODECRAFT_SIGN_IDENTITY`.
    pub sign: Option<String>,
    /// `--keychain <path>`: where codesign finds the identity (CI's temporary keychain).
    pub keychain: Option<PathBuf>,
}

impl Options {
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
                "--version-payload" => o.version_payload = Some(PathBuf::from(value("--version-payload")?)),
                "--release" => o.release = true,
                "--profile" => {
                    let p = value("--profile")?;
                    if p.is_empty() || !p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                        return Err("invalid build profile".into());
                    }
                    o.build_profile = Some(p);
                }
                "--layout-only" => o.layout_only = true,
                "--dry-run" => o.dry_run = true,
                "--sign" => o.sign = Some(value("--sign")?),
                "--keychain" => o.keychain = Some(PathBuf::from(value("--keychain")?)),
                other => {
                    return Err(format!(
                        "unknown option {other} (expected --release, --layout-only, --dry-run, --sign <identity>, --keychain <path>)"
                    ));
                }
            }
        }
        Ok(o)
    }

    fn profile(&self) -> &str {
        if let Some(p) = &self.build_profile {
            return match p.as_str() { "dev"|"test"=>"debug", "bench"=>"release", _=>p };
        }
        if self.release { "release" } else { "debug" }
    }

    fn runner(&self) -> Runner {
        Runner { dry_run: self.dry_run }
    }

    pub fn signer(&self) -> Signer {
        Signer::resolve(self.sign.as_deref(), self.keychain.as_deref())
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

/// `cargo build -p … [--release]` (the `cargo` running xtask).
pub fn cargo_build_cmd(opts: &Options, packages: &[&str]) -> Cmd {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = Cmd::new(&cargo).arg("build");
    for p in packages {
        cmd = cmd.args(["-p", p]);
    }
    if opts.version_payload.is_some() { cmd = cmd.arg("--locked"); }
    if let Some(p) = &opts.build_profile {
        cmd = cmd.args(["--profile", p]);
        if opts.version_payload.is_some() {
            cmd = cmd.arg("--message-format=json-render-diagnostics");
        }
    } else if opts.release {
        cmd = cmd.arg("--release");
    }
    cmd
}

fn cargo_build(root: &Path, opts: &Options, packages: &[&str]) -> Result<(), String> {
    let cmd = cargo_build_cmd(opts, packages);
    if opts.dry_run {
        return opts.runner().run(&cmd);
    }
    println!("$ {cmd}");
    run(cmd.command().current_dir(root))
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
        ("CFBundleDisplayName", DISPLAY_NAME),
        ("CFBundleExecutable", EXECUTABLE),
        ("CFBundleIconFile", APP_NAME),
        ("CFBundleIdentifier", BUNDLE_ID),
        ("CFBundleInfoDictionaryVersion", "6.0"),
        ("CFBundleName", DISPLAY_NAME),
        ("CFBundlePackageType", "APPL"),
        ("CFBundleShortVersionString", &v),
        ("CFBundleSignature", "????"),
        ("CFBundleVersion", &v),
        ("LSApplicationCategoryType", "public.app-category.developer-tools"),
        ("LSMinimumSystemVersion", MIN_MACOS),
        // Shown when a program run from CodeCraft (Terminal, run configurations) sends Apple Events;
        // pairs with the apple-events entitlement (apps/codecraft/macos/CodeCraft.entitlements).
        ("NSAppleEventsUsageDescription", APPLE_EVENTS_USAGE),
        ("NSHumanReadableCopyright", COPYRIGHT),
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
/// an `.icns` file (`None`: generate a native ICNS container directly in Rust).
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
        None => icon::write_icns(&layout.icns)?,
    }

    Ok(layout)
}

/// Sign the bundle inside-out with the hardened runtime and the entitlements, then verify it.
pub fn sign_bundle(root: &Path, opts: &Options, layout: &AppLayout) -> Result<(), String> {
    let signer = opts.signer();
    if signer.is_ad_hoc() {
        eprintln!("signing ad hoc (runs on this Mac only; pass --sign <identity> to distribute)");
    }
    let entitlements = root.join(sign::ENTITLEMENTS);
    for (key, why) in sign::ENTITLEMENT_KEYS {
        eprintln!("entitlement {key}: {why}");
    }
    if layout.executable.file_name().is_some_and(|n| n == "codecraft-launcher") {
        sign::sign_prepared_app(&opts.runner(), &signer, &layout.app, &entitlements)
    } else {
        sign::sign_app(&opts.runner(), &signer, &layout.app, &layout.executable, &entitlements)
    }
}

/// Stable release layout: background launcher outside, full foreground factory app inside.
pub fn write_launcher_bundle(
    bundle_dir: &Path,
    app_binary: Option<&Path>,
    launcher: Option<&Path>,
    icns: Option<&Path>,
) -> Result<AppLayout, String> {
    let mut layout = write_app_bundle(bundle_dir, None, icns)?;
    let factory_dir = layout.app.join("Contents/Resources/factory");
    write_app_bundle(&factory_dir, app_binary, Some(layout.icns.as_path()).filter(|p| p.is_file()))?;
    layout.executable = layout.app.join("Contents/MacOS/codecraft-launcher");
    if let Some(binary) = launcher { copy(binary, &layout.executable)?; make_executable(&layout.executable)?; }
    let plist = info_plist(version()).replace("<string>codecraft</string>", "<string>codecraft-launcher</string>")
        .replace("</dict>", "<key>LSUIElement</key><true/>\n</dict>");
    write(&layout.info_plist, plist)?;
    Ok(layout)
}

/// Build a stable release or a standalone version payload (`--version-payload <directory>`).
pub fn bundle(root: &Path, opts: &Options) -> Result<PathBuf, String> {
    let target = target_dir(root);
    let binary = target.join(opts.profile()).join(EXECUTABLE);
    let launcher = target.join(opts.profile()).join("codecraft-launcher");
    let bundle_dir = opts.version_payload.clone().unwrap_or_else(|| target.join("bundle"));
    let packages = if opts.version_payload.is_some() { vec!["codecraft"] } else { vec!["codecraft", "codecraft-launcher"] };
    if !opts.layout_only { cargo_build(root, opts, &packages)?; }
    let payload_layout = AppLayout::new(&bundle_dir);
    if opts.dry_run {
        println!("# lay out {} (factory payload + stable launcher unless --version-payload)", payload_layout.app.display());
        if !opts.layout_only {
            if opts.version_payload.is_none() {
                let factory = AppLayout::new(&payload_layout.app.join("Contents/Resources/factory"));
                sign_bundle(root, opts, &factory)?;
                let mut stable = AppLayout::new(&bundle_dir);
                stable.executable = stable.app.join("Contents/MacOS/codecraft-launcher");
                sign_bundle(root, opts, &stable)?;
            } else { sign_bundle(root, opts, &payload_layout)?; }
        }
        return Ok(payload_layout.app);
    }
    let bin = (!opts.layout_only).then_some(binary.as_path());
    let layout = if opts.version_payload.is_some() {
        write_app_bundle(&bundle_dir, bin, None)?
    } else {
        write_launcher_bundle(&bundle_dir, bin, (!opts.layout_only).then_some(launcher.as_path()), None)?
    };
    if !opts.layout_only && cfg!(target_os = "macos") {
        if opts.version_payload.is_none() {
            sign_bundle(root, opts, &AppLayout::new(&layout.app.join("Contents/Resources/factory")))?;
        }
        sign_bundle(root, opts, &layout)?;
    }
    println!("{}", layout.app.display()); Ok(layout.app)
}

/// The disk image `dmg` writes: `<target>/bundle/CodeCraft-<version>.dmg`.
pub fn dmg_path(root: &Path) -> PathBuf {
    target_dir(root).join("bundle").join(format!("{APP_NAME}-{}.dmg", version()))
}

/// `hdiutil create`: the staging folder as a compressed (UDZO) image.
pub fn hdiutil_create(staging: &Path, image: &Path) -> Cmd {
    Cmd::new("hdiutil")
        .args(["create", "-volname", DISPLAY_NAME, "-fs", "HFS+", "-format", "UDZO", "-ov", "-srcfolder"])
        .arg(staging)
        .arg(image)
}

/// `cargo xtask dmg`: the signed bundle plus an `/Applications` link in a compressed disk image,
/// itself signed with the same identity and verified.
pub fn dmg(root: &Path, opts: &Options) -> Result<PathBuf, String> {
    if !cfg!(target_os = "macos") && !opts.dry_run {
        return Err("dmg needs macOS (hdiutil)".into());
    }
    if opts.layout_only {
        return Err("dmg needs a built, signed bundle (drop --layout-only)".into());
    }
    let app = bundle(root, opts)?;
    let bundle_dir = target_dir(root).join("bundle");
    let staging = bundle_dir.join("dmg-staging");
    let image = dmg_path(root);
    let runner = opts.runner();
    let ditto = Cmd::new("ditto").arg(&app).arg(staging.join(format!("{APP_NAME}.app")));
    if opts.dry_run {
        runner.run(&ditto)?;
        println!("# ln -s /Applications {}", staging.join("Applications").display());
        runner.run(&hdiutil_create(&staging, &image))?;
    } else {
        remove_dir(&staging)?;
        create_dir(&staging)?;
        runner.run(&ditto)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink("/Applications", staging.join("Applications"))
            .map_err(|e| format!("Applications link: {e}"))?;
        runner.run(&hdiutil_create(&staging, &image))?;
        remove_dir(&staging)?;
    }
    sign::sign_dmg(&runner, &opts.signer(), &image)?;
    println!("{}", image.display());
    Ok(image)
}

/// `cargo xtask notarize`: submit the dmg (or `--app`) with the user's notarytool keychain profile,
/// wait, staple and check with Gatekeeper. Never handles credentials itself.
pub fn notarize(root: &Path, opts: &sign::NotarizeOptions) -> Result<(), String> {
    if !cfg!(target_os = "macos") && !opts.dry_run {
        return Err("notarize needs macOS (xcrun notarytool, stapler, spctl)".into());
    }
    let notary = opts.notary()?;
    let target = opts.target(dmg_path(root));
    sign::notarize(&Runner { dry_run: opts.dry_run }, &notary, &target)
}

// ---------------------------------------------------------------- Linux

/// The freedesktop `.desktop` entry.
pub fn desktop_entry() -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={DISPLAY_NAME}\n\
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

const LINUX_README: &str = "Code Craft for Linux\n\
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
    write_unix_tree(dir, bin_dir)?;
    write(dir.join("README.txt"), LINUX_README)?;
    copy_docs(root, dir)
}

/// `bin/` (the binaries, when built) and `share/` (desktop entry, icons) under `prefix`.
fn write_unix_tree(prefix: &Path, bin_dir: Option<&Path>) -> Result<(), String> {
    let bin = prefix.join("bin");
    let apps = prefix.join("share").join("applications");
    create_dir(&bin)?;
    create_dir(&apps)?;
    write(apps.join(format!("{BUNDLE_ID}.desktop")), desktop_entry())?;
    for size in [64u32, 128, 256, 512] {
        let icons = prefix.join(format!("share/icons/hicolor/{size}x{size}/apps"));
        create_dir(&icons)?;
        icon::write_png(&icons.join(format!("{BUNDLE_ID}.png")), size)?;
    }
    create_dir(&bin.join("factory"))?;
    if let Some(src) = bin_dir {
        copy(&src.join("codecraft"), &bin.join("factory/codecraft"))?;
        make_executable(&bin.join("factory/codecraft"))?;
        copy(&src.join("codecraft-launcher"), &bin.join("codecraft"))?;
        make_executable(&bin.join("codecraft"))?;
        copy(&src.join("codecraft-cli"), &bin.join("codecraft-cli"))?;
        make_executable(&bin.join("codecraft-cli"))?;
    }
    Ok(())
}

fn copy_docs(root: &Path, dir: &Path) -> Result<(), String> {
    for f in ["README.md", "LICENSE"] {
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
        cargo_build(root, opts, &["codecraft", "codecraft-cli", "codecraft-launcher"])?;
    } else {
        eprintln!("layout only (binaries are built on Linux)");
    }
    let tarball = out.join(format!("{name}.tar.gz"));
    let tar = Cmd::new("tar").arg("-czf").arg(&tarball).arg("-C").arg(&out).arg(&name);
    if opts.dry_run {
        println!("# lay out {}", dir.display());
        if build {
            opts.runner().run(&tar)?;
        }
        return Ok(if build { tarball } else { dir });
    }
    let bins = target.join(opts.profile());
    write_linux_layout(&dir, root, build.then_some(bins.as_path()))?;
    if !build {
        println!("{}", dir.display());
        return Ok(dir);
    }
    opts.runner().run(&tar)?;
    println!("{}", tarball.display());
    Ok(tarball)
}

// ---------------------------------------------------------------- Debian / Ubuntu

/// Debian's name for the build machine's architecture.
pub fn deb_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// A Debian upstream version: without a revision it may not contain `-`, and `~` sorts a
/// pre-release before its release (`0.3.0~beta.1` < `0.3.0`).
pub fn deb_version(version: &str) -> String {
    version.replace('-', "~")
}

/// `DEBIAN/control`. The windowing, OpenGL and keyboard libraries are loaded at run time, so
/// they are listed by hand (dpkg-shlibdeps can't see them); OpenSSL is linked (TLS of the
/// SQL Server and ClickHouse drivers). `libc6 (>= 2.35)`: the release is built on Ubuntu 22.04.
pub fn deb_control(version: &str, arch: &str, installed_kib: u64) -> String {
    format!(
        "Package: codecraft\n\
         Version: {}\n\
         Section: devel\n\
         Priority: optional\n\
         Architecture: {arch}\n\
         Maintainer: The CodeCraft contributors <{HOMEPAGE}/issues>\n\
         Installed-Size: {installed_kib}\n\
         Depends: libc6 (>= 2.35), libgcc-s1, libssl3 | libssl3t64, libgl1, libegl1, libxkbcommon0\n\
         Recommends: libwayland-client0, libwayland-egl1, libxkbcommon-x11-0, libx11-6, libxcursor1, libxi6, libxrandr2, git\n\
         Suggests: rust-analyzer, gdb, lldb\n\
         Homepage: {HOMEPAGE}\n\
         Description: {DISPLAY_NAME}, a native IDE written in Rust\n \
         A fast native IDE for Rust, Python, C/C++, SQL and more, with debugging,\n \
         code analysis, profiling and databases, drivable by coding agents.\n",
        deb_version(version)
    )
}

/// `usr/share/doc/codecraft/copyright` in Debian's machine-readable format.
fn deb_copyright(license: &str) -> String {
    let mut out = format!(
        "Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/\n\
         Upstream-Name: {DISPLAY_NAME}\n\
         Source: {HOMEPAGE}\n\n\
         Files: *\n\
         Copyright: 2026 The CodeCraft contributors\n\
         License: MIT\n"
    );
    for line in license.lines().skip_while(|l| !l.starts_with("Permission")) {
        out.push_str(if line.trim().is_empty() { " .\n" } else { " " });
        if !line.trim().is_empty() {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Total size of the files under `dir` in KiB (rounded up), for `Installed-Size`.
fn tree_kib(dir: &Path) -> u64 {
    fn walk(dir: &Path) -> u64 {
        let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
        entries
            .filter_map(Result::ok)
            .map(|e| match e.file_type() {
                Ok(t) if t.is_dir() => walk(&e.path()),
                Ok(_) => e.metadata().map(|m| m.len()).unwrap_or(0),
                Err(_) => 0,
            })
            .sum()
    }
    walk(dir).div_ceil(1024)
}

/// Lay out the Debian package tree in `dir` (replaced): `usr/…` and `DEBIAN/control`.
pub fn write_deb_layout(dir: &Path, root: &Path, bin_dir: Option<&Path>) -> Result<(), String> {
    remove_dir(dir)?;
    let usr = dir.join("usr");
    write_unix_tree(&usr, bin_dir)?;
    let doc = usr.join("share/doc/codecraft");
    create_dir(&doc)?;
    let license = std::fs::read_to_string(root.join("LICENSE")).unwrap_or_default();
    write(doc.join("copyright"), deb_copyright(&license))?;
    let debian = dir.join("DEBIAN");
    create_dir(&debian)?;
    write(debian.join("control"), deb_control(version(), deb_arch(), tree_kib(&usr)))?;
    #[cfg(unix)]
    {
        // dpkg-deb wants 0755 directories whatever the umask was.
        use std::os::unix::fs::PermissionsExt;
        fn dirs(d: &Path) {
            let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o755));
            if let Ok(entries) = std::fs::read_dir(d) {
                for e in entries.filter_map(Result::ok) {
                    if e.file_type().is_ok_and(|t| t.is_dir()) {
                        dirs(&e.path());
                    }
                }
            }
        }
        dirs(dir);
    }
    Ok(())
}

/// `cargo xtask package-deb`: on Linux, build and write `codecraft_<version>_<arch>.deb` with
/// `dpkg-deb`; elsewhere only the layout without binaries.
pub fn package_deb(root: &Path, opts: &Options) -> Result<PathBuf, String> {
    let target = target_dir(root);
    let name = format!("codecraft_{}_{}", deb_version(version()), deb_arch());
    let out = target.join("package");
    let dir = out.join("deb").join(&name);
    let build = cfg!(target_os = "linux") && !opts.layout_only;
    if build {
        cargo_build(root, opts, &["codecraft", "codecraft-cli", "codecraft-launcher"])?;
    } else {
        eprintln!("layout only (binaries are built on Linux)");
    }
    let deb = out.join(format!("{name}.deb"));
    let cmd = Cmd::new("dpkg-deb").args(["--root-owner-group", "--build"]).arg(&dir).arg(&deb);
    if opts.dry_run {
        println!("# lay out {}", dir.display());
        if build {
            opts.runner().run(&cmd)?;
        }
        return Ok(if build { deb } else { dir });
    }
    let bins = target.join(opts.profile());
    write_deb_layout(&dir, root, build.then_some(bins.as_path()))?;
    if !build {
        println!("{}", dir.display());
        return Ok(dir);
    }
    opts.runner().run(&cmd)?;
    println!("{}", deb.display());
    Ok(deb)
}

// ---------------------------------------------------------------- Windows

/// Lay out the Windows package in `dir` (replaced): `codecraft.exe`, `codecraft-cli.exe`,
/// `codecraft.png`, README and licenses.
pub fn write_windows_layout(dir: &Path, root: &Path, bin_dir: Option<&Path>) -> Result<(), String> {
    remove_dir(dir)?;
    create_dir(dir)?;
    create_dir(&dir.join("factory"))?;
    if let Some(src) = bin_dir {
        copy(&src.join("codecraft.exe"), &dir.join("factory/codecraft.exe"))?;
        copy(&src.join("codecraft-launcher.exe"), &dir.join("codecraft.exe"))?;
        copy(&src.join("codecraft-cli.exe"), &dir.join("codecraft-cli.exe"))?;
    }
    icon::write_png(&dir.join("codecraft.png"), 256)?;
    icon::write_ico(&dir.join("codecraft.ico"))?;
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
        cargo_build(root, opts, &["codecraft", "codecraft-cli", "codecraft-launcher"])?;
    } else {
        eprintln!("layout only (binaries are built on Windows)");
    }
    let zip = out.join(format!("{name}.zip"));
    let tar = Cmd::new("tar").arg("-a").arg("-c").arg("-f").arg(&zip).arg("-C").arg(&out).arg(&name);
    if opts.dry_run {
        println!("# lay out {}", dir.display());
        if build {
            opts.runner().run(&tar)?;
        }
        return Ok(if build { zip } else { dir });
    }
    let bins = target.join(opts.profile());
    write_windows_layout(&dir, root, build.then_some(bins.as_path()))?;
    if !build {
        println!("{}", dir.display());
        return Ok(dir);
    }
    opts.runner().run(&tar)?;
    println!("{}", zip.display());
    Ok(zip)
}

/// The Inno Setup script for the installer: a per-user install by default (no administrator
/// needed; the wizard offers all users), Start menu entry, optional desktop shortcut,
/// uninstaller, upgrades in place (fixed AppId).
pub fn inno_script(version: &str, arch: &str, source: &Path, out_dir: &Path, base_name: &str) -> String {
    let src = source.display();
    let x64 = if arch == "x64" {
        "ArchitecturesAllowed=x64compatible\nArchitecturesInstallIn64BitMode=x64compatible\n"
    } else {
        ""
    };
    // `{{` is a literal `{` in Inno Setup.
    let app_id = WINDOWS_APP_ID.replacen('{', "{{", 1);
    format!(
        "; Generated by `cargo xtask package-windows-installer`.\n\
         [Setup]\n\
         AppId={app_id}\n\
         AppName={DISPLAY_NAME}\n\
         AppVersion={version}\n\
         AppVerName={DISPLAY_NAME} {version}\n\
         AppPublisher=The CodeCraft contributors\n\
         AppPublisherURL={HOMEPAGE}\n\
         AppSupportURL={HOMEPAGE}/issues\n\
         AppUpdatesURL={HOMEPAGE}/releases\n\
         AppCopyright={COPYRIGHT}\n\
         DefaultDirName={{autopf}}\\{DISPLAY_NAME}\n\
         DefaultGroupName={DISPLAY_NAME}\n\
         DisableProgramGroupPage=yes\n\
         PrivilegesRequired=lowest\n\
         PrivilegesRequiredOverridesAllowed=dialog\n\
         {x64}\
         LicenseFile={src}\\LICENSE\n\
         SetupIconFile={src}\\codecraft.ico\n\
         UninstallDisplayIcon={{app}}\\codecraft.exe\n\
         UninstallDisplayName={DISPLAY_NAME}\n\
         OutputDir={}\n\
         OutputBaseFilename={base_name}\n\
         Compression=lzma2/max\n\
         SolidCompression=yes\n\
         WizardStyle=modern\n\
         CloseApplications=yes\n\
         \n\
         [Tasks]\n\
         Name: \"desktopicon\"; Description: \"{{cm:CreateDesktopIcon}}\"; GroupDescription: \"{{cm:AdditionalIcons}}\"; Flags: unchecked\n\
         \n\
         [Files]\n\
         Source: \"{src}\\*\"; DestDir: \"{{app}}\"; Flags: ignoreversion recursesubdirs createallsubdirs\n\
         \n\
         [Icons]\n\
         Name: \"{{autoprograms}}\\{DISPLAY_NAME}\"; Filename: \"{{app}}\\codecraft.exe\"; IconFilename: \"{{app}}\\codecraft.ico\"\n\
         Name: \"{{autodesktop}}\\{DISPLAY_NAME}\"; Filename: \"{{app}}\\codecraft.exe\"; IconFilename: \"{{app}}\\codecraft.ico\"; Tasks: desktopicon\n\
         \n\
         [Run]\n\
         Filename: \"{{app}}\\codecraft.exe\"; Description: \"{{cm:LaunchProgram,{DISPLAY_NAME}}}\"; Flags: nowait postinstall skipifsilent\n",
        out_dir.display()
    )
}

/// The Inno Setup compiler: `$ISCC`, else its default install folders, else `iscc` on PATH.
fn iscc() -> String {
    if let Some(p) = std::env::var_os("ISCC").filter(|p| !p.is_empty()) {
        return p.to_string_lossy().into_owned();
    }
    for var in ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(var) {
            for sub in ["Inno Setup 6", "Programs\\Inno Setup 6"] {
                let p = PathBuf::from(&base).join(sub).join("ISCC.exe");
                if p.is_file() {
                    return p.display().to_string();
                }
            }
        }
    }
    "iscc".into()
}

/// `cargo xtask package-windows-installer`: `package-windows` (build, layout, zip), then an Inno
/// Setup installer `codecraft-<version>-windows-<arch>-setup.exe` from the same layout. Elsewhere
/// it writes the layout and the script only.
pub fn package_windows_installer(root: &Path, opts: &Options) -> Result<PathBuf, String> {
    package_windows(root, opts)?;
    let out = target_dir(root).join("package");
    let name = format!("codecraft-{}-windows-{}", version(), arch());
    let base = format!("{name}-setup");
    let script_path = out.join(format!("{name}.iss"));
    let script = inno_script(version(), arch(), &out.join(&name), &out, &base);
    let cmd = Cmd::new(&iscc()).arg("/Qp").arg(&script_path);
    if opts.dry_run {
        println!("# write {}", script_path.display());
        return opts.runner().run(&cmd).map(|()| out.join(format!("{base}.exe")));
    }
    write(&script_path, script)?;
    if !cfg!(windows) || opts.layout_only {
        println!("{}", script_path.display());
        return Ok(script_path);
    }
    opts.runner().run(&cmd)?;
    let exe = out.join(format!("{base}.exe"));
    println!("{}", exe.display());
    Ok(exe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_profiles_use_cargo_output_directories() {
        let dev=Options::parse(&["--profile".into(),"dev".into(),"--version-payload".into(),"payload".into()]).unwrap();
        assert_eq!(dev.profile(),"debug");
        assert!(cargo_build_cmd(&dev,&["codecraft"]).args.windows(2).any(|w|w==["--profile","dev"]));
        let local=Options::parse(&["--profile".into(),"selfupdate".into()]).unwrap();assert_eq!(local.profile(),"selfupdate");
        assert!(Options::parse(&["--profile".into(),"../escape".into()]).is_err());
    }
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
        assert!(o.release && !o.layout_only && !o.dry_run);
        assert!(o.sign.is_none() && o.keychain.is_none());
        assert!(Options::parse(&["--bogus".into()]).is_err());
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let o = Options::parse(&args(&[
            "--sign",
            "Developer ID Application: Jane (ABC)",
            "--keychain=/tmp/k.keychain-db",
            "--dry-run",
        ]))
        .unwrap();
        assert_eq!(o.sign.as_deref(), Some("Developer ID Application: Jane (ABC)"));
        assert_eq!(o.keychain, Some(PathBuf::from("/tmp/k.keychain-db")));
        assert!(o.dry_run);
        assert_eq!(o.signer().identity, "Developer ID Application: Jane (ABC)");
        assert_eq!(Options::parse(&args(&["--sign", "-"])).unwrap().signer().identity, "-");
        assert_eq!(Options::parse(&args(&["--sign=-"])).unwrap().sign.as_deref(), Some("-"));
        assert!(Options::parse(&args(&["--sign"])).is_err());
        assert!(Options::parse(&args(&["--keychain"])).is_err());
    }

    #[test]
    fn build_and_dmg_commands() {
        let o = Options { release: true, ..Default::default() };
        let c = cargo_build_cmd(&o, &["codecraft", "codecraft-cli"]);
        assert_eq!(c.args, ["build", "-p", "codecraft", "-p", "codecraft-cli", "--release"]);
        let h = hdiutil_create(Path::new("/b/dmg-staging"), Path::new("/b/CodeCraft-1.0.dmg"));
        assert_eq!(
            h.to_string(),
            "hdiutil create -volname 'Code Craft' -fs HFS+ -format UDZO -ov -srcfolder /b/dmg-staging /b/CodeCraft-1.0.dmg"
        );
        let root = Path::new("/w");
        assert!(dmg_path(root).ends_with(format!("bundle/CodeCraft-{}.dmg", version())));
    }

    #[test]
    fn dry_runs_write_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let opts =
            Options { dry_run: true, sign: Some("Developer ID Application: X (Y)".into()), ..Default::default() };
        let app = bundle(dir.path(), &opts).unwrap();
        assert!(app.ends_with("bundle/CodeCraft.app"));
        let image = dmg(dir.path(), &opts).unwrap();
        assert!(image.ends_with(format!("bundle/CodeCraft-{}.dmg", version())));
        package_linux(dir.path(), &opts).unwrap();
        package_windows(dir.path(), &opts).unwrap();
        // Nothing under the (default) target dir of this fake root.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        let n = sign::NotarizeOptions { profile: Some("p".into()), dry_run: true, ..Default::default() };
        notarize(dir.path(), &n).unwrap();
        // Without a profile notarize says how to make one.
        let e = notarize(dir.path(), &sign::NotarizeOptions { dry_run: true, ..Default::default() });
        if std::env::var_os(sign::PROFILE_ENV).is_none() {
            assert!(e.unwrap_err().contains("store-credentials"));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn stable_factory_signatures_keep_entitlements_and_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let layout = write_launcher_bundle(dir.path(), Some(Path::new("/usr/bin/true")), Some(Path::new("/usr/bin/true")), None).unwrap();
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let opts = Options { sign: Some("-".into()), ..Default::default() };
        let factory = AppLayout::new(&layout.app.join("Contents/Resources/factory"));
        sign_bundle(root, &opts, &factory).unwrap();
        sign_bundle(root, &opts, &layout).unwrap();
        let output = Command::new("codesign").args(["--display", "--entitlements", "-", "--xml"]).arg(&factory.app).output().unwrap();
        assert!(output.status.success());
        let entitlements = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        assert!(entitlements.contains("com.apple.security.automation.apple-events"));
    }

    #[test]
    fn stable_launcher_and_payload_layouts() {
        let dir = tempfile::tempdir().unwrap();
        let icon = dir.path().join("icon.icns"); std::fs::write(&icon, "icns").unwrap();
        let bin = dir.path().join("app"); std::fs::write(&bin, "app").unwrap();
        let launcher = dir.path().join("launcher"); std::fs::write(&launcher, "launcher").unwrap();
        let layout = write_launcher_bundle(&dir.path().join("stable"), Some(&bin), Some(&launcher), Some(&icon)).unwrap();
        assert_eq!(std::fs::read_to_string(&layout.executable).unwrap(), "launcher");
        assert_eq!(std::fs::read_to_string(layout.app.join("Contents/Resources/factory/CodeCraft.app/Contents/MacOS/codecraft")).unwrap(), "app");
        let plist = std::fs::read_to_string(layout.info_plist).unwrap();
        assert!(plist.contains("<string>codecraft-launcher</string>")); assert!(plist.contains("<key>LSUIElement</key><true/>"));
        let payload = write_app_bundle(&dir.path().join("payload"), Some(&bin), Some(&icon)).unwrap();
        assert!(!payload.app.join("Contents/Resources/factory").exists());
        assert!(Options::parse(&["--version-payload".into(), "payload".into()]).unwrap().version_payload.is_some());
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
        for b in ["codecraft", "codecraft-cli", "codecraft-launcher"] {
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
        for b in ["codecraft.exe", "codecraft-cli.exe", "codecraft-launcher.exe"] {
            std::fs::write(bins.join(b), "exe").unwrap();
        }
        let out = dir.path().join("win");
        write_windows_layout(&out, dir.path(), Some(&bins)).unwrap();
        assert!(out.join("codecraft.exe").is_file());
        assert!(out.join("codecraft-cli.exe").is_file());
        assert!(out.join("codecraft.png").is_file());
        let ico = std::fs::read(out.join("codecraft.ico")).unwrap();
        assert_eq!(&ico[..4], &[0, 0, 1, 0], "an icon file");
        assert_eq!(u16::from_le_bytes([ico[4], ico[5]]), 6, "six sizes");
        // The 256 px entry says 0 (= 256) and points at a PNG inside the file.
        let last = 6 + 16 * 5;
        assert_eq!(ico[last], 0);
        let off = u32::from_le_bytes(ico[last + 12..last + 16].try_into().unwrap()) as usize;
        assert_eq!(&ico[off..off + 4], b"\x89PNG");
    }

    #[test]
    fn names_match_the_engine() {
        let about = include_str!("../../crates/engine/src/about.rs");
        assert!(about.contains(&format!("pub const DISPLAY_NAME: &str = \"{DISPLAY_NAME}\";")));
        assert!(about.contains(&format!("pub const REPOSITORY: &str = \"{HOMEPAGE}\";")));
        let p = info_plist("1.0.0");
        assert!(p.contains("<key>CFBundleName</key>\n\t<string>Code Craft</string>"));
        assert!(p.contains("<key>NSHumanReadableCopyright</key>\n\t<string>Copyright (c) 2026 The CodeCraft contributors. MIT License.</string>"));
        assert!(desktop_entry().contains("\nName=Code Craft\n"));
    }

    #[test]
    fn deb_package_layout() {
        assert_eq!(deb_version("0.3.0-beta.1"), "0.3.0~beta.1");
        let control = deb_control("0.2.0", "amd64", 12345);
        for needle in [
            "Package: codecraft\n",
            "Version: 0.2.0\n",
            "Architecture: amd64\n",
            "Installed-Size: 12345\n",
            "Depends: libc6 (>= 2.35)",
            "Description: Code Craft, a native IDE written in Rust\n ",
        ] {
            assert!(control.contains(needle), "missing {needle:?} in\n{control}");
        }
        // Every continuation line of the description starts with a space; the file ends with a newline.
        assert!(control.ends_with('\n'));
        assert!(control.lines().skip_while(|l| !l.starts_with("Description:")).skip(1).all(|l| l.starts_with(' ')));

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("LICENSE"), "MIT License\n\nCopyright (c) 2026 X\n\nPermission is hereby granted.\n\nTHE SOFTWARE IS PROVIDED \"AS IS\".\n").unwrap();
        let bins = dir.path().join("bins");
        std::fs::create_dir_all(&bins).unwrap();
        for b in ["codecraft", "codecraft-cli", "codecraft-launcher"] {
            std::fs::write(bins.join(b), vec![0u8; 4000]).unwrap();
        }
        let out = dir.path().join("deb");
        write_deb_layout(&out, &root, Some(&bins)).unwrap();
        assert!(out.join("usr/bin/codecraft").is_file());
        assert!(out.join("usr/bin/codecraft-cli").is_file());
        assert!(out.join("usr/share/applications/dev.codecraft.CodeCraft.desktop").is_file());
        assert!(out.join("usr/share/icons/hicolor/512x512/apps/dev.codecraft.CodeCraft.png").is_file());
        let copyright = std::fs::read_to_string(out.join("usr/share/doc/codecraft/copyright")).unwrap();
        assert!(copyright.contains("License: MIT\n Permission is hereby granted.\n .\n THE SOFTWARE"), "{copyright}");
        let control = std::fs::read_to_string(out.join("DEBIAN/control")).unwrap();
        let kib: u64 = control.lines().find_map(|l| l.strip_prefix("Installed-Size: ")).unwrap().parse().unwrap();
        assert!(kib >= 8, "the binaries count: {kib}");
        assert!(!out.join("README.txt").exists(), "nothing outside usr/ and DEBIAN/");
    }

    #[test]
    fn windows_installer_script() {
        let s = inno_script(
            "0.2.0",
            "x64",
            Path::new("C:\\b\\pkg\\codecraft-0.2.0-windows-x64"),
            Path::new("C:\\b\\pkg"),
            "codecraft-0.2.0-windows-x64-setup",
        );
        for needle in [
            "AppId={{8E0C5B7A-3F2D-4C1E-9A6B-2D7F4E8C1A35}\n",
            "AppName=Code Craft\n",
            "AppVersion=0.2.0\n",
            "DefaultDirName={autopf}\\Code Craft\n",
            "PrivilegesRequired=lowest\n",
            "ArchitecturesInstallIn64BitMode=x64compatible\n",
            "LicenseFile=C:\\b\\pkg\\codecraft-0.2.0-windows-x64\\LICENSE\n",
            "OutputBaseFilename=codecraft-0.2.0-windows-x64-setup\n",
            "Source: \"C:\\b\\pkg\\codecraft-0.2.0-windows-x64\\*\"; DestDir: \"{app}\"",
            "Name: \"{autoprograms}\\Code Craft\"; Filename: \"{app}\\codecraft.exe\"",
            "Description: \"{cm:LaunchProgram,Code Craft}\"",
        ] {
            assert!(s.contains(needle), "missing {needle:?} in\n{s}");
        }
        assert!(!inno_script("0.2.0", "arm64", Path::new("a"), Path::new("b"), "c").contains("x64compatible"));
    }
}
