#!/usr/bin/env bash
# Publish a Venticode release for self-update (see README, "Self-update").
#
# usage: publish.sh <packages-dir> s3://bucket/prefix
#   PUBLIC_URL    HTTPS URL that serves that prefix (S3 website, CloudFront...)
#   MINISIGN_KEY  secret key, MINISIGN_PUB the matching minisign.pub; the public
#                 key and manifest URL are stamped into install.sh / install.ps1
#   <packages-dir>/<platform>/venticode-tui[.exe] and venticode-harness[.exe],
#   platform being linux-x64, macos-x64, macos-arm64 or windows-x64.
# needs: jq, sha256sum, minisign, aws; MINISIGN_KEY points at the secret key.
set -euo pipefail
dir=${1:?packages dir} dest=${2:?s3://bucket/prefix}
dest=${dest%/}
public=${PUBLIC_URL:?https URL that serves the prefix}
public=${public%/}
pubkey=$(sed -n 2p "${MINISIGN_PUB:?path to minisign.pub}")
root=$(cd "$(dirname "$0")/../.." && pwd)
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/tui/Cargo.toml" | head -1)
[ "$version" = "$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/harness/Cargo.toml" | head -1)" ] ||
  { echo 'tui and harness versions differ' >&2; exit 1; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
artifacts='{}'
for platform_dir in "$dir"/*/; do
  platform=$(basename "$platform_dir")
  for file in "$platform_dir"*; do
    name=$(basename "$file")
    sha=$(sha256sum "$file" | cut -d' ' -f1)
    artifacts=$(jq -c --arg p "$platform" --arg c "${name%.exe}" --arg u "$version/$platform/$name" --arg s "$sha" \
      '.[$p][$c] = {url: $u, sha256: $s}' <<<"$artifacts")
  done
done
jq -n --arg v "$version" --argjson a "$artifacts" '{version: $v, artifacts: $a}' >"$work/manifest.json"
minisign -S -s "${MINISIGN_KEY:?path to the minisign secret key}" -m "$work/manifest.json" -x "$work/manifest.json.minisig"

for f in install.sh install.ps1; do
  sed -e "s|@MANIFEST_URL@|$public/manifest.json|" -e "s|@PUBKEY@|$pubkey|" "$root/deploy/update/$f" >"$work/$f"
done

# Binaries first, so the manifest never points at a missing object.
aws s3 cp --recursive "$dir" "$dest/$version/"
for f in manifest.json manifest.json.minisig install.sh install.ps1; do
  aws s3 cp --cache-control no-cache "$work/$f" "$dest/$f"
done
echo "published $version to $dest"
echo "install: curl -fsSL $public/install.sh | sh"
echo "install (Windows): irm $public/install.ps1 | iex"
