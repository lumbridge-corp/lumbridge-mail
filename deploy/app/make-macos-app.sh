#!/usr/bin/env bash
# Assemble "Lumbridge Mail.app" on macOS from a release binary (no sudo).
#
#   deploy/app/make-macos-app.sh [--binary PATH] [--out DIR] [--install]
#
# Builds DIR/Lumbridge Mail.app (default DIR: target/macos) with Contents/MacOS/lumbridge-mail,
# Contents/MacOS/lumbridge-maild (the app starts it from there: it looks next to itself first) and
# Contents/MacOS/lbmail (all three must sit next to --binary), the icon, and an
# Info.plist with bundle id com.lumbridgecorp.mail that claims the mailto: URL scheme. The bundle is
# signed ad hoc (codesign -s -) so Gatekeeper on Apple silicon runs it locally. --install copies it to
# ~/Applications and registers it with Launch Services; choosing it as the default mail app is then done
# in Mail.app > Settings > General > Default email reader (macOS asks the user, no script can).
#
# --install also links lbmail into ~/.local/bin when that directory exists.
#
# Build first, on the Mac:  cargo build --release -p lumbridge-mail-app -p lumbridge-maild
set -euo pipefail

app_id=com.lumbridgecorp.mail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../.." && pwd)
binary="$repo/target/release/lumbridge-mail"
out="$repo/target/macos"
install=0

while [ $# -gt 0 ]; do
  case "$1" in
    --binary) binary=${2:?--binary needs a path}; shift 2 ;;
    --out) out=${2:?--out needs a directory}; shift 2 ;;
    --install) install=1; shift ;;
    -h|--help) sed -n '2,16p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

if [ "$(uname -s)" != Darwin ]; then
  echo "This assembles a macOS app bundle; run it on the Mac." >&2
  exit 1
fi
if [ ! -x "$binary" ]; then
  echo "No binary at $binary. Build it with: cargo build --release -p lumbridge-mail-app (or pass --binary)." >&2
  exit 1
fi
bin_src=$(dirname "$binary")
for b in lumbridge-maild lbmail; do
  if [ ! -x "$bin_src/$b" ]; then
    echo "No $bin_src/$b. Build it too: cargo build --release -p lumbridge-mail-app -p lumbridge-maild" >&2
    exit 1
  fi
done
for b in "$binary" "$bin_src/lumbridge-maild" "$bin_src/lbmail"; do
  if ! file "$b" | grep -q "Mach-O"; then
    echo "$b is not a macOS binary." >&2
    exit 1
  fi
done

version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo/crates/lumbridge-mail-app/Cargo.toml" | head -1)
version=${version:-0.1.0}
app="$out/Lumbridge Mail.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
install -m 0755 "$binary" "$app/Contents/MacOS/lumbridge-mail"
install -m 0755 "$bin_src/lumbridge-maild" "$app/Contents/MacOS/lumbridge-maild"
install -m 0755 "$bin_src/lbmail" "$app/Contents/MacOS/lbmail"
install -m 0644 "$here/icons/lumbridge-mail.icns" "$app/Contents/Resources/lumbridge-mail.icns"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleDisplayName</key><string>Lumbridge Mail</string>
  <key>CFBundleName</key><string>Lumbridge Mail</string>
  <key>CFBundleExecutable</key><string>lumbridge-mail</string>
  <key>CFBundleIdentifier</key><string>$app_id</string>
  <key>CFBundleIconFile</key><string>lumbridge-mail</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.productivity</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSPrincipalClass</key><string>NSApplication</string>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
  <key>CFBundleURLTypes</key>
  <array>
    <dict>
      <key>CFBundleURLName</key><string>Email Address URL</string>
      <key>CFBundleURLSchemes</key><array><string>mailto</string></array>
      <key>LSHandlerRank</key><string>Owner</string>
    </dict>
  </array>
</dict>
</plist>
PLIST
plutil -lint "$app/Contents/Info.plist" >/dev/null
printf 'APPL????' > "$app/Contents/PkgInfo"

if command -v codesign >/dev/null 2>&1; then
  codesign --force --deep --sign - "$app"
fi
echo "Built $app"

if [ "$install" = 1 ]; then
  dest="$HOME/Applications/Lumbridge Mail.app"
  mkdir -p "$HOME/Applications"
  rm -rf "$dest"
  cp -R "$app" "$dest"
  lsregister=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
  if [ -x "$lsregister" ]; then
    "$lsregister" -f "$dest"
  fi
  if [ -d "$HOME/.local/bin" ]; then
    ln -sfn "$dest/Contents/MacOS/lbmail" "$HOME/.local/bin/lbmail"
    echo "Linked ~/.local/bin/lbmail."
  fi
  echo "Installed $dest. To make it the default mail app: Mail.app > Settings > General > Default email reader."
fi
