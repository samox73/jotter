#!/usr/bin/env bash
# Sets up jotter-test/ in this clone for testing/macos/PLAYBOOK.md.
# Download jotter-aarch64-macos.tar.gz with your browser first (not curl: the
# browser sets the quarantine flag that the Gatekeeper step is about).
set -euo pipefail

[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "this needs an Apple Silicon Mac, not $(uname -s) $(uname -m)"; exit 1; }
command -v uv >/dev/null || { echo "uv is missing: brew install uv"; exit 1; }

tarball=$HOME/Downloads/jotter-aarch64-macos.tar.gz
[[ -f "$tarball" ]] || { echo "$tarball not found: download it with your browser from https://github.com/samox73/jotter/releases/latest"; exit 1; }

root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
here=$root/testing/macos
test=$root/jotter-test

mkdir -p "$test/release" "$test/my notes/ünïcode"
tar xzf "$tarball" -C "$test/release"
# macOS tar carries the quarantine flag over to the extracted files; if it
# didn't, set the same flag a browser would, so the Gatekeeper step still happens.
xattr -p com.apple.quarantine "$test/release/jotter" >/dev/null 2>&1 \
  || xattr -w com.apple.quarantine "0081;$(printf '%x' "$(date +%s)");Safari;" "$test/release/jotter"

cp "$here/test.ipynb" "$test/test.ipynb"
cp "$here/test.ipynb" "$test/my notes/ünïcode/tést.ipynb"

uv venv -q "$test/.venv"
uv pip install -q --python "$test/.venv/bin/python" ipykernel numpy matplotlib sympy pandas

cat > "$test/env.zsh" <<ENV
export PATH="$test/release:\$PATH"
fpath=("$test/release/completions" \$fpath)
autoload -Uz compinit && compinit
ENV

echo "ready: cd $test && ./release/jotter --version"
