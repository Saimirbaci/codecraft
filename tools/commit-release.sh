#!/usr/bin/env bash
# Version the current commit and build its packages, locally.
#
#   tools/commit-release.sh [patch|minor|major|skip] [--no-build] [--publish] [--yes]
#   tools/commit-release.sh --install-hook | --uninstall-hook
#
# Asks "patch, minor or major?" (Enter or "skip" does nothing), then:
#   1. bumps the workspace version (`cargo xtask version`: Cargo.toml and the workspace entries
#      of Cargo.lock), commits "Release vX.Y.Z" and tags it vX.Y.Z (nothing is pushed);
#   2. builds the packages for this OS into dist/vX.Y.Z/ with a SHA256SUMS file:
#        macOS    CodeCraft-X.Y.Z.dmg
#        Linux    codecraft-X.Y.Z-linux-<arch>.tar.gz and codecraft_X.Y.Z_<arch>.deb
#        Windows  codecraft-X.Y.Z-windows-<arch>-setup.exe and .zip   (run from Git Bash)
#      plus the self-update files for this OS: codecraft-payload-X.Y.Z-<platform>.tar.gz,
#      update-<platform>.json and its .sig, signed with ~/.codecraft-release/update.key
#      (`cargo xtask update-keygen` creates it once; docs/release.md › Updates).
#   3. with --publish: pushes the commit and tag, and creates the GitHub release with everything
#      in dist/vX.Y.Z/ (gh must be logged in). Installed apps then offer the update.
#
# Signing follows `cargo xtask`: ad hoc unless CODECRAFT_SIGN_IDENTITY is set (docs/release.md).
# For releases that other people download, push the tag and run the Release workflow instead
# (it can use a self-hosted runner: docs/release.md › Runners).
#
# --install-hook adds a post-commit hook that runs this script after every commit you make by
# hand, so each commit can get a version. It asks on the terminal, skips without one, during a
# rebase, and for the "Release v…" commits this script makes. Press Enter to skip a commit.
# Set CODECRAFT_NO_RELEASE=1 to silence the hook for one command.

set -euo pipefail

die() { echo "commit-release.sh: $*" >&2; exit 1; }

usage() { sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }

level=""
build=true
publish=false
yes=false
hook=false
action=""
for arg in "$@"; do
    case "$arg" in
        patch|minor|major|skip) level=$arg ;;
        --no-build) build=false ;;
        --publish) publish=true ;;
        --yes|-y) yes=true ;;
        --hook) hook=true ;;
        --install-hook|--uninstall-hook) action=$arg ;;
        -h|--help) usage ;;
        *) echo "commit-release.sh: unknown argument '$arg'" >&2; usage ;;
    esac
done

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
hook_file=$(git rev-parse --git-path hooks/post-commit)

if [ "$action" = --install-hook ]; then
    if [ -e "$hook_file" ] && ! grep -q 'commit-release.sh' "$hook_file"; then
        die "$hook_file already exists and is not ours; add 'tools/commit-release.sh --hook' to it by hand"
    fi
    mkdir -p "$(dirname "$hook_file")"
    printf '#!/bin/sh\n# Installed by tools/commit-release.sh --install-hook\nexec "$(git rev-parse --show-toplevel)/tools/commit-release.sh" --hook\n' > "$hook_file"
    chmod +x "$hook_file"
    echo "Installed $hook_file. Remove it with: tools/commit-release.sh --uninstall-hook"
    exit 0
fi
if [ "$action" = --uninstall-hook ]; then
    if [ -e "$hook_file" ] && grep -q 'commit-release.sh' "$hook_file"; then rm -f "$hook_file"; echo "Removed $hook_file."; fi
    exit 0
fi

# ---- hook mode: stay out of the way unless a person is typing
if $hook; then
    [ -z "${CODECRAFT_NO_RELEASE:-}" ] || exit 0
    [ -z "${CODECRAFT_RELEASE_RUNNING:-}" ] || exit 0
    gitdir=$(git rev-parse --git-dir)
    if [ -d "$gitdir/rebase-merge" ] || [ -d "$gitdir/rebase-apply" ] || [ -e "$gitdir/MERGE_HEAD" ] \
        || [ -e "$gitdir/CHERRY_PICK_HEAD" ]; then exit 0; fi
    case "$(git log -1 --format=%s)" in "Release v"*) exit 0 ;; esac
    { : < /dev/tty; } 2>/dev/null || exit 0
    exec < /dev/tty
fi
export CODECRAFT_RELEASE_RUNNING=1

# ---- checks
[ -z "$(git status --porcelain --untracked-files=no)" ] || die "tracked files have uncommitted changes; commit or stash them first"
current=$(cargo xtask version | tail -n 1)
subject=$(git log -1 --format='%h %s')

if $build && [ ! -f "${CODECRAFT_UPDATE_KEY:-$HOME/.codecraft-release/update.key}" ]; then
    die "no update signing key; run 'cargo xtask update-keygen' once (or use --no-build)"
fi

# ---- which bump
if [ -z "$level" ]; then
    echo "Commit $subject (version $current)"
    read -r -p "New version: [p]atch, [m]inor, ma[j]or, or Enter to skip? " answer
    case "$answer" in
        p|patch) level=patch ;;
        m|minor) level=minor ;;
        j|major) level=major ;;
        *) level=skip ;;
    esac
fi
[ "$level" != skip ] || { echo "No new version."; exit 0; }

next=$(cargo xtask version "$level" --dry-run | tail -n 1)
tag="v$next"
git rev-parse -q --verify "refs/tags/$tag" > /dev/null && die "tag $tag already exists"
echo "Version: $current → $next (tag $tag, commit \"Release $tag\")"
if ! $yes && ! $hook; then
    read -r -p "Go ahead? [y/N] " answer
    case "$answer" in y|Y|yes|YES) ;; *) echo "Cancelled."; exit 1 ;; esac
fi

# ---- bump, commit, tag (undo the bump if anything fails before the commit)
committed=false
restore() { $committed || git checkout --quiet -- Cargo.toml Cargo.lock 2> /dev/null || true; }
trap restore EXIT
cargo xtask version "$level" > /dev/null
cargo tree --locked --offline --workspace --depth 0 > /dev/null || die "Cargo.lock is out of date after the bump"
git add Cargo.toml Cargo.lock
git commit --quiet --no-verify -m "Release $tag"
committed=true
git tag -a "$tag" -m "Code Craft $next"
echo "Committed and tagged $tag (not pushed)."

$build || { echo "Skipping the build (--no-build)."; exit 0; }

# ---- build the packages for this OS
export CODECRAFT_BUILD_COMMIT=$(git rev-parse HEAD)
export CODECRAFT_BUILD_DATE=$(date -u +%F)
out="$root/dist/$tag"
rm -rf "$out"
mkdir -p "$out"
target=${CARGO_TARGET_DIR:-target}
case "$target" in /*) ;; *) target="$root/$target" ;; esac

payload="$out/payload"
case "$(uname -s)" in
    Darwin)
        cargo xtask dmg --release
        cp "$target"/bundle/CodeCraft-*.dmg "$out/"
        # The foreground app a running install can switch to (self-update payload).
        cargo xtask bundle --release --version-payload "$payload"
        ;;
    Linux)
        cargo xtask package-linux --release
        cargo xtask package-deb --release
        cp "$target"/package/codecraft-*.tar.gz "$target"/package/codecraft_*.deb "$out/"
        mkdir -p "$payload" && cp "$target/release/codecraft" "$payload/"
        ;;
    MINGW*|MSYS*|CYGWIN*)
        cargo xtask package-windows-installer --release
        cp "$target"/package/codecraft-*-setup.exe "$target"/package/codecraft-*.zip "$out/"
        mkdir -p "$payload" && cp "$target/release/codecraft.exe" "$payload/"
        ;;
    *) die "no packaging for $(uname -s)" ;;
esac

# Signed update manifest + payload archive (the payload folder itself is not shipped).
cargo xtask update-pack --payload "$payload" --out "$out" --version "$next" --commit "$CODECRAFT_BUILD_COMMIT"
rm -rf "$payload"

(
    cd "$out"
    if command -v sha256sum > /dev/null; then sha256sum -- *.* > SHA256SUMS.txt; else shasum -a 256 -- *.* > SHA256SUMS.txt; fi
)
echo
echo "Built $tag in $out:"
ls -1 "$out"
echo
if $publish; then
    git push --atomic origin "HEAD:refs/heads/$(git rev-parse --abbrev-ref HEAD)" "refs/tags/$tag"
    flags=()
    case "$next" in *-*) flags+=(--prerelease) ;; esac
    # Updates are read from the repository's latest release, so a stable release must not be a draft.
    gh release create "$tag" "$out"/* --verify-tag ${flags[@]+"${flags[@]}"} --title "Code Craft $tag" --generate-notes
    echo "Published $tag. Installed apps will offer it within a day, or via Check for Updates."
else
    echo "Share it: re-run with --publish, or: git push origin HEAD $tag && gh release create $tag $out/*"
fi
