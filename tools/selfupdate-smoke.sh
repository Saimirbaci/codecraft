#!/bin/sh
# No network, agent, real home, or production version store. Includes uncommitted source.
set -eu
# A caller launched from a git hook/difftool must not redirect the temporary git operations.
unset GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR GIT_INDEX_FILE GIT_PREFIX GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_SHALLOW_FILE GIT_GRAFT_FILE GIT_IMPLICIT_WORK_TREE
export CARGO_TARGET_DIR="$(pwd)/target-m18" CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=3 CARGO_NET_OFFLINE=true
root=$(pwd)
smoke=$(mktemp -d "${TMPDIR:-/tmp}/codecraft smoke.XXXXXX")
trap 'rm -rf "$smoke"' EXIT HUP INT TERM
mkdir -p "$smoke/tree" "$smoke/home"
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0
# Copy only repository files; targets and credentials are never included.
python3 - "$root" "$smoke/tree" <<'PY'
import os, shutil, subprocess, sys
root,dest=sys.argv[1:]
files=subprocess.check_output(['git','ls-files','--cached','--others','--exclude-standard','-z'],cwd=root).split(b'\0')
for raw in set(files):
    if not raw: continue
    rel=os.fsdecode(raw); src=os.path.join(root,rel); dst=os.path.join(dest,rel)
    if not os.path.isfile(src) or os.path.islink(src): continue
    os.makedirs(os.path.dirname(dst),exist_ok=True); shutil.copy2(src,dst)
PY
git -C "$smoke/tree" init -q -b main
git -C "$smoke/tree" add -- .
git -C "$smoke/tree" -c user.name=Smoke -c user.email=smoke@codecraft.invalid commit -qm snapshot
git clone --bare -- "$smoke/tree" "$smoke/remote.git"
# Build drivers before changing HOME (cargo's cache stays at its existing explicit location).
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
cargo build --offline -p codecraft-launcher -p codecraft-selfupdate --example smoke
cargo build --offline -p codecraft-launcher
HOME="$smoke/home" "$CARGO_TARGET_DIR/debug/examples/smoke" "$smoke/base" "$smoke/remote.git" "$CARGO_TARGET_DIR/debug/codecraft-launcher"
