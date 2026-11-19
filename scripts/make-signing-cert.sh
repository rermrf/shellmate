#!/bin/zsh
# Create the self-signed code-signing certificate that Shellmate releases are signed with.
#
#   scripts/make-signing-cert.sh [output-dir]      # default: ~/Documents/shellmate-signing
#
# Writes into a private directory outside the repository:
#   shellmate-signing.p12   certificate + private key; GitHub Actions signs releases with it
#   password.txt            the password of the .p12
#   shellmate-signing.cer   the public certificate (safe to share)
# and prints the certificate's SHA-1, which the in-app updater pins.
#
# Nothing secret is printed. Run it once: a different certificate makes every installed copy reject
# automatic updates and asks for keychain access again. See docs/signing.md.
set -euo pipefail

name="Shellmate Code Signing"
repo=${0:A:h:h}
# Two steps: zsh mangles the "-" of a default value nested inside a modifier, e.g. ${${1:-a-b}:A}.
out=${1:-$HOME/Documents/shellmate-signing}
out=${out:A}

if [[ $out == $repo || $out == $repo/* ]]; then
  print -u2 "The private key must not live inside the repository: $out"; exit 2
fi
if [[ -e $out/shellmate-signing.p12 ]]; then
  print -u2 "$out/shellmate-signing.p12 already exists. Keep using it; a new certificate would make"
  print -u2 "installed copies reject updates. Delete it yourself first if you really mean to replace it."
  exit 2
fi

# LibreSSL's PKCS#12 encryption is the kind `security import` accepts; OpenSSL 3's default is not.
ssl=/usr/bin/openssl
umask 077
mkdir -p "$out"
chmod 700 "$out"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

$ssl req -x509 -newkey rsa:2048 -sha256 -days 7300 -nodes \
  -keyout "$work/key.pem" -out "$work/cert.pem" -subj "/CN=$name" \
  -addext "basicConstraints=critical,CA:FALSE" \
  -addext "keyUsage=critical,digitalSignature" \
  -addext "extendedKeyUsage=critical,codeSigning" 2>/dev/null
$ssl rand -base64 30 | tr -d '\n' > "$out/password.txt"
$ssl pkcs12 -export -name "$name" -inkey "$work/key.pem" -in "$work/cert.pem" \
  -out "$out/shellmate-signing.p12" -passout "file:$out/password.txt"
$ssl x509 -in "$work/cert.pem" -outform der -out "$out/shellmate-signing.cer"
chmod 644 "$out/shellmate-signing.cer"

sha1=$($ssl x509 -in "$work/cert.pem" -noout -fingerprint -sha1 | cut -d= -f2 | tr -d ':')
expires=$($ssl x509 -in "$work/cert.pem" -noout -enddate | cut -d= -f2)
cat <<DONE
已生成「$name」（有效期至 $expires）：
  $out/shellmate-signing.p12   证书与私钥（保密，务必备份）
  $out/password.txt            .p12 的密码（保密，务必备份）
  $out/shellmate-signing.cer   公开证书

证书 SHA-1：$sha1

下一步见 docs/signing.md：导入本机钥匙串、添加 GitHub Secrets、备份。
DONE
