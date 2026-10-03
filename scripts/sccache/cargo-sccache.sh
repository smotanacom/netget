#!/bin/bash
# Wrapper for cargo-isolated.sh with sccache support
# This script wraps cargo-isolated.sh and enables sccache for faster builds

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Check if sccache is available
if ! command -v sccache &> /dev/null; then
    echo "Warning: sccache not found. Install with: cargo install sccache"
    echo "Falling back to cargo-isolated.sh without sccache..."
    RUSTC_WRAPPER= exec "$PROJECT_ROOT/cargo-isolated.sh" "$@"
fi

# Check if sccache is configured (either local or remote)
if [ -f "$HOME/.sccache-r2-env" ]; then
    echo "Loading Cloudflare R2 sccache configuration..."
    source "$HOME/.sccache-r2-env"
elif [ -f "$HOME/.sccache-upstash-env" ]; then
    echo "Loading Upstash Redis sccache configuration..."
    source "$HOME/.sccache-upstash-env"
elif [ -n "$SCCACHE_REDIS_ENDPOINT" ] || [ -n "$SCCACHE_BUCKET" ]; then
    echo "Using existing sccache environment configuration"
else
    echo "No remote sccache configured, using local disk cache"
fi

# Enable sccache
export RUSTC_WRAPPER=sccache

# Show cache status before build
echo "sccache status:"
sccache --show-stats | grep -E "(Cache location|Cache hits|Cache misses)" | head -3
echo

# Run cargo-isolated.sh with all arguments
echo "Running cargo with sccache enabled..."
"$PROJECT_ROOT/cargo-isolated.sh" "$@"

# Show cache statistics after build
echo
echo "=== sccache Statistics After Build ==="
sccache --show-stats | grep -E "(Compile requests|Cache hits|Cache misses|Cache hits rate)"
echo
echo "Tip: Run 'sccache --show-stats' anytime to see cache performance"
