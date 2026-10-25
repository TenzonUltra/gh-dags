# Sourced by each case's scaffold: installs this checkout's gh-dags as the
# run's `gh dags` (in the run's own HOME, so nothing touches your real gh), and
# starts a repository with gh-dags set up in the workspace. There is no
# GitHub: runs can't push, and the cases don't need to.
set -euo pipefail
# On macOS the eval sandbox can't run /usr/bin/git (a shim into Xcode that
# writes a cache it forbids), nor follow Homebrew's symlinked git. So give the
# run a plain script that calls the real binary, first on PATH in its shells.
if [ "$(uname)" = Darwin ]; then
    real=
    for g in /opt/homebrew/bin/git /usr/local/bin/git $(type -ap git); do
        [ "$g" != /usr/bin/git ] && [ -x "$g" ] && real="$(realpath "$g")" && break
    done
    if [ -z "$real" ]; then
        echo "the eval needs a git other than /usr/bin/git (e.g. Homebrew's)" >&2
        exit 1
    fi
    mkdir -p "$HOME/bin"
    printf '#!/bin/sh\nexec %s "$@"\n' "$real" > "$HOME/bin/git"
    chmod +x "$HOME/bin/git"
    for rc in .zshenv .zprofile .zshrc .bash_profile .bashrc; do
        echo 'export PATH="$HOME/bin:$PATH"' >> "$HOME/$rc"
    done
fi
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
bin="$root/target/release/gh-dags"
[ -x "$bin" ] || bin="$root/target/debug/gh-dags"
ext="$HOME/.local/share/gh/extensions/gh-dags"
mkdir -p "$ext"
cp "$bin" "$ext/gh-dags"

git init -q -b main
git config user.name "Eval"
git config user.email "eval@example.com"
echo "# demo" > README.md
git add README.md
git commit -q -m "initial"
gh dags init --trunk main >/dev/null
