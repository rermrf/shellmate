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

if [[ $profile == release ]]; then
  # Local symbols only matter for debugging; the linker's ad-hoc signature is replaced below.
  strip -x "$app/Contents/MacOS/Shellmate"
fi
codesign --force --sign - "$app" >/dev/null
print "built $app ($version)"

[[ $profile == release ]] || exit 0

# A drag-to-install disk image with first-launch notes for testers.
arch=$(uname -m)
dmg=target/release/Shellmate-$version-$arch.dmg
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
cp -R "$app" "$stage/"
ln -s /Applications "$stage/Applications"
cat > "$stage/首次打开说明.txt" <<'NOTE'
Shellmate 体验版 · 安装与首次打开

1. 把 Shellmate 拖到「应用程序」文件夹。
2. 这个体验版没有 Apple 开发者签名，第一次打开时 macOS 会提示无法验证开发者：
   打开「系统设置 → 隐私与安全性」，在页面下方找到 Shellmate，点「仍要打开」，再确认一次即可。
   若提示「已损坏，无法打开」，在「终端」里执行下面这行后再打开：
     xattr -dr com.apple.quarantine /Applications/Shellmate.app
3. 仅支持 Apple 芯片（M1 及以后）的 Mac，macOS 13 或更新版本。

数据保存在本机：~/Library/Application Support/Shellmate
密码默认存入 macOS 钥匙串；首次读取时系统会询问，选「始终允许」即可。
NOTE
rm -f "$dmg"
hdiutil create -quiet -volname "Shellmate $version" -srcfolder "$stage" -fs HFS+ -format UDZO "$dmg"
print "built $dmg"
