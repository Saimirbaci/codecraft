#!/usr/bin/env bash
# Cut a new version: bump, commit, tag and push. Building and publishing the release is a
# separate, manual step (Actions › Release › Run workflow), so pushing the tag starts nothing.
#
#   ./release.sh patch|minor|major|<x.y.z[-pre]> [--yes] [--no-push] [--dry-run]
#
#   patch     0.1.0 → 0.1.1 (a pre-release 0.2.0-rc.1 → 0.2.0)
#   minor     0.1.0 → 0.2.0
#   major     0.1.0 → 1.0.0
#   0.3.0-beta.1   that exact version
#   --yes      don't ask for confirmation
#   --no-push  commit and tag locally only (push later: git push --atomic origin main vX.Y.Z)
#   --dry-run  show what would happen; change nothing
#
# Checks first: on main, clean working tree, up to date with origin/main, tag not taken.
# The version lives in Cargo.toml ([workspace.package]); `cargo xtask version` rewrites it and
# the workspace's own entries in Cargo.lock (docs/release.md › Versions).

set -euo pipefail

# Overridable for the script's own tests only.
XTASK=${CODECRAFT_RELEASE_XTASK:-cargo xtask}
LOCK_CHECK=${CODECRAFT_RELEASE_LOCK_CHECK:-cargo metadata --locked --offline --format-version 1}
REMOTE=${CODECRAFT_RELEASE_REMOTE:-origin}
BRANCH=main

usage() {
    sed -n '5,13p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
}

die() {
    echo "release.sh: $*" >&2
    exit 1
}

level=""
yes=false
push=true
dry_run=false
for arg in "$@"; do
    case "$arg" in
        --yes|-y) yes=true ;;
        --no-push) push=false ;;
        --dry-run|-n) dry_run=true ;;
        -h|--help) usage ;;
        patch|minor|major) level=$arg ;;
        v[0-9]*|[0-9]*) level=${arg#v} ;;
        *) echo "release.sh: unknown argument '$arg'" >&2; usage ;;
    esac
done
[ -n "$level" ] || usage

cd "$(dirname "$0")"

# ---- checks
[ "$(git rev-parse --abbrev-ref HEAD)" = "$BRANCH" ] || die "switch to $BRANCH first (on $(git rev-parse --abbrev-ref HEAD))"
if [ -n "$(git status --porcelain)" ]; then
    if $dry_run; then
        echo "warning: the working tree has changes; a real run refuses until they are committed or stashed" >&2
    else
        die "the working tree has changes; commit or stash them first"
    fi
fi
if $push || ! $dry_run; then
    git fetch --quiet "$REMOTE" "$BRANCH" --tags || die "cannot fetch $REMOTE"
    behind=$(git rev-list --count "HEAD..$REMOTE/$BRANCH")
    [ "$behind" = 0 ] || die "$BRANCH is $behind commit(s) behind $REMOTE/$BRANCH; pull first"
fi

current=$($XTASK version | tail -n 1)
next=$($XTASK version "$level" --dry-run | tail -n 1)
tag="v$next"
if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
    die "tag $tag already exists"
fi
if $push && git ls-remote --exit-code --tags "$REMOTE" "refs/tags/$tag" >/dev/null 2>&1; then
    die "tag $tag already exists on $REMOTE"
fi

echo "Version: $current → $next"
echo "Commit:  Release $tag (Cargo.toml, Cargo.lock) on $BRANCH"
echo "Tag:     $tag"
if $push; then echo "Push:    $BRANCH and $tag to $REMOTE (no release starts)"; else echo "Push:    no (--no-push)"; fi

if $dry_run; then
    echo "(dry run: nothing changed)"
    exit 0
fi
if ! $yes; then
    read -r -p "Go ahead? [y/N] " answer
    case "$answer" in y|Y|yes|YES) ;; *) echo "Cancelled."; exit 1 ;; esac
fi

# ---- bump, commit, tag (undo the bump if anything fails before the commit)
committed=false
restore() {
    if ! $committed; then
        git checkout --quiet -- Cargo.toml Cargo.lock 2>/dev/null || true
    fi
}
trap restore EXIT

$XTASK version "$level" >/dev/null
$LOCK_CHECK >/dev/null || die "Cargo.lock is out of date after the bump (see the error above)"
git add Cargo.toml Cargo.lock
git commit --quiet -m "Release $tag"
committed=true
git tag -a "$tag" -m "Code Craft $next"
echo "Committed and tagged $tag."

if $push; then
    git push --atomic "$REMOTE" "HEAD:refs/heads/$BRANCH" "refs/tags/$tag"
    echo "Pushed $BRANCH and $tag to $REMOTE."
else
    echo "Not pushed. When ready: git push --atomic $REMOTE $BRANCH $tag"
fi

cat <<EOF

To build the packages and create the release (draft by default):
  GitHub: Actions › Release › Run workflow › tag: $tag
  or:     gh workflow run release.yml -f tag=$tag
EOF
