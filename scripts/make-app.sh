#!/bin/sh
# Builds vmac.app (macOS): a release build, the icon from assets/logo.svg,
# and a launcher that takes your login shell's PATH, since apps opened from
# Finder, Spotlight or the Dock get a bare one (and vmac needs it to find
# language servers, fish, npx and the agents).
#
#   scripts/make-app.sh                  # installs /Applications/vmac.app
#   scripts/make-app.sh ~/Applications   # or another folder
#
# VMAC_BIN=path uses that vmac instead of building one (the release
# workflow passes a universal binary).
#
# Needs rsvg-convert and ImageMagick (brew install librsvg imagemagick).
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
dest=${1:-/Applications}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
say() { printf 'make-app: %s\n' "$*"; }

for tool in rsvg-convert magick iconutil; do
  command -v "$tool" >/dev/null 2>&1 || { say "needs $tool"; exit 1; }
done

bin=${VMAC_BIN:-}
if [ -z "$bin" ]; then
  say "building vmac $version (release)"
  cargo build --release --locked --manifest-path "$root/Cargo.toml"
  bin="$root/target/release/vmac"
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
app="$work/vmac.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin" "$app/Contents/MacOS/vmac-bin"

# The icon: the logo on Apple's grid (824 of 1024 pixels, centered), at
# every size an .icns holds.
set_dir="$work/vmac.iconset"
mkdir -p "$set_dir"
rsvg-convert -w 824 -h 824 "$root/assets/logo.svg" -o "$work/logo.png"
magick -size 1024x1024 xc:none "$work/logo.png" -gravity center -composite "$work/icon.png"
for size in 16 32 128 256 512; do
  magick "$work/icon.png" -resize "${size}x${size}" "$set_dir/icon_${size}x${size}.png"
  double=$((size * 2))
  magick "$work/icon.png" -resize "${double}x${double}" "$set_dir/icon_${size}x${size}@2x.png"
done
iconutil -c icns "$set_dir" -o "$app/Contents/Resources/vmac.icns"

# The launcher: the login shell's PATH, a home folder to start in, then vmac.
cat > "$app/Contents/MacOS/vmac" <<'EOF'
#!/bin/sh
here=$(cd "$(dirname "$0")" && pwd)
if [ -z "${VMAC_LAUNCHED:-}" ]; then
  shell=$(dscl . -read "/Users/$(id -un)" UserShell 2>/dev/null | sed 's/^UserShell: //')
  [ -x "${shell:-}" ] || shell=${SHELL:-/bin/zsh}
  case "$shell" in
    *fish) flags="-l -c" ;;
    *) flags="-l -i -c" ;;
  esac
  found=$("$shell" $flags 'printf "\n__vmac_path=%s\n" "$PATH"' 2>/dev/null </dev/null |
    sed -n 's/^__vmac_path=//p' | tail -1)
  [ -n "$found" ] && PATH=$found
  export PATH VMAC_LAUNCHED=1
fi
# Finder starts apps in /; start at home instead.
[ "$(pwd)" = "/" ] && cd "$HOME"
exec "$here/vmac-bin" "$@"
EOF
chmod +x "$app/Contents/MacOS/vmac"

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>vmac</string>
  <key>CFBundleDisplayName</key><string>vmac</string>
  <key>CFBundleIdentifier</key><string>dev.super-yojan.vmac</string>
  <key>CFBundleExecutable</key><string>vmac</string>
  <key>CFBundleIconFile</key><string>vmac</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF

# Signed for this machine (ad hoc), so macOS runs it as one app.
codesign --force --deep --sign - "$app" >/dev/null 2>&1 || say "couldn't sign it (it may still run)"

mkdir -p "$dest"
rm -rf "$dest/vmac.app"
cp -R "$app" "$dest/vmac.app"
# Tell Spotlight and the Dock about it (and its icon).
lsregister=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
[ -x "$lsregister" ] && "$lsregister" -f "$dest/vmac.app" >/dev/null 2>&1 || true
touch "$dest/vmac.app"
say "installed $dest/vmac.app"
