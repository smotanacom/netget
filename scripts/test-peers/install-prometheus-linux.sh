#!/usr/bin/env bash
# Required independent exporter/promtool peers; only writes to caller-owned root.
set -euo pipefail
if [[ $# != 1 || $1 != /* || $1 == / ]]; then
  printf 'Usage: %s /absolute/owned/peer-root\n' "$0" >&2
  exit 2
fi
if [[ $(uname -s) != Linux || $(uname -m) != x86_64 ]]; then
  printf 'Pinned peer installer requires Linux amd64.\n' >&2
  exit 2
fi
peer_root=$1
mkdir -p "$peer_root/downloads" "$peer_root/bin"
archive="$peer_root/downloads/prometheus-3.15.0.linux-amd64.tar.gz"
expected_sha=2a542df32eac02ee17b9d844fb2aa1de00dafa5476579ba8a3ba862e9d572ea0
if [[ ! -f $archive ]]; then
  curl --proto '=https' --proto-redir '=https' --tlsv1.2 --fail --location --retry 2 \
    --connect-timeout 15 --max-time 180 \
    https://github.com/prometheus/prometheus/releases/download/v3.15.0/prometheus-3.15.0.linux-amd64.tar.gz \
    --output "$archive.part"
  printf '%s  %s\n' "$expected_sha" "$archive.part" | sha256sum --check --status
  mv "$archive.part" "$archive"
fi
printf '%s  %s\n' "$expected_sha" "$archive" | sha256sum --check --status
# Extract only the two upstream binaries, retaining the release archive for evidence.
tar -xzf "$archive" -C "$peer_root/bin" --strip-components=1 \
  prometheus-3.15.0.linux-amd64/prometheus prometheus-3.15.0.linux-amd64/promtool
"$peer_root/bin/prometheus" --version
"$peer_root/bin/promtool" --version
