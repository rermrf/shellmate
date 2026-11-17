#!/bin/zsh
# Build Shellmate.app with the app icon (scheme 1c).
#
#   scripts/bundle-macos.sh            # release build -> target/release/Shellmate.app
#   scripts/bundle-macos.sh debug      # wrap the existing debug build -> target/debug/Shellmate.app
#
# The bundle is ad-hoc signed. The Dock icon also follows the system dark
# appearance at runtime (crates/shellmate/src/app_icon.rs).
set -euo pipefail
cd "${0:A:h}/.."

profile=${1:-release}
case $profile in
  release) cargo build --release -p shellmate ;;
  debug) [[ -x target/debug/shellmate ]] || cargo build -p shellmate ;;
  *) print -u2 "usage: $0 [release|debug]"; exit 2 ;;
esac

version=$(cargo metadata --no-deps --format-version 1 --offline \
  | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="shellmate"))')
app=target/$profile/Shellmate.app

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "target/$profile/shellmate" "$app/Contents/MacOS/Shellmate"
cp crates/shellmate/assets/app-icon/Shellmate.icns "$app/Contents/Resources/Shellmate.icns"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Shellmate</string>
  <key>CFBundleDisplayName</key><string>Shellmate</string>
  <key>CFBundleIdentifier</key><string>dev.shellmate.Shellmate</string>
  <key>CFBundleExecutable</key><string>Shellmate</string>
  <key>CFBundleIconFile</key><string>Shellmate</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

codesign --force --sign - "$app" >/dev/null
print "built $app ($version)"
