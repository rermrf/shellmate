#!/bin/zsh
# Compile the Finder Sync extension binary from macos/FinderExtension for one or more architectures.
#
#   scripts/build-finder-extension.sh <output> arm64            # one slice
#   scripts/build-finder-extension.sh <output> arm64 x86_64     # universal binary
#
# Used by scripts/bundle-macos.sh and by CI, so both compile the extension the same way.
set -euo pipefail
cd "${0:A:h}/.."

if (( $# < 2 )); then
  print -u2 "usage: $0 <output> <arch>..."; exit 2
fi
out=${1:A}; shift
if ! command -v swiftc >/dev/null; then
  print -u2 "Building Shellmate's Finder extension requires swiftc (Xcode or Command Line Tools)."
  exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
for arch in "$@"; do
  swiftc -O -target "$arch-apple-macos13.0" -application-extension -module-name ShellmateFinder \
    -framework FinderSync -Xlinker -e -Xlinker _NSExtensionMain \
    macos/FinderExtension/ShellmateFinder.swift -o "$work/$arch"
done
mkdir -p "${out:h}"
if (( $# == 1 )); then
  cp "$work/$1" "$out"
else
  lipo -create "$work/"${^@} -output "$out"
fi
