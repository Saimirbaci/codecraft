# Signed builds, notarization and releases

How to build a Code Craft that other people can open: a Developer ID-signed, notarized and stapled
`.dmg` for macOS, a `.deb` and a tarball for Linux, and an installer and a zip for Windows, locally
or from CI, and how to cut a new version.

Credentials never live in the repository. Locally, the signing identity is in your login keychain
and notarization uses a `notarytool` keychain profile you create once. In CI they come from GitHub
Actions secrets, are imported into a temporary keychain for the job and deleted afterwards.
`cargo xtask` only ever passes the identity name and the profile name (neither is a secret) and
never runs `notarytool store-credentials` itself.

## Commands

```sh
cargo xtask bundle [--release] [--sign <identity>] [--keychain <path>] [--dry-run]
cargo xtask dmg    [--release] [--sign <identity>] [--keychain <path>] [--dry-run]
cargo xtask notarize --profile <keychain-profile> [--dmg <path> | --app <path>] [--keychain <path>] [--dry-run]
```

- `--sign` takes a codesign identity: `-` (ad hoc, the default), the full name
  (`"Developer ID Application: Jane Doe (ABCDE12345)"`) or its SHA-1 hash. Without `--sign`,
  `CODECRAFT_SIGN_IDENTITY` is used when set.
- `--profile` names the notarytool keychain profile; without it `CODECRAFT_NOTARY_PROFILE` is used.
  `notarize` defaults to the dmg `cargo xtask dmg` wrote (`<target>/bundle/CodeCraft-<version>.dmg`).
- `--keychain` points codesign and notarytool at a specific keychain (CI's temporary one).
- `--dry-run` prints the exact commands, in order, and runs and writes nothing. Use it to review
  what will be signed and submitted.

### What `bundle` does

1. `cargo build -p codecraft -p codecraft-launcher [--release]`, then lays out `CodeCraft.app` (see
   docs/development.md › Packaging).
2. Signs nested code inside-out: every nested bundle (`.framework`, `.app`, `.xpc`, `.appex`,
   `.bundle`, `.plugin`) and Mach-O file other than the main executable, deepest first. The full factory app is
   signed before the outer stable launcher bundle.
   `--deep` is never used for signing (Apple recommends signing each piece explicitly).
3. Signs the bundle with the hardened runtime and the entitlements:
   `codesign --force --sign <identity> --timestamp --options runtime --entitlements apps/codecraft/macos/CodeCraft.entitlements CodeCraft.app`
   (ad hoc: `--timestamp=none`, since an ad-hoc signature cannot carry a secure timestamp; local
   builds still get the hardened runtime, so they behave like release builds).
4. Verifies: `codesign --verify --deep --strict --verbose=2 CodeCraft.app`, then
   `codesign --display --verbose=2` must show `runtime` in the CodeDirectory flags.

### What `dmg` does

`bundle`, then copies the app and an `/Applications` link into a staging folder, makes a
compressed image (`hdiutil create … -format UDZO`), signs the image with the same identity
(`codesign --force --sign <identity> --timestamp CodeCraft-<version>.dmg`, no runtime or
entitlements: those are for executables) and verifies it
(`codesign --verify --strict --verbose=2`).

### What `notarize` does

1. `xcrun notarytool submit CodeCraft-<version>.dmg --keychain-profile <profile> --wait --output-format json`
   (with `--app`, the app is zipped with `ditto -c -k --keepParent` first and the zip is uploaded).
2. If the status is not `Accepted`, prints `xcrun notarytool log <submission-id> --keychain-profile <profile>`
   (the notary service's list of problems) and fails.
3. `xcrun stapler staple` and `xcrun stapler validate` on the dmg (or the app), so Gatekeeper can
   check the ticket offline.
4. `spctl -a -vv -t install CodeCraft-<version>.dmg` (with `--app`: `spctl -a -vv -t exec CodeCraft.app`):
   expect `accepted` and `source=Notarized Developer ID`.

Notarizing the dmg registers tickets for the app inside it too, so an app copied out of the dmg
passes Gatekeeper online; the staple itself is on the dmg. Run `notarize --app` before `dmg` if
you also want the ticket stapled to the app (two submissions).

## Entitlements

`apps/codecraft/macos/CodeCraft.entitlements` is deliberately minimal. Every key is listed with
its reason in `xtask/src/sign.rs` (`ENTITLEMENT_KEYS`) and here; a test checks that the file,
the list and this page agree.

| Entitlement | Why |
|---|---|
| `com.apple.security.automation.apple-events` | macOS attributes programs started from the Terminal tool window and from run configurations to CodeCraft (the "responsible" app). Under the hardened runtime their Apple Events (`osascript`, a script driving another app) are denied without a prompt unless CodeCraft holds this entitlement. `Info.plist` carries the matching `NSAppleEventsUsageDescription`. |

Not requested, and why:

- `com.apple.security.cs.allow-jit`, `allow-unsigned-executable-memory`: no JIT; egui/wgpu render
  through Metal, whose shader compiler runs in a system service.
- `com.apple.security.cs.allow-dyld-environment-variables`, `disable-library-validation`:
  CodeCraft is a static Rust binary that links only system frameworks and loads no plug-ins.
  Programs it starts are separate processes and keep their own environment (`DYLD_*` for
  `cargo run` is unaffected).
- `com.apple.security.cs.debugger`: the debugger is `lldb-dap`, whose `debugserver` is signed by
  Apple with its own entitlements; CodeCraft never calls `task_for_pid` itself. Profilers
  (samply, py-spy) are likewise separate signed tools.
- `com.apple.security.get-task-allow`: release builds must not be attachable by other processes.
- `com.apple.security.device.camera`, `device.audio-input`: programs run from CodeCraft that need
  the camera or microphone are attributed to CodeCraft and are denied today. Add these (with
  `NSCameraUsageDescription` / `NSMicrophoneUsageDescription` in `info_plist()`) if that becomes a
  real need.
- No App Sandbox: an IDE has to read and write arbitrary project folders and run toolchains.

## One-time setup (local)

1. **Developer ID certificate.** In an Apple Developer Program team, create a *Developer ID
   Application* certificate (Xcode › Settings › Accounts › Manage Certificates, or
   developer.apple.com › Certificates). It lands in your login keychain. Check:

   ```sh
   security find-identity -v -p codesigning
   # 1) 0123…ABCD "Developer ID Application: Jane Doe (ABCDE12345)"
   ```

2. **Notarytool profile.** Store the notary credentials in your keychain once. With an Apple ID and
   an app-specific password (appleid.apple.com › Sign-In and Security › App-Specific Passwords):

   ```sh
   xcrun notarytool store-credentials codecraft --apple-id you@example.com --team-id ABCDE12345
   # prompts for the app-specific password; nothing is written outside the keychain
   ```

   Or with an App Store Connect API key:
   `xcrun notarytool store-credentials codecraft --key AuthKey_XXXX.p8 --key-id XXXX --issuer <uuid>`.
   Check it with `xcrun notarytool history --keychain-profile codecraft`.

## Building a signed, notarized dmg locally

```sh
export CODECRAFT_SIGN_IDENTITY="Developer ID Application: Jane Doe (ABCDE12345)"
cargo xtask dmg --release --dry-run          # review the commands
cargo xtask dmg --release                    # build, sign app inside-out, verify, dmg, sign dmg
cargo xtask notarize --profile codecraft     # submit, wait, staple, validate, spctl
```

Verify by hand at any point:

```sh
codesign --verify --deep --strict --verbose=2 target/bundle/CodeCraft.app
codesign --display --verbose=2 target/bundle/CodeCraft.app          # flags=…(runtime)
codesign --display --entitlements - --xml target/bundle/CodeCraft.app
xcrun stapler validate target/bundle/CodeCraft-0.1.0.dmg
spctl -a -vv -t install target/bundle/CodeCraft-0.1.0.dmg
```

Ad-hoc builds (`cargo xtask bundle`) need no certificate and run on the Mac that built them. On
another Mac Gatekeeper refuses them; the recipient can right-click › Open, or remove the
quarantine attribute (`xattr -dr com.apple.quarantine CodeCraft.app`).

### When notarization fails

`notarize` prints the notary log. Common causes: a nested binary signed without the hardened
runtime or without a timestamp (re-run `dmg`; it signs every nested Mach-O), an ad-hoc signature
(`--sign` missing), a certificate that is not *Developer ID Application*, or an entitlement that
Apple rejects. Submissions and logs are also listed by
`xcrun notarytool history --keychain-profile codecraft`.

## CI

### `.github/workflows/ci.yml` (push to `main`, pull requests)

- **macOS and Linux** (`macos-latest`, `ubuntu-latest`): `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`, `cargo xtask layers`, and `cargo xtask parity` followed by
  `git diff --exit-code docs/parity-checklist.md` (the committed checklist must be current).
  Tests that need tools the runners lack (rust-analyzer, lldb-dap, debugpy, Docker, databases,
  …) skip themselves. Linux installs `pkg-config`, `libssl-dev` (OpenSSL for the SQL Server and
  ClickHouse drivers' TLS), Wayland/xkbcommon headers, and Mesa's Vulkan driver so the egui
  snapshot tests have a software GPU.
- **Windows** (`windows-latest`): `cargo check --workspace` (a build check; tests are not run on
  Windows yet).
- **rustfmt is not checked**: the tree is not rustfmt-clean today (`cargo fmt --all -- --check`
  reports differences in about a thousand places across crates other tracks are editing).
  Formatting it is a separate, all-at-once change; once it is made, add
  `cargo fmt --all -- --check` as the first step of the `test` job.

### `.github/workflows/release.yml` (by hand)

Releases are started by hand only: **pushing a tag starts nothing**. Run it from Actions ›
Release › Run workflow, or `gh workflow run release.yml -f tag=v0.2.0` (with a tag: build and
release it; without: only build the packages, which the run keeps as downloadable Artifacts,
for testing a build before releasing it), or tick "start the release" in the New version
workflow.

1. **version**: the tag must equal the workspace version (`v0.1.0` ↔ `version = "0.1.0"` in
   `Cargo.toml`). Every build job checks out that tag and compiles in the commit and the date
   (`CODECRAFT_BUILD_COMMIT`, `CODECRAFT_BUILD_DATE`), which Help › About and `--version` show.
2. **macOS**: if the signing secrets exist, the p12 is imported into a temporary keychain made
   for the job (random password, auto-lock), the identity is read from it, and
   `cargo xtask dmg --release --sign <identity> --keychain <temp>` runs. If the notarization
   secrets exist too, `xcrun notarytool store-credentials` stores them as profile `codecraft-ci`
   in that same temporary keychain and `cargo xtask notarize --profile codecraft-ci --keychain <temp>`
   submits, staples and assesses the dmg. Without secrets the dmg is ad-hoc signed (usable for
   testing, not for distribution). The temporary keychain and the p12 file are deleted in an
   `always()` step. Apple silicon only (`macos-latest` is arm64).
3. **Linux** (Ubuntu 22.04, so the binary runs on glibc 2.35 and newer):
   `cargo xtask package-linux --release` → `codecraft-<version>-linux-x64.tar.gz`, and
   `cargo xtask package-deb --release` → `codecraft_<version>_amd64.deb` (`dpkg-deb`; installs
   `/usr/bin/codecraft`, `/usr/bin/codecraft-cli`, the desktop entry and icons; a pre-release
   `0.3.0-beta.1` becomes Debian version `0.3.0~beta.1`).
4. **Windows**: `cargo xtask package-windows-installer --release` → the zip
   `codecraft-<version>-windows-x64.zip` and the Inno Setup installer
   `codecraft-<version>-windows-x64-setup.exe` (per-user install without administrator rights,
   Start menu entry, optional desktop shortcut, uninstaller; upgrades in place thanks to a fixed
   AppId). Inno Setup is installed with Chocolatey when the runner lacks it. Neither is
   Authenticode-signed yet, so SmartScreen asks for confirmation.
5. **release** (only with a tag): downloads the artifacts, writes `SHA256SUMS.txt`, and creates
   the GitHub release with install notes per platform plus the generated change list. It is a
   **draft** unless the run asked to publish; a version with a `-` (`0.3.0-beta.1`) is marked
   as a pre-release.

Secrets the release workflow reads (repository › Settings › Secrets and variables › Actions). All
are optional; missing ones only disable signing or notarization:

| Secret | Contents |
|---|---|
| `MACOS_CERTIFICATE_P12` | The *Developer ID Application* certificate and private key exported as `.p12`, base64-encoded (`base64 -i cert.p12 \| pbcopy`) |
| `MACOS_CERTIFICATE_PASSWORD` | The password the `.p12` was exported with |
| `APPLE_ID` | The Apple ID used for notarization |
| `APPLE_TEAM_ID` | The 10-character team ID |
| `APPLE_APP_SPECIFIC_PASSWORD` | An app-specific password for that Apple ID |

Actions are pinned to exact release tags (`actions/checkout@v4.2.2`, …). Pinning to commit SHAs is
stricter; convert them with a tool such as Dependabot (`package-ecosystem: github-actions`) or
`pinact`. No third-party actions are used: Rust comes from the runner's `rustup`, the release from
the runner's `gh`.

### `.github/workflows/new-version.yml` (by hand)

The CI twin of `./release.sh` (below). Actions › New version › Run workflow, on `main`:

- **bump**: `patch` (0.1.0 → 0.1.1), `minor` (→ 0.2.0), `major` (→ 1.0.0), or an exact version
  (`0.3.0`, `0.3.0-beta.1`; `patch` on a pre-release releases it: 0.3.0-beta.1 → 0.3.0).
- **start the release**: off (default) stops after pushing the tag; on also runs the Release
  workflow for it.
- **publish** (with the release): off (default) creates a draft release to check and publish by
  hand; on publishes it right away.

It runs `cargo xtask version <bump>`, checks that `Cargo.lock` is consistent
(`cargo tree --locked`), commits `Release vX.Y.Z` to main as `github-actions[bot]`, tags it,
and pushes both in one atomic push. If main is protected against direct pushes, add GitHub
Actions to the rule's bypass list, or use `./release.sh`.

## Versions

The version lives once, under `[workspace.package]` in `Cargo.toml`; every crate inherits it.
It follows semantic versioning: patch for fixes, minor for features, major for breaking changes
to saved settings, the command ids agents use, or the CLI.

```sh
cargo xtask version                 # print it: 0.1.0
cargo xtask version minor --dry-run # what a bump would do
cargo xtask version minor           # Cargo.toml + the workspace's entries in Cargo.lock → 0.2.0
```

### Cutting a release

1. **Tag the version** with `./release.sh` from a clean, up-to-date `main`:

   ```sh
   ./release.sh patch            # 0.1.0 → 0.1.1  (minor → 0.2.0, major → 1.0.0)
   ./release.sh 0.3.0-beta.1     # an exact version
   ./release.sh minor --dry-run  # show what would happen
   ./release.sh minor --no-push  # commit and tag locally only
   ```

   It checks that you are on `main` with a clean tree, not behind `origin/main`, and that the tag
   is new; shows the plan and asks for confirmation (`--yes` skips it); runs
   `cargo xtask version <level>` and `cargo tree --locked --offline` (the bump is undone if
   that fails); commits `Release vX.Y.Z`; creates the annotated tag; and pushes `main` and the tag
   in one atomic push. Nothing is built: pushing the tag starts no workflow. (Or use the New
   version workflow, above.)
2. **Release it** when ready: Actions › Release › Run workflow with the tag, or
   `gh workflow run release.yml -f tag=vX.Y.Z` (`-f draft=false` to publish at once).
3. Open the draft release, check the artifacts (the macOS job log shows `notarization accepted`
   and `source=Notarized Developer ID` when the secrets are set), edit the notes, publish.

## Cross-platform checks without the toolchains

The Windows target is checked only in CI. On a Mac, the Linux code paths can be type-checked with
an installed Linux target even without a cross C toolchain: two crates build C code
(`libsqlite3-sys` compiles SQLite; `openssl-sys` probes OpenSSL's headers), and `cargo check`
never links, so a stand-in compiler that forwards preprocessing to the host `clang` and turns
compiles into empty objects is enough:

```sh
mkdir -p /tmp/cc-check
cat > /tmp/cc-check/clang <<'EOF'
#!/bin/sh
for a in "$@"; do [ "$a" = "-c" ] && c=1; done
[ -z "$c" ] && exec /usr/bin/clang "$@"
while [ $# -gt 0 ]; do [ "$1" = "-o" ] && out="$2"; shift; done
exec /usr/bin/clang --target=aarch64-unknown-linux-musl -c -x c /dev/null -o "$out"
EOF
chmod +x /tmp/cc-check/clang
CC_aarch64_unknown_linux_musl=/tmp/cc-check/clang AR_aarch64_unknown_linux_musl=/usr/bin/ar \
AARCH64_UNKNOWN_LINUX_MUSL_OPENSSL_DIR=$(brew --prefix openssl@3) \
  cargo check --workspace --all-targets --target aarch64-unknown-linux-musl
```

The script must be named `clang`: the `cc` crate guesses the compiler family from the name and
otherwise may take the stand-in for MSVC. The result says nothing about linking or running;
CI's Linux job does that. (2026-10-16: the whole workspace, all targets, checks clean this way.)

## Self-update payloads (M18 pass 1)

Release bundles use `Contents/MacOS/codecraft-launcher` as `CFBundleExecutable` with
`LSUIElement=true`; the foreground factory app lives in
`Contents/Resources/factory/CodeCraft.app`. The launcher starts its executable directly and
supervises health without adding a foreground Dock icon. Self-update never rebuilds the
installed launcher. `cargo xtask bundle --version-payload <dir> [--release]` creates only an
ad-hoc signed foreground payload for staging (use no `--sign` for local self-updates).
`--layout-only` works for both modes. Developer `cargo run -p codecraft` stays direct.

Linux packages install the launcher as `bin/codecraft` and the factory as
`bin/factory/codecraft`; Windows packages use `codecraft.exe` and `factory/codecraft.exe`.
The stable entry point accepts `--factory`, `--version-id <id>`, `--status`, and `--rollback`.
`--factory` ignores version pointers and metadata; on macOS run
`/Applications/CodeCraft.app/Contents/MacOS/codecraft-launcher --factory`.
See [m18-plan.md](m18-plan.md) for health deadlines, rollback, tests and manual GUI checks.

Self-update payload builds use `cargo xtask bundle --profile selfupdate --version-payload <dir>`.
The stable launcher additionally accepts `--wait-pid <pid> --version-id <id> --wait-lock` for
handoff: wait for the prior app, acquire supervision, then boot the requested version. The wait
is bounded to five minutes. Release packaging still uses the release profile; local feedback
builds favor turnaround over peak optimization. Managed source lives under `CODECRAFT_HOME/source`
on `codecraft-user`; no credentials are stored and no changes are pushed.

`tools/selfupdate-smoke.sh` is an offline real-binary smoke using a local bare repository and
temporary home/store. `codecraft --selfupdate-health-check` tests the launcher's health protocol
without opening a GUI or reading project/agent state. This diagnostic does not replace native
Dock/menu identity, Gatekeeper/TCC, GUI health or session-restoration release checks.
