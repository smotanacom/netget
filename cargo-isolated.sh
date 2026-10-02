#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [[ $# -eq 0 ]]; then
  echo "Usage: ./cargo-isolated.sh <cargo-args>" >&2
  echo "       ./cargo-isolated.sh --print-last  # Print the most recent log file" >&2
  echo "       ./cargo-isolated.sh --print       # Same as --print-last" >&2
  echo "       ./cargo-isolated.sh --tail-last   # Tail the most recent log file" >&2
  echo "       ./cargo-isolated.sh --tail        # Same as --tail-last" >&2
  exit 1
fi

# Helper function to get the most recent log file
get_latest_log() {
  TMP_DIR="${SCRIPT_DIR}/tmp"
  if [[ ! -d "$TMP_DIR" ]]; then
    echo "Error: No tmp/ directory found. Run ./cargo-isolated.sh first." >&2
    exit 1
  fi
  LOG_FILE=$(ls -t "${TMP_DIR}/netget-"*.log 2>/dev/null | head -n 1)
  if [[ -z "${LOG_FILE:-}" ]]; then
    echo "Error: No log files found" >&2
    exit 1
  fi
  echo "$LOG_FILE"
}

# Handle --print-last and --print (print entire log)
if [[ "${1:-}" == "--print-last" ]] || [[ "${1:-}" == "--print" ]]; then
  LOG_FILE=$(get_latest_log)
  echo "Reading log: $LOG_FILE" >&2
  echo "============================" >&2
  cat "$LOG_FILE"
  exit 0
fi

# Handle --tail-last and --tail (tail -f the log)
if [[ "${1:-}" == "--tail-last" ]] || [[ "${1:-}" == "--tail" ]]; then
  LOG_FILE=$(get_latest_log)
  echo "Tailing log: $LOG_FILE" >&2
  echo "============================" >&2
  tail -f "$LOG_FILE"
  exit 0
fi

# Use standard target/ directory (shared across all sessions)
TARGET_DIR="${SCRIPT_DIR}/target"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$TARGET_DIR}"

# Ensure sccache is used and consistent
export RUSTC_WRAPPER="${RUSTC_WRAPPER-sccache}"
export SCCACHE_CACHE_SIZE="${SCCACHE_CACHE_SIZE:-30G}"

# Disable incremental; env overrides profiles
export CARGO_INCREMENTAL=0                               # global off for better caching

# Stabilize paths in debuginfo and diagnostics
PROJECT_ROOT="$(pwd)"
ADD_REMAPS=(
  "--remap-path-prefix=${PROJECT_ROOT}=/proj"
  "--remap-path-prefix=${TARGET_DIR}=/tgt"
)
# Use CARGO_ENCODED_RUSTFLAGS to avoid word-splitting issues
ENCODED="${CARGO_ENCODED_RUSTFLAGS:-}"
for ((i=0; i<${#ADD_REMAPS[@]}; i++)); do
  if [[ -n "$ENCODED" ]]; then ENCODED+=$'\x1f'; fi
  ENCODED+="${ADD_REMAPS[$i]}"
done
export CARGO_ENCODED_RUSTFLAGS="$ENCODED"               # recommended for multi-flag injection

# Optional: strip debuginfo in dev to further stabilize keys
# export CARGO_PROFILE_DEV_DEBUG=0                       # fewer path embeddings

# Supervision records the calling session; target directories remain shared.
export CARGO_SESSION_PID="${CARGO_SESSION_PID:-$PPID}"

# Use shared target directory (no longer isolated per-session)
CARGO_USE_ISOLATION=false exec "${SCRIPT_DIR}/cargo.sh" "$@"
