#!/usr/bin/env bash
# Pinned, unchanged independent RDAP peers in an owned root:
#   ICANN icann-rdap-srv / icann-rdap-cli 1.0.0 (MIT OR Apache-2.0), built from crates.io with
#   their own published lockfiles (--locked), and OpenRDAP 0.10.2 (MIT) through the Go module
#   proxy, verified by the checksum database. Prints the NETGET_* variables the tests read.
set -euo pipefail
if [[ $# != 1 || "$1" != /* || "$1" == / ]]; then
  printf '%s\n' 'usage: install_peers.sh /absolute/owned/peer/root' >&2
  exit 2
fi
root="$1"
mkdir -p "$root"
if [[ ! -x "$root/bin/rdap-srv" || ! -x "$root/bin/rdap" ]]; then
  CARGO_TARGET_DIR="$root/cargo-target" cargo install --locked --root "$root" icann-rdap-srv --version 1.0.0
  CARGO_TARGET_DIR="$root/cargo-target" cargo install --locked --root "$root" icann-rdap-cli --version 1.0.0
fi
if [[ ! -x "$root/gobin/rdap" ]]; then
  GOPATH="$root/go-path" GOMODCACHE="$root/go-modules" GOCACHE="$root/go-cache" GOBIN="$root/gobin" GOTOOLCHAIN=local \
    go install github.com/openrdap/rdap/cmd/rdap@v0.10.2
fi
"$root/gobin/rdap" --version 2>&1 | grep -q 'v0.10.2'
printf 'export NETGET_OPENRDAP=%s\n' "$root/gobin/rdap"
printf 'export NETGET_ICANN_RDAP=%s\n' "$root/bin/rdap"
printf 'export NETGET_ICANN_RDAP_SRV=%s\n' "$root/bin/rdap-srv"
printf 'export NETGET_ICANN_RDAP_SRV_DATA=%s\n' "$root/bin/rdap-srv-data"
