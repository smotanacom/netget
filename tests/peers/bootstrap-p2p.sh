#!/usr/bin/env bash
set -euo pipefail
# External test peers only. System prerequisites are installed by protocol-pairs CI.
base=${1:?Usage: bootstrap-p2p.sh /absolute/test-peer-directory}
python_bin=${NETGET_PEER_BOOTSTRAP_PYTHON:-python3.10}
make_bin=make
if [ "$(uname -s)" = Darwin ] && command -v gmake >/dev/null; then make_bin=gmake; fi
mkdir -p "$base"
"$python_bin" -m venv "$base/venv"
"$base/venv/bin/pip" install 'aioslsk==1.6.4' 'pexpect==4.9.0' 'nntpserver==0.0.3'
fetch() {
    local url=$1 archive=$2 checksum=$3
    curl --fail --location --retry 3 "$url" -o "$archive"
    printf '%s  %s\n' "$checksum" "$archive" | shasum -a 256 --check
}
fetch https://dev.yorhel.nl/download/ncdc-1.25.tar.gz "$base/ncdc-1.25.tar.gz" b9be58e7dbe677f2ac1c472f6e76fad618a65e2f8bf1c7b9d3d97bc169feb740
tar -xzf "$base/ncdc-1.25.tar.gz" -C "$base"
(cd "$base/ncdc-1.25" && ./configure && "$make_bin" -j4)
if [ ! -d "$base/uhub/.git" ]; then git clone --branch 0.8.0 --recurse-submodules https://github.com/janvidar/uhub.git "$base/uhub"; fi
test "$(git -C "$base/uhub" rev-parse HEAD)" = bad5a500758d7dbafd98a20b4c7c4175531f83c9
test "$(git -C "$base/uhub/third_party/exotic" rev-parse HEAD)" = edcf3dcf91c9d4de66329fd03669e1b9b0c5183d
test "$(git -C "$base/uhub/third_party/quickjs" rev-parse HEAD)" = fd0a0210b7be00957751871e7e01b8291268fc29
cmake -S "$base/uhub" -B "$base/uhub-build"
cmake --build "$base/uhub-build" --parallel 4
fetch https://github.com/gtk-gnutella/gtk-gnutella/archive/refs/tags/v1.3.1.tar.gz "$base/gtk-gnutella-1.3.1.tar.gz" 8dbf3a0483499c8db381d888169490ddf57b4c575bc0c3df7145b88a5a8f1607
tar -xzf "$base/gtk-gnutella-1.3.1.tar.gz" -C "$base"
(
    cd "$base/gtk-gnutella-1.3.1"
    extra=()
    if [ "$(uname -s)" = Darwin ]; then extra+=(--cflags=-Wno-error=incompatible-function-pointer-types); fi
    ./build.sh --topless --disable-malloc --disable-gnutls --disable-dbus --disable-nls --configure-only "${extra[@]}"
    # Older mkdep treats Clang's pseudo-source line directives as physical files.
    "$python_bin" - <<'PY'
from pathlib import Path
import sys
if sys.platform == 'darwin':
    p = Path('src/main.c')
    # Optional benchmark can deadlock in upstream tqsort/thread_join on macOS 27 ARM64.
    # vsort explicitly supplies working defaults when initialization is omitted.
    p.write_text(p.read_text().replace('vsort_init(isatty(STDERR_FILENO) ? 0 : 1);',
                                       '/* Test fixture: use the default sort routines on macOS. */'))
for p in Path('.').rglob('Makefile'):
    p.write_text('\n'.join(line for line in p.read_text().split('\n')
                         if not ('.o: <' in line or '.o: y.tab.c' in line)))
PY
    "$make_bin" -j4
)
printf 'export NETGET_P2P_PYTHON=%q\n' "$base/venv/bin/python"
printf 'export NETGET_NCDC=%q\n' "$base/ncdc-1.25/ncdc"
printf 'export NETGET_UHUB=%q\n' "$base/uhub-build/uhub"
printf 'export NETGET_GNUTELLA=%q\n' "$base/gtk-gnutella-1.3.1/src/gtk-gnutella"
