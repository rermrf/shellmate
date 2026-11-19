#!/bin/sh
# Prints the code requirement a release must satisfy: Shellmate's bundle identifier, signed by one of the
# certificates in crates/shellmate/assets/signing/trusted-certificates.txt. The in-app updater builds the
# same requirement from the same file (crates/shellmate/src/update/install.rs).
set -eu
cd "$(dirname "$0")/.."
list=crates/shellmate/assets/signing/trusted-certificates.txt
leaves=""
for pin in $(sed 's/#.*//' "$list" | awk 'NF { print tolower($1) }'); do
  if ! printf '%s\n' "$pin" | grep -Eq '^[0-9a-f]{40}$' || [ "$pin" = 0000000000000000000000000000000000000000 ]; then
    echo "$list: not a certificate SHA-1: $pin" >&2
    exit 1
  fi
  leaves="${leaves:+$leaves or }certificate leaf = H\"$pin\""
done
if [ -z "$leaves" ]; then
  echo "$list lists no certificate" >&2
  exit 1
fi
printf 'identifier "dev.shellmate.Shellmate" and (%s)\n' "$leaves"
