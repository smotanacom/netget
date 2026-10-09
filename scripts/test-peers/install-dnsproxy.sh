#!/bin/sh
# Download a pinned independent DoQ server into an owned temporary directory.
# Digests: https://github.com/AdguardTeam/dnsproxy/releases/expanded_assets/v0.85.0
set -eu
base=${1:?Supply an owned absolute temporary directory}
case "$base" in /*) ;; *) echo 'An absolute directory is required' >&2; exit 2;; esac
case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) platform=darwin-arm64; digest=64c2a6c2645745e24369f21c9e22661a18bfcfdcbdd8ff54af0d37328b3ea9e6 ;;
    Darwin-x86_64) platform=darwin-amd64; digest=48ea71b4f3d3f78d39f3e5bce43379bef41889e32fc45b8391598df9dd3b55c1 ;;
    Linux-aarch64) platform=linux-arm64; digest=6243b9e6c48d2fce9eee0c1170566b8474768ec87602baccbc6dc44514a83568 ;;
    Linux-x86_64) platform=linux-amd64; digest=740af768b17fe8ecc2dbc8c82c7b5224e43278a181b4fe31b0cce5d8da656332 ;;
    *) echo 'Unsupported test-peer platform' >&2; exit 2 ;;
esac
mkdir -p "$base"
archive="$base/dnsproxy-$platform-v0.85.0.tar.gz"
if [ ! -f "$archive" ]; then
    curl --fail --location --max-time 120 --output "$archive" \
        "https://github.com/AdguardTeam/dnsproxy/releases/download/v0.85.0/dnsproxy-$platform-v0.85.0.tar.gz"
fi
printf '%s  %s\n' "$digest" "$archive" | shasum -a 256 --check
tar -xzf "$archive" -C "$base"
test -x "$base/$platform/dnsproxy"
printf 'NETGET_DNSPROXY_BIN=%s/%s/dnsproxy\n' "$base" "$platform"
