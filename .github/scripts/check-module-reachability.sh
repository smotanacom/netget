#!/usr/bin/env bash
#
# Fail if any repository .rs file is unreachable from every cargo target root, and keep the
# stragglers formatted.
#
# Why this exists
# ---------------
# `tests/server.rs` compiles only the modules `tests/server/mod.rs` declares, and each of those
# directories compiles only what *its* mod.rs declares. A file nobody declares is not an error
# and not a warning — it simply does not exist as far as rustc, clippy and rustfmt are
# concerned. The `orphaned-tests` job caught the directory case; it could not see a file inside
# a declared directory, nor a test tree outside tests/server and tests/client. That gap hid 29
# test functions, including the only proxy suite that actually routes traffic.
#
# It is also the true cause of unformatted code in the tree. `cargo fmt` has no feature flags
# and does not evaluate `cfg`, so cfg-gated modules ARE visited; only undeclared ones are not.
#
# How it works
# ------------
# `cargo fmt -- --emit stdout` prints a `<path>:` header for every file rustfmt parses, walking
# the same module graph rustc does, from every target root in cargo metadata. That set is the
# ground truth for "reachable". Diff it against `git ls-files`.
#
# Usage: .github/scripts/check-module-reachability.sh
# Run from the repository root. Needs `cargo`, `rustfmt` and `git`; compiles nothing.

set -uo pipefail

ALLOWLIST_FILE=".github/unreachable-modules.txt"
EDITION="$(sed -n 's/^edition *= *"\([0-9]*\)".*/\1/p' Cargo.toml | head -n1)"
EDITION="${EDITION:-2021}"

# GitHub Actions annotations when running in CI, plain text otherwise.
if [ -n "${GITHUB_ACTIONS:-}" ]; then
  err() { echo "::error::$*"; }
  warn() { echo "::warning::$*"; }
else
  err() { echo "ERROR: $*" >&2; }
  warn() { echo "WARNING: $*" >&2; }
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

repo_root="$(pwd -P)"

# ---------------------------------------------------------------------------------------
# 1. The set rustfmt reaches.
# ---------------------------------------------------------------------------------------
if ! cargo fmt -- --emit stdout > "$tmp/emit.txt" 2> "$tmp/emit.err"; then
  err "cargo fmt failed; module reachability cannot be determined."
  cat "$tmp/emit.err" >&2
  exit 1
fi

grep -oE "^${repo_root}/[A-Za-z0-9_/.-]+\.rs:$" "$tmp/emit.txt" \
  | sed "s|^${repo_root}/||; s|:$||" \
  | sort -u > "$tmp/reachable.txt"

if [ ! -s "$tmp/reachable.txt" ]; then
  err "Could not extract any reachable file from 'cargo fmt --emit stdout'. The output format \
probably changed; this check is broken, not the tree."
  exit 1
fi

# ---------------------------------------------------------------------------------------
# 2. The set on disk.
#
# Restricted to src/ and tests/ — the crate's own code. Nested example crates
# (examples/*/src/**) are separate cargo packages, outside this workspace's metadata, and are
# covered by the rustfmt sweep in step 4 instead.
# ---------------------------------------------------------------------------------------
# Include new source files and exclude working-tree deletions before they are staged.
# This makes the local check agree with the eventual committed module graph.
git ls-files --cached --others --exclude-standard '*.rs' | grep -E '^(src|tests)/' \
  | while IFS= read -r source; do [ ! -f "$source" ] || printf '%s\n' "$source"; done \
  | sort -u > "$tmp/ondisk.txt"

# ---------------------------------------------------------------------------------------
# 3. Allowlist, and unreachable files that are not on it.
# ---------------------------------------------------------------------------------------
if [ -f "$ALLOWLIST_FILE" ]; then
  sed 's/#.*//' "$ALLOWLIST_FILE" | tr -d '\r' | awk 'NF' | sort -u > "$tmp/allow.txt"
else
  : > "$tmp/allow.txt"
fi

comm -23 "$tmp/ondisk.txt" "$tmp/reachable.txt" > "$tmp/unreachable.txt"
comm -23 "$tmp/unreachable.txt" "$tmp/allow.txt" > "$tmp/offenders.txt"

status=0

if [ -s "$tmp/offenders.txt" ]; then
  err "Unreachable Rust file(s): present in the working tree but declared by no 'mod' statement, so cargo \
never compiles them. Tests inside them never run; lints and rustfmt never see them."
  sed 's/^/    /' "$tmp/offenders.txt"
  echo
  echo "Fix each one:"
  echo "  * a file beside a mod.rs        -> add 'mod <name>;' (feature-gated as its siblings are)"
  echo "  * a whole directory under tests/ -> add a 'tests/<name>.rs' root that #[path]s it in"
  echo "  * genuinely dead                 -> delete it, or add it to $ALLOWLIST_FILE with a reason"
  echo
  status=1
fi

# A stale entry is bookkeeping, not breakage: warn so reviving a module never reddens master.
while IFS= read -r entry; do
  [ -n "$entry" ] || continue
  if ! grep -qxF "$entry" "$tmp/unreachable.txt"; then
    if [ -e "$entry" ]; then
      warn "Stale allowlist entry: '$entry' is reachable again. Remove it from $ALLOWLIST_FILE."
    else
      warn "Stale allowlist entry: '$entry' no longer exists. Remove it from $ALLOWLIST_FILE."
    fi
  fi
done < "$tmp/allow.txt"

# ---------------------------------------------------------------------------------------
# 4. rustfmt over every existing tracked/new file, allowlist excluded.
#
# `cargo fmt --check` covers the reachable set. This covers the working tree,
# including targets cargo metadata does not enumerate (nested example crates) — so a file can
# never hide from formatting by being undeclared, only by being explicitly declared dead.
# ---------------------------------------------------------------------------------------
git ls-files --cached --others --exclude-standard '*.rs' \
  | while IFS= read -r source; do [ ! -f "$source" ] || printf '%s\n' "$source"; done \
  | sort -u > "$tmp/tracked.txt"
comm -23 "$tmp/tracked.txt" "$tmp/allow.txt" > "$tmp/to_fmt.txt"

if [ -s "$tmp/to_fmt.txt" ]; then
  # `xargs < file` rather than `xargs -a file`: BSD xargs (macOS) has no -a.
  if ! xargs rustfmt --edition "$EDITION" --check < "$tmp/to_fmt.txt" > "$tmp/fmt.txt" 2>&1; then
    err "rustfmt --check failed on working-tree file(s) (run 'cargo fmt', then rustfmt any file \
listed below by hand):"
    grep -oE '^Diff in [^:]+' "$tmp/fmt.txt" | sed "s|Diff in ${repo_root}/||" | sort -u \
      | sed 's/^/    /'
    status=1
  fi
fi

if [ "$status" = "0" ]; then
  echo "OK: $(wc -l < "$tmp/ondisk.txt" | tr -d ' ') files under src/ and tests/ are all reachable \
($(wc -l < "$tmp/allow.txt" | tr -d ' ') allowlisted), and every working-tree .rs file is formatted."
fi

exit "$status"
