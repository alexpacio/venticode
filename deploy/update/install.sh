#!/bin/sh
# Venticode installer for Linux and macOS:
#   curl -fsSL <release-url>/install.sh | sh              (venticode-tui and venticode-harness)
#   curl -fsSL <release-url>/install.sh | sh -s -- tui    (only one of tui, harness)
# publish.sh stamps the two placeholders below; VENTICODE_INSTALL_DIR overrides ~/.local/bin.
set -eu
MANIFEST_URL='@MANIFEST_URL@'
PUBKEY='@PUBKEY@'

die() { echo "venticode install: $*" >&2; exit 1; }
case $MANIFEST_URL in
  https://*|http://127.0.0.1*|http://localhost*) ;;
  *) die "this installer was not stamped with an https manifest URL (use the published install.sh)" ;;
esac
for tool in curl awk; do command -v "$tool" >/dev/null 2>&1 || die "$tool is required"; done
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) platform=linux-x64 ;;
  Darwin-x86_64) platform=macos-x64 ;;
  Darwin-arm64) platform=macos-arm64 ;;
  *) die "unsupported platform $(uname -s) $(uname -m)" ;;
esac
sha256() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1; }

dir=${VENTICODE_INSTALL_DIR:-$HOME/.local/bin}
[ $# -gt 0 ] || set -- tui harness
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

curl -fsSL "$MANIFEST_URL" -o "$tmp/manifest.json"
if command -v minisign >/dev/null 2>&1; then
  curl -fsSL "$MANIFEST_URL.minisig" -o "$tmp/manifest.json.minisig"
  minisign -Vm "$tmp/manifest.json" -P "$PUBKEY" -x "$tmp/manifest.json.minisig" >/dev/null ||
    die "the manifest signature does not verify"
else
  echo "minisign not found: trusting HTTPS and the sha256 for this install (later updates are signature-checked)" >&2
fi

mkdir -p "$dir"
for component in "$@"; do
  case $component in tui|harness) ;; *) die "unknown component $component (use tui or harness)" ;; esac
  name=venticode-$component
  # publish.sh writes the manifest pretty-printed, one key per line.
  entry=$(awk -F'"' -v p="$platform" -v c="$name" '
    $2 ~ /^(linux|macos|windows)-/ { inp = ($2 == p); inc = 0; next }
    inp && $2 == c { inc = 1; next }
    inc && $2 == "url" { url = $4 }
    inc && $2 == "sha256" { print url " " $4; exit }' "$tmp/manifest.json")
  [ -n "$entry" ] || die "$name is not published for $platform"
  url=${entry% *}
  case $url in http://*|https://*) ;; *) url=${MANIFEST_URL%/*}/$url ;; esac
  curl -fsSL "$url" -o "$tmp/$name"
  [ "$(sha256 "$tmp/$name")" = "${entry#* }" ] || die "$name does not match the manifest sha256"
  cp "$tmp/$name" "$dir/.$name.new"
  chmod 755 "$dir/.$name.new"
  mv -f "$dir/.$name.new" "$dir/$name"
  echo "installed $dir/$name"
done
# The binaries read this to find the manifest when they update themselves.
printf '{"url":"%s","pubkey":"%s"}\n' "$MANIFEST_URL" "$PUBKEY" >"$dir/update.json"
case ":$PATH:" in *":$dir:"*) ;; *) echo "add $dir to your PATH to run the commands" ;; esac
