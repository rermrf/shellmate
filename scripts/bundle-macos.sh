#!/bin/zsh
# Build Shellmate.app with the app icon (scheme 1c).
#
#   scripts/bundle-macos.sh                       # release build for this Mac -> target/release/Shellmate.app + dmg
#   scripts/bundle-macos.sh release --universal   # Apple silicon + Intel in one binary (released by CI)
#   scripts/bundle-macos.sh release --arm64       # Apple silicon only, macOS 13+ (released by CI; reuses
#                                                 # the arm64 half of a previous --universal build)
#   scripts/bundle-macos.sh debug                 # wrap the existing debug build -> target/debug/Shellmate.app
#
# Signing: SIGN_IDENTITY if set, otherwise the project's self-signed "Shellmate Code Signing" certificate
# when a keychain has it (scripts/make-signing-cert.sh, docs/signing.md), otherwise ad-hoc ("-").
# A certificate-signed app must match crates/shellmate/assets/signing/certificate.sha1, the certificate
# the in-app updater trusts. A Developer ID identity also turns on the hardened runtime and a timestamp.
#
# Environment:
#   SIGN_IDENTITY   codesign identity, or "-" to force ad-hoc signing.
#   SIGN_KEYCHAIN   keychain file holding the identity (CI uses a temporary one).
#   MACOSX_DEPLOYMENT_TARGET   defaults to 13.0 for --universal and --arm64, matching LSMinimumSystemVersion.
#
# The Dock icon also follows the system dark appearance at runtime (crates/shellmate/src/app_icon.rs).
set -euo pipefail
cd "${0:A:h}/.."

profile=release
target=native
for arg in "$@"; do
  case $arg in
    release|debug) profile=$arg ;;
    --universal) target=universal ;;
    --arm64) target=arm64 ;;
    *) print -u2 "usage: $0 [release|debug] [--universal|--arm64]"; exit 2 ;;
  esac
done
if [[ $target != native && $profile != release ]]; then
  print -u2 "--$target only applies to release builds"; exit 2
fi

if [[ $target != native ]]; then
  # Both release flavours build per-triple with the same deployment target, so the arm64 build is
  # shared: --arm64 after --universal only re-bundles.
  export MACOSX_DEPLOYMENT_TARGET=${MACOSX_DEPLOYMENT_TARGET:-13.0}
  triples=(aarch64-apple-darwin)
  [[ $target == universal ]] && triples+=(x86_64-apple-darwin)
  for triple in $triples; do
    cargo build --release -p shellmate --target $triple
  done
fi

if [[ $target == universal ]]; then
  binary=target/release/shellmate-universal
  mkdir -p target/release
  lipo -create -output $binary \
    target/aarch64-apple-darwin/release/shellmate \
    target/x86_64-apple-darwin/release/shellmate
  arch=universal
elif [[ $target == arm64 ]]; then
  binary=target/aarch64-apple-darwin/release/shellmate
  arch=arm64
else
  case $profile in
    release) cargo build --release -p shellmate ;;
    debug) [[ -x target/debug/shellmate ]] || cargo build -p shellmate ;;
  esac
  binary=target/$profile/shellmate
  arch=$(uname -m)
fi

version=$(cargo metadata --no-deps --format-version 1 --offline \
  | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="shellmate"))')
app=target/$profile/Shellmate.app

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary" "$app/Contents/MacOS/Shellmate"
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
identity=${SIGN_IDENTITY:-}
if [[ -z $identity ]]; then
  if security find-identity -p codesigning 2>/dev/null | grep -q '"Shellmate Code Signing"'; then
    identity="Shellmate Code Signing"
  else
    identity=-
  fi
fi
keychain=()
[[ -n ${SIGN_KEYCHAIN:-} ]] && keychain=(--keychain "$SIGN_KEYCHAIN")
case $identity in
  -) codesign --force --sign - "$app" >/dev/null ;;
  "Developer ID Application:"*)
    codesign --force --options runtime --timestamp $keychain --sign "$identity" "$app" >/dev/null ;;
  *) codesign --force $keychain --sign "$identity" "$app" >/dev/null ;;
esac
if [[ $identity != - ]]; then
  pinned=$(tr -d ' \n' < crates/shellmate/assets/signing/certificate.sha1 | tr 'A-F' 'a-f')
  if ! codesign --verify --deep --strict \
      -R="identifier \"dev.shellmate.Shellmate\" and certificate leaf = H\"$pinned\"" "$app" 2>/dev/null; then
    print -u2 "$app is signed by \"$identity\", but the updater only trusts the certificate in"
    print -u2 "crates/shellmate/assets/signing/certificate.sha1 ($pinned); see docs/signing.md."
    exit 1
  fi
fi
print "built $app ($version, $arch, signed by ${${identity:#-}:-ad-hoc})"

[[ $profile == release ]] || exit 0

if [[ $arch == universal ]]; then
  supported="支持 Apple 芯片与 Intel 处理器的 Mac"
else
  supported="仅支持 Apple 芯片（M1 及以后）的 Mac"
fi

# A drag-to-install disk image with first-launch notes.
dmg=target/release/Shellmate-$version-$arch.dmg
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
cp -R "$app" "$stage/"
ln -s /Applications "$stage/Applications"
cat > "$stage/首次打开说明.txt" <<NOTE
Shellmate · 安装与首次打开

1. 把 Shellmate 拖到「应用程序」文件夹。
2. 这个版本没有 Apple 开发者签名，第一次打开时 macOS 会提示无法验证开发者：
   打开「系统设置 → 隐私与安全性」，在页面下方找到 Shellmate，点「仍要打开」，再确认一次即可。
   若提示「已损坏，无法打开」，在「终端」里执行下面这行后再打开：
     xattr -dr com.apple.quarantine /Applications/Shellmate.app
3. ${supported}，macOS 13 或更新版本。

数据保存在本机：~/Library/Application Support/Shellmate
密码默认存入 macOS 钥匙串；首次读取时系统会询问，选「始终允许」即可。
NOTE
rm -f "$dmg"
hdiutil create -quiet -volname "Shellmate $version" -srcfolder "$stage" -fs HFS+ -format UDZO "$dmg"
print "built $dmg"
