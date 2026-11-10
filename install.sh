#!/bin/sh
# vmac installer: builds vmac with Rust's cargo and puts it on your PATH
# (~/.cargo/bin), then makes its config folder.
#
#   curl -fsSL https://raw.githubusercontent.com/Super-Yojan/vmac/master/install.sh | sh
#
# The latest release by default; VMAC_VERSION=master builds the latest code,
# VMAC_VERSION=v0.0.1 (another tag) that release.
set -eu

repo="https://github.com/Super-Yojan/vmac"
version="${VMAC_VERSION:-v0.0.8}"

say() { printf 'vmac: %s\n' "$*"; }
die() { say "$*" >&2; exit 1; }

command -v cargo >/dev/null 2>&1 ||
  die "needs Rust's cargo; install it from https://rustup.rs and run this again"
command -v git >/dev/null 2>&1 || die "needs git"

case "$(uname -s)" in
  Darwin) ;;
  Linux) say "Linux builds need GPUI's system libraries (Vulkan, X11/Wayland, fontconfig)" ;;
  *) say "untested on $(uname -s); trying anyway" ;;
esac

if [ "$version" = "master" ]; then
  say "building the latest vmac (a few minutes)"
  cargo install --git "$repo" --branch master --locked vmac
else
  say "building vmac $version (a few minutes)"
  cargo install --git "$repo" --tag "$version" --locked vmac
fi

config="$HOME/.config/vmac"
mkdir -p "$config/plugins"
say "installed: $(command -v vmac || echo "$HOME/.cargo/bin/vmac")"
say "config goes in $config (init.scm, init.lua, plugins/)"
say "start it with: vmac FILE"
