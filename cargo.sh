#!/bin/bash
# Cargo wrapper with shared-target caching and session-owned process supervision
# Usage: ./cargo.sh <cargo-args>
# Example: ./cargo.sh build --release --all-features
#
# Modes:
# - Standard mode (default): Uses target/ directory
# - Isolated mode: Set CARGO_USE_ISOLATION=true to use target-claude/claude-{PPID}/
#
# Options:
# - --skip-cleanup: Skip cleanup of old target directories (faster startup)

set -e

# Parse flags
SKIP_CLEANUP=false
CARGO_ARGS=()
for arg in "$@"; do
    if [[ "$arg" == "--skip-cleanup" ]]; then
        SKIP_CLEANUP=true
    else
        CARGO_ARGS+=("$arg")
    fi
done

# Determine project root (where Cargo.toml lives)
PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Set default values if not provided
CARGO_USE_ISOLATION="${CARGO_USE_ISOLATION:-false}"
CARGO_CLEANUP_OLD="${CARGO_CLEANUP_OLD:-true}"

# Override cleanup if --skip-cleanup flag is passed
if [[ "$SKIP_CLEANUP" == true ]]; then
    CARGO_CLEANUP_OLD=false
fi

# Cleanup old target directories from dead sessions (only for isolated mode)
if [[ "$CARGO_USE_ISOLATION" == true ]] && [[ "$CARGO_CLEANUP_OLD" == true ]] && [[ -d "${PROJECT_ROOT}/target-claude" ]]; then
    CLEANED=0
    for dir in "${PROJECT_ROOT}/target-claude"/claude-*; do
        # Skip if no directories match
        [[ -d "$dir" ]] || continue

        # Extract PID from directory name (e.g., claude-12345 -> 12345)
        PID=$(basename "$dir" | sed 's/^claude-//')

        # Check if PID is still running
        if ! ps -p "$PID" > /dev/null 2>&1; then
            echo "Cleaning up old session: $dir (PID $PID no longer active)" >&2
            rm -rf "$dir"
            CLEANED=$((CLEANED + 1))
        fi
    done

    if [[ $CLEANED -gt 0 ]]; then
        echo "Cleaned up $CLEANED old session(s)" >&2
    fi
fi

# Set target directory based on isolation mode
if [[ "$CARGO_USE_ISOLATION" == true ]]; then
    # Legacy isolated mode - no longer used
    # DEPRECATED: Now using shared target/ directory for better cache sharing
    echo "Warning: CARGO_USE_ISOLATION=true is deprecated. Using shared target/ directory." >&2
fi

# Use standard target directory (don't override CARGO_TARGET_DIR if already set)
if [[ -z "$CARGO_TARGET_DIR" ]]; then
    export CARGO_TARGET_DIR="${PROJECT_ROOT}/target"
fi
BUILD_MODE="Shared"

# Check for sccache
if [[ "$RUSTC_WRAPPER" == "sccache" ]]; then
    # RUSTC_WRAPPER is set to sccache - verify it's installed
    if ! command -v sccache &> /dev/null; then
        echo "⚠️  RUSTC_WRAPPER is set to sccache, but sccache is not installed" >&2
        echo "Installing sccache automatically..." >&2
        echo "" >&2

        # Install sccache (temporarily disable RUSTC_WRAPPER to avoid circular dependency)
        if (unset RUSTC_WRAPPER && cargo install sccache); then
            echo "" >&2
            echo "✓ sccache installed successfully" >&2
            echo "" >&2
        else
            echo "" >&2
            echo "❌ Failed to install sccache" >&2
            echo "Build may fail. To fix manually:" >&2
            echo "  unset RUSTC_WRAPPER && cargo install sccache" >&2
            echo "Or disable sccache for this session:" >&2
            echo "  unset RUSTC_WRAPPER" >&2
            echo "" >&2
            exit 1
        fi
    fi
elif [[ -z "$RUSTC_WRAPPER" || "$RUSTC_WRAPPER" != "sccache" ]]; then
    # sccache not configured - show optional warning
    echo "⚠️  WARNING: RUSTC_WRAPPER is not set to sccache" >&2
    echo "" >&2
    echo "To speed up builds, install sccache:" >&2
    echo "  cargo install sccache" >&2
    echo "" >&2
    echo "Then add to your ~/.bashrc, ~/.zshrc, or shell config:" >&2
    echo "  export RUSTC_WRAPPER=sccache" >&2
    echo "  export SCCACHE_CACHE_SIZE=50G" >&2
    echo "" >&2
    echo "Or set it for this session:" >&2
    echo "  export RUSTC_WRAPPER=sccache" >&2
    echo "  export SCCACHE_CACHE_SIZE=50G" >&2
    echo "" >&2
fi

# Create tmp directory for logs
mkdir -p "${PROJECT_ROOT}/tmp"

# Clean up log files older than 1 day
find "${PROJECT_ROOT}/tmp" -name "netget-*.log" -type f -mtime +1 -delete 2>/dev/null || true

# Each invocation gets a separate log, even for simultaneous builds in one session.
COMMAND="${CARGO_ARGS[0]:-unknown}"
COMMAND="${COMMAND//[^[:alnum:]_-]/_}"
SESSION_PID="${CARGO_SESSION_PID:-$PPID}"
export CARGO_SESSION_PID="$SESSION_PID"
LOG_FILE="${PROJECT_ROOT}/tmp/netget-${COMMAND}-${SESSION_PID}-$$.log"

# Echo the target directory info for visibility
echo "=== Cargo $BUILD_MODE Build ===" >&2
echo "Target directory: $CARGO_TARGET_DIR" >&2
echo "Command: cargo ${CARGO_ARGS[*]}" >&2
echo "Log file: $LOG_FILE" >&2
echo "============================" >&2

# Enable pipefail to capture cargo's exit code through the pipe
set -o pipefail

# Run cargo and tee output to log file (captures both stdout and stderr)
python3 "${PROJECT_ROOT}/scripts/cargo_session.py" run \
    --root "$PROJECT_ROOT" --session-pid "$SESSION_PID" -- cargo "${CARGO_ARGS[@]}" 2>&1 | tee "$LOG_FILE"
CARGO_EXIT=$?

# Run cargo-sweep in background to keep target/ under 15GB (non-blocking)
# NOTE: Using --maxsize instead of --time because sccache doesn't update file
# modification times when serving cached artifacts, causing --time to incorrectly
# delete recently-used artifacts.
if command -v cargo-sweep &> /dev/null; then
    (cargo sweep --maxsize 15 "$CARGO_TARGET_DIR" &> /dev/null &)
fi

# Exit with cargo's exit code
exit $CARGO_EXIT
