#!/bin/sh
# Installs a tagged vmac release on this Mac: /Applications/vmac.app and the
# `vmac` command in ~/.cargo/bin. It builds from a clean worktree at the tag,
# so uncommitted changes in the checkout are left out.
#
#   scripts/install-release.sh                   # the newest v* tag
#   scripts/install-release.sh v0.0.5            # a given tag
#   scripts/install-release.sh v0.0.5 ~/Applications
#
# Needs rsvg-convert and ImageMagick (brew install librsvg imagemagick).
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
say() { printf 'install-release: %s\n' "$*"; }

git -C "$root" fetch --tags --quiet origin 2>/dev/null || say "couldn't fetch tags; using local ones"

tag=${1:-$(git -C "$root" tag -l 'v*' --sort=-v:refname | head -1)}
dest=${2:-/Applications}
[ -n "$tag" ] || { say "no v* tags found"; exit 1; }
git -C "$root" rev-parse -q --verify "refs/tags/$tag" >/dev/null || { say "no tag $tag"; exit 1; }

work=$(mktemp -d)
tree="$work/vmac-$tag"
cleanup() {
  git -C "$root" worktree remove --force "$tree" 2>/dev/null || true
  git -C "$root" worktree prune
  rm -rf "$work"
}
trap cleanup EXIT INT TERM

say "checking out $tag"
git -C "$root" worktree add --quiet --detach "$tree" "$tag"

# Builds the release binary and installs the app.
"$tree/scripts/make-app.sh" "$dest"

# The same binary for the command line.
bin="${CARGO_HOME:-$HOME/.cargo}/bin"
mkdir -p "$bin"
install -m 755 "$tree/target/release/vmac" "$bin/vmac"
say "installed $bin/vmac"
say "vmac $tag is installed; quit and reopen vmac.app if it's running"
