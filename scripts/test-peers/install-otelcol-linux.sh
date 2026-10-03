#!/usr/bin/env bash
# Official core Collector peer in caller-owned storage; no global installation.
set -euo pipefail
peer_root="${1:?absolute owned peer directory required}"
case "$peer_root" in /*) ;; *) echo 'Peer directory must be absolute' >&2; exit 2 ;; esac
case "$(uname -s)-$(uname -m)" in Linux-x86_64) ;; *) echo 'Pinned peer supports Linux amd64' >&2; exit 2 ;; esac
mkdir -p "$peer_root"
archive="$peer_root/otelcol_0.162.0_linux_amd64.tar.gz"
curl --fail --location --proto '=https' --tlsv1.2 --retry 3 \
  'https://github.com/open-telemetry/opentelemetry-collector-releases/releases/download/v0.162.0/otelcol_0.162.0_linux_amd64.tar.gz' \
  --output "$archive"
printf '%s  %s\n' 'f99929987a915d3c6b2c9b15bc4938c5cea903a37a3f49e478024a0fa0772339' "$archive" | sha256sum --check --strict
# Select the binary explicitly; the verified archive does not become a source tree.
tar --no-same-owner -xzf "$archive" -C "$peer_root" otelcol
chmod 755 "$peer_root/otelcol"
"$peer_root/otelcol" --version | tee "$peer_root/version.txt"
grep -F '0.162.0' "$peer_root/version.txt" >/dev/null
rm -- "$archive"
