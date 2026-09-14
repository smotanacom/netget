#!/usr/bin/env bash
# Build the browser demo: crates/netget-web -> docs/demo/pkg/.
#
#   ./web/build.sh            size-tuned build (the `web` profile), bindings into docs/demo/pkg
#   ./web/build.sh --dev      debug build (faster, much larger .wasm)
#
# Needs: the wasm32-unknown-unknown target (`rustup target add wasm32-unknown-unknown`),
# `wasm-bindgen` at the exact version Cargo.lock pins (`cargo install wasm-bindgen-cli
# --version <that>`), and an archiver that understands wasm objects for ring's C code. On
# macOS the system `ar`/`ranlib` silently write a broken archive ("not a mach-o file"), which
# surfaces as `undefined symbol: ring_core_...` at link time; `rustup component add
# llvm-tools` provides `llvm-ar` and this script points cargo at it.
set -euo pipefail
cd "$(dirname "$0")/.."

profile=web
profile_dir=web
wasm_ar_env=()
if [[ "${1:-}" == "--dev" ]]; then
    profile=dev
    profile_dir=debug
fi

# Prefer the rustup toolchain's cargo: a Homebrew cargo on PATH has no wasm target.
if [[ -x "$HOME/.cargo/bin/cargo" ]]; then
    export PATH="$HOME/.cargo/bin:$PATH"
fi

if ! command -v llvm-ar >/dev/null 2>&1; then
    sysroot="$(rustc --print sysroot)"
    host="$(rustc -vV | sed -n 's/^host: //p')"
    if [[ -x "$sysroot/lib/rustlib/$host/bin/llvm-ar" ]]; then
        export PATH="$sysroot/lib/rustlib/$host/bin:$PATH"
    fi
fi
if command -v llvm-ar >/dev/null 2>&1; then
    # Both spellings reach the `cc` crate; ring's build script re-runs only when the
    # hyphenated one changes, and a hyphen is not a shell identifier, hence `env` below.
    export AR_wasm32_unknown_unknown="$(command -v llvm-ar)"
    wasm_ar_env=("AR_wasm32-unknown-unknown=$(command -v llvm-ar)")
else
    echo "warning: no llvm-ar on PATH; on macOS the link will fail with undefined ring_core symbols." >&2
    echo "         rustup component add llvm-tools" >&2
fi

locked="$(grep -A1 '^name = "wasm-bindgen"$' Cargo.lock | sed -n 's/^version = "\(.*\)"/\1/p')"
if ! command -v wasm-bindgen >/dev/null 2>&1; then
    echo "error: wasm-bindgen is not installed; run: cargo install wasm-bindgen-cli --version $locked" >&2
    exit 1
fi
have="$(wasm-bindgen --version | awk '{print $2}')"
if [[ "$have" != "$locked" ]]; then
    echo "error: wasm-bindgen $have is installed but Cargo.lock pins $locked; run: cargo install wasm-bindgen-cli --version $locked" >&2
    exit 1
fi

echo "== cargo build (profile: $profile)"
env "${wasm_ar_env[@]}" cargo build -p netget-web --target wasm32-unknown-unknown --profile "$profile"

out=docs/demo/pkg
mkdir -p "$out"
echo "== wasm-bindgen -> $out"
wasm-bindgen --target web --no-typescript --out-dir "$out" \
    "target/wasm32-unknown-unknown/$profile_dir/netget_web.wasm"

if command -v wasm-opt >/dev/null 2>&1 && [[ "$profile" == "web" ]]; then
    echo "== wasm-opt -Oz"
    wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int \
        -o "$out/netget_web_bg.wasm" "$out/netget_web_bg.wasm"
fi

ls -la "$out"
