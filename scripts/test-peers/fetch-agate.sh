#!/bin/sh
# Fetch the pinned independent Gemini peer, without system installation or a build cache.
# Usage: sh scripts/test-peers/fetch-agate.sh /absolute/owned/temporary/directory
set -eu
base=${1:?Supply an owned absolute temporary directory}
case "$base" in /*) ;; *) echo 'An absolute directory is required' >&2; exit 2;; esac
case "$(uname -s)-$(uname -m)" in
 Darwin-arm64) target=aarch64-apple-darwin; expected=08827bc51c0b77073dbf9573bff3aa67e1f9b34ef599bac82fbc642a5d7c1e4b;;
 Darwin-x86_64) target=x86_64-apple-darwin; expected=7da63de1d858f58fd953ae89a70e9d34d847257ea1a2e2fd26bef3ab434f3464;;
 Linux-aarch64) target=aarch64-unknown-linux-gnu; expected=5e5e2f81d127da77a4b12e78d3d06e28b639246103bf9e8508bdb0103429b8f2;;
 Linux-x86_64) target=x86_64-unknown-linux-gnu; expected=32379846ff14ce980377455bd935dd8635c56c61a0f336fb76aa461195110404;;
 *) echo 'No pinned Agate binary for this platform' >&2; exit 2;;
esac
mkdir -p "$base"
archive="$base/agate-3.3.24.$target.gz"
curl --fail --location --max-time 120 --output "$archive" "https://github.com/mbrubeck/agate/releases/download/v3.3.24/agate.$target.gz"
if command -v sha256sum >/dev/null 2>&1; then
 printf '%s  %s\n' "$expected" "$archive" | sha256sum -c -
else
 printf '%s  %s\n' "$expected" "$archive" | shasum -a 256 -c -
fi
gzip -dc "$archive" > "$base/agate-3.3.24"
chmod +x "$base/agate-3.3.24"
"$base/agate-3.3.24" --version
printf 'NETGET_TEST_AGATE=%s/agate-3.3.24\n' "$base"
