#!/usr/bin/env bash
# Cancel only builds registered by this caller session, irrespective of shared target paths.
# Usage: ./cargo-isolated-kill.sh [--yes | --list]
# CARGO_SESSION_PID selects the same live shell used by cargo.sh/cargo-isolated.sh.
set -euo pipefail
PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SESSION_PID="${CARGO_SESSION_PID:-$PPID}"
exec python3 "${PROJECT_ROOT}/scripts/cargo_session.py" cancel \
  --root "$PROJECT_ROOT" --session-pid "$SESSION_PID" "$@"
