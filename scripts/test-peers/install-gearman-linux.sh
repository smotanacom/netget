#!/usr/bin/env bash
# Build only the independent Gearman wire/CLI peers used by the blocking suite.
# Dependencies (Ubuntu): build-essential curl ca-certificates pkg-config libtool
# libevent-dev libboost-program-options-dev uuid-dev gperf. No system installation here.
set -euo pipefail
if [[ $# != 1 || $1 != /* || $1 == / ]]; then
  printf 'Usage: %s /absolute/owned/peer-root\n' "$0" >&2
  exit 2
fi
if [[ $(uname -s) != Linux ]]; then
  printf 'This installer requires Linux. macOS peers use Homebrew gearman 2.1.0.\n' >&2
  exit 2
fi
for command in curl sha256sum tar make timeout c++ pkg-config install gperf; do
  command -v "$command" >/dev/null || { printf 'Missing build dependency: %s\n' "$command" >&2; exit 2; }
done
peer_root=$1
mkdir -p "$peer_root"
peer_root=$(cd "$peer_root" && pwd -P)
if [[ $peer_root == / ]]; then exit 2; fi
mkdir -p "$peer_root/downloads" "$peer_root/source" "$peer_root/bin" "$peer_root/logs"
report_failure() {
  local status=$?
  trap - ERR
  printf 'Gearman peer installation failed (exit %s). Bounded diagnostic tails:\n' "$status" >&2
  for log in "$peer_root/logs/configure.log" "$peer_root/source/gearmand-2.1.0/config.log" "$peer_root/logs/build.log"; do
    if [[ -f $log ]]; then
      printf '\n%s\n' "$log" >&2
      tail -n 80 "$log" | cut -c 1-2048 >&2
    fi
  done
  exit "$status"
}
trap report_failure ERR
archive="$peer_root/downloads/gearmand-2.1.0.tar.gz"
source_root="$peer_root/source/gearmand-2.1.0"
expected_sha=4d24340ab39be851b40d895687c17d6d16e730ece1fa9d9294d6b2b0a8cb1261
if [[ ! -f $archive ]]; then
  # TLS verification stays enabled; an HTTP downgrade or alternate archive is refused.
  curl --proto '=https' --proto-redir '=https' --tlsv1.2 --fail --location --retry 2 \
    --connect-timeout 15 --max-time 120 \
    https://github.com/gearman/gearmand/releases/download/2.1.0/gearmand-2.1.0.tar.gz \
    --output "$archive.part"
  printf '%s  %s\n' "$expected_sha" "$archive.part" | sha256sum --check --status
  mv "$archive.part" "$archive"
fi
printf '%s  %s\n' "$expected_sha" "$archive" | sha256sum --check --status
tar -xzf "$archive" -C "$peer_root/source"
cd "$source_root"
# Disable optional persistent backends and TLS: the fixtures test plain Gearman TCP.
# Two compiler jobs, debug information disabled, and whole configure/build deadlines.
timeout --kill-after=15s 5m env CFLAGS='-O0 -g0' CXXFLAGS='-O0 -g0 -std=c++17' \
  ./configure --prefix="$peer_root/installed" --disable-shared --enable-static \
  --disable-ssl \
  --disable-dependency-tracking --disable-libdrizzle --disable-libpq \
  --disable-hiredis --disable-libmemcached --without-mysql --without-sqlite3 \
  >"$peer_root/logs/configure.log" 2>&1
timeout --kill-after=15s 10m make -j2 gearmand/gearmand bin/gearman bin/gearadmin \
  >"$peer_root/logs/build.log" 2>&1
install -m 755 gearmand/gearmand "$peer_root/bin/gearmand"
install -m 755 bin/gearman "$peer_root/bin/gearman"
install -m 755 bin/gearadmin "$peer_root/bin/gearadmin"
"$peer_root/bin/gearmand" --version | tee "$peer_root/logs/version.log"
grep -Eq 'gearmand 2\.1\.0([[:space:]]|$)' "$peer_root/logs/version.log"
printf 'Gearman peers installed into %s/bin; prepend it to PATH for the suite.\n' "$peer_root"
