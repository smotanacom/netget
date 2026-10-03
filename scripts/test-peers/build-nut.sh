#!/bin/sh
# Build independent NUT 2.8.4 test peers without installing system packages.
# Usage: sh scripts/test-peers/build-nut.sh /absolute/owned/temporary/directory
set -eu
base=${1:?Supply an owned absolute temporary directory}
case "$base" in /*) ;; *) echo 'An absolute directory is required' >&2; exit 2;; esac
mkdir -p "$base"
archive="$base/nut-2.8.4.tar.gz"
source_dir="$base/nut-2.8.4"
if [ ! -f "$archive" ]; then
    curl --fail --location --max-time 120 --output "$archive" https://networkupstools.org/source/2.8/nut-2.8.4.tar.gz
fi
expected=0130ba82ea79f04ba4f34c5249a85943977efd984ed7df6aec1a518d5a3594f8
if command -v sha256sum >/dev/null 2>&1; then
    printf '%s  %s\n' "$expected" "$archive" | sha256sum -c -
else
    printf '%s  %s\n' "$expected" "$archive" | shasum -a 256 -c -
fi
if [ ! -d "$source_dir" ]; then tar -xzf "$archive" -C "$base"; fi
cd "$source_dir"
./configure --with-drivers=dummy-ups --without-ssl --without-usb --without-snmp \
    --without-neon --without-ipmi --without-powerman --without-modbus --without-gpio \
    --without-macosx_ups --without-linux_i2c --without-doc --without-python \
    --without-python2 --without-python3 --without-libsystemd --prefix="$base/nut-tools"
make -j2 -C include all
make -j2 -C common all
make -j2 -C clients upsc upscmd upsrw
make -j2 -C server upsd
make -j2 -C drivers dummy-ups
# These are libtool launchers, so retain their build directories while testing.
cat <<ENV
NUT_UPSC_BIN=$source_dir/clients/upsc
NUT_UPSCMD_BIN=$source_dir/clients/upscmd
NUT_UPSD_BIN=$source_dir/server/upsd
NUT_DUMMY_BIN=$source_dir/drivers/dummy-ups
ENV
