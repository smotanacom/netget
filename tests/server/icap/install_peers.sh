#!/usr/bin/env bash
# Pinned, unchanged c-icap 0.6.5 (LGPL-2.1) built from its release tarball into an owned
# prefix: the `c-icap` server with its echo service and the `c-icap-client` tool. Optional
# libraries are disabled; nothing is installed outside the prefix (SOCKDIR included).
set -euo pipefail
if [[ $# != 1 || "$1" != /* || "$1" == / ]]; then
  printf '%s\n' 'usage: install_peers.sh /absolute/owned/peer/root' >&2
  exit 2
fi
root="$1"
prefix="$root/install"
mkdir -p "$root"
if [[ ! -x "$prefix/bin/c-icap-client" ]]; then
  archive="$root/c_icap-0.6.5.tar.gz"
  if [[ ! -f "$archive" ]]; then
    curl --fail --location --silent --show-error --proto '=https' --max-time 180 --max-filesize 4194304 \
      "https://downloads.sourceforge.net/project/c-icap/c-icap/0.6.x/c_icap-0.6.5.tar.gz" --output "$archive"
  fi
  printf '%s  %s\n' 82e457b3f234d56f537c70ec76a1d51ce8f6a55522592f2df07fc2557856f184 "$archive" | shasum -a 256 -c - >/dev/null
  rm -rf "$root/src" && mkdir -p "$root/src"
  tar -xzf "$archive" -C "$root/src"
  ( cd "$root/src/c_icap-0.6.5"
    ./configure --prefix="$prefix" --without-bdb --without-zlib --without-bzlib --without-brotli --without-zstd \
      --without-openssl --without-pcre --without-pcre2 --without-ldap --without-memcached >/dev/null
    make -j4 >/dev/null
    make install SOCKDIR="$prefix/var/run/c-icap" >/dev/null )
fi
"$prefix/bin/c-icap-client" -V 2>&1 | grep -q '0.6.5'
printf 'export NETGET_C_ICAP=%s\n' "$prefix"
