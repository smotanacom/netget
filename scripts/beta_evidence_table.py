#!/usr/bin/env python3
"""Generate the Beta-evidence table from the source tree.

CLAUDE.md carries a hand-maintained list of which protocols are Beta and what their
evidence is. That list has been wrong three times, in both directions: it under-rated
`etcd` and `modbus` while they already had official-client tests, and it over-rated
`ssh` on the strength of `russh` — which is the server's *own* library, so the "peer"
was the server. A generated table cannot drift.

For every registered server protocol this prints:

  state          the DevelopmentState its own metadata() declares
  peer           the third-party binaries and crates its tests actually drive
  circular       any of those crates that its own src/server/<p>/ also imports, which
                 means the peer is the same implementation as the framer and the
                 evidence proves only that a crate agrees with itself
  optional       any of those crates declared `optional = true` in Cargo.toml, which
                 means the dependency is not compiled where the gate runs
  ignored        how many of its tests are #[ignore]d
  skip-gate      whether a test prints a skip message and returns success anyway

The last four are the three ways CLAUDE.md says evidence fails to execute, plus the
circular case. `--check` exits non-zero only on what a script can be *sure* of — no
independent peer, a skip-and-pass gate, or every test `#[ignore]`d. Circular and
optional dependencies are printed as review flags instead, because both have accepted
exceptions (`quic`/quinn and `webrtc`/webrtc-rs use the server's own crate in the
opposite role, completing a real handshake) and no static rule can tell those from the
`ssh`/russh case.

Usage:
    python3 scripts/beta_evidence_table.py              # Beta only, markdown
    python3 scripts/beta_evidence_table.py --all        # every protocol
    python3 scripts/beta_evidence_table.py --check      # exit 1 on a defective Beta
    python3 scripts/beta_evidence_table.py --experimental-with-evidence
                                                        # promotion candidates
"""

from __future__ import annotations

import argparse
import re
import shutil
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# Crates that are infrastructure rather than a protocol peer: importing them says
# nothing about who validated the wire format.
INFRASTRUCTURE = {
    "std", "core", "alloc", "super", "crate", "self",
    "tokio", "tokio_util", "tokio_test", "futures", "futures_util", "async_trait",
    "serde", "serde_json", "anyhow", "thiserror", "tracing", "log",
    "tempfile", "rand", "once_cell", "lazy_static", "parking_lot", "itertools",
    "bytes", "byteorder", "chrono", "time", "uuid", "regex", "url", "libc", "nix",
    "ctor", "hex", "base64", "sha1", "sha2", "md5", "md_5", "urlencoding",
    "netget", "helpers", "mod", "axum", "axum_server", "http", "http_body_util",
}

# A generic HTTP client proves an HTTP server answers, not that the protocol layered
# ON TOP of it is right. CLAUDE.md rules that class out of Beta explicitly (couchdb,
# openapi, spark, yarn, …).
#
# It is not ruled out for a protocol that *is* HTTP, and getting that wrong is easy:
# an earlier version of this script reported `http` itself as having no peer. Which
# case a protocol is in is derivable rather than a judgement — `stack_name()` spells
# the layering out, so "…>HTTP" is HTTP and "…>HTTP>Maven" is something on top of it.
GENERIC_HTTP = {"reqwest", "hyper", "hyper_util", "ureq", "isahc"}

# Binaries that are shells, build tools or checksum utilities rather than a protocol
# client. `shasum` is an oracle for a digest, not a peer that speaks the protocol.
NOT_A_PEER_BINARY = {
    "sh", "bash", "zsh", "which", "echo", "cat", "true", "false", "sleep", "env",
    "shasum", "sha256sum", "sha1sum", "md5sum", "python3", "python", "node", "protoc",
    "cargo", "rustc", "unzip", "zip", "tar", "gzip", "cp", "mv", "rm", "mkdir",
}

SKIP_MESSAGE = re.compile(
    r"""(?ix)
    (?:e?println!|warn!|info!|eprint!)\s*\(\s*
    (?:[^;]{0,400}?)
    (?:\bSKIP|\bskipping\b|\bskipped\b|not\ installed|not\ found|not\ available)
    """
)
RETURN_OK = re.compile(r"return\s+Ok\(\(\)\)\s*;|^\s*return\s*;", re.M)

# A few crates spell their library differently from their Cargo key, so a `use`
# statement does not name the dependency. Keep this list short and obvious.
LIB_NAME_ALIASES = {
    "rust_s3": "s3",
}


def declared_states() -> dict[str, str]:
    """Every server protocol's own DevelopmentState, keyed by its source directory."""
    pattern = re.compile(
        r"\.state\(\s*(?:crate::protocol::metadata::)?DevelopmentState::([A-Za-z]+)"
    )
    out: dict[str, str] = {}
    base = ROOT / "src" / "server"
    for actions in sorted(list(base.glob("*/actions.rs")) + list(base.glob("*/*/actions.rs"))):
        rel = actions.relative_to(base).parent.as_posix()
        m = pattern.search(actions.read_text(errors="ignore"))
        if m:
            out[rel] = m.group(1)
    return out


def http_native(protocol: str) -> bool:
    """True when the protocol's own stack ends at HTTP, so an HTTP client is its client.

    `ETH>IP>TCP>HTTP` is HTTP; `ETH>IP>TCP>HTTP>Maven` is Maven carried over HTTP, and
    for that one a generic HTTP client proves the transport and nothing above it.
    """
    actions = ROOT / "src" / "server" / protocol / "actions.rs"
    if not actions.is_file():
        return False
    m = re.search(
        r'fn\s+stack_name\s*\([^)]*\)\s*->\s*&\'static\s+str\s*\{[^}]*?"([^"]+)"',
        actions.read_text(errors="ignore"),
        re.S,
    )
    if not m:
        return False
    return m.group(1).split(">")[-1].upper() in {"HTTP", "HTTPS", "HTTP/1.1", "HTTP2"}


def cargo_dependencies() -> dict[str, bool]:
    """Every declared dependency, mapped to whether it is `optional = true`.

    Keys are normalised to the crate's Rust identifier (`-` becomes `_`), which is
    how a `use` statement spells it.
    """
    text = (ROOT / "Cargo.toml").read_text(errors="ignore")
    deps: dict[str, bool] = {}
    section = None
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("["):
            section = stripped.strip("[]")
            continue
        if section is None or "dependencies" not in section:
            continue
        m = re.match(r'^([A-Za-z0-9_-]+)\s*=\s*(.*)$', stripped)
        if not m:
            continue
        name, rest = m.group(1), m.group(2)
        optional = "optional = true" in rest or "optional=true" in rest
        key = name.replace("-", "_")
        deps[key] = optional
        # `foo = { package = "bar" }` means `use bar::` is what the code writes.
        pkg = re.search(r'package\s*=\s*"([^"]+)"', rest)
        if pkg:
            deps[pkg.group(1).replace("-", "_")] = optional
        if key in LIB_NAME_ALIASES:
            deps[LIB_NAME_ALIASES[key]] = optional
    return deps


def imported_crates(directory: Path, known: set[str]) -> set[str]:
    """Third-party crates a directory reaches for.

    Both spellings matter. `use async_imap::…` is the usual one, but a test may also
    write `async_nats::connect(…)` inline with no `use` at all — `tests/server/nats`
    does, and an earlier version of this script reported NATS as having no peer
    because of it.

    The inline form is matched only against names that really are Cargo dependencies.
    Matching every lowercase `x::` instead reported `u16`, `mpsc`, `str` and `collect`
    as third-party peers, which is worse than missing one: a table full of noise gets
    ignored, and an ignored table drifts exactly like the hand-maintained list it
    replaces.
    """
    if not directory.is_dir():
        return set()
    found: set[str] = set()
    use_form = re.compile(r"^\s*use\s+([a-z][a-z0-9_]*)\s*::", re.M)
    inline_form = re.compile(r"(?<![A-Za-z0-9_:])([a-z][a-z0-9_]*)\s*::")
    for f in directory.rglob("*.rs"):
        code = "\n".join(line.split("//")[0] for line in f.read_text(errors="ignore").splitlines())
        for m in use_form.finditer(code):
            crate = m.group(1)
            if crate not in INFRASTRUCTURE and crate in known:
                found.add(crate)
        for m in inline_form.finditer(code):
            crate = m.group(1)
            if crate not in INFRASTRUCTURE and crate in known:
                found.add(crate)
    return found


def test_directory(protocol: str) -> Path:
    base = ROOT / "tests" / "server"
    for candidate in (protocol.replace("/", "_"), protocol, protocol.split("/")[-1]):
        path = base / candidate
        if path.is_dir():
            return path
    return base / protocol.replace("/", "_")


def scan_tests(directory: Path) -> dict:
    """Binaries driven, #[ignore] counts, and any skip-and-pass gate."""
    binaries: set[str] = set()
    ignored: list[str] = []
    total = 0
    skip_gates: list[str] = []
    if not directory.is_dir():
        return {"binaries": binaries, "ignored": ignored, "tests": 0, "skip_gates": skip_gates}

    for f in sorted(directory.rglob("*.rs")):
        text = f.read_text(errors="ignore")
        code = "\n".join(line.split("//")[0] for line in text.splitlines())

        # `Command::new("dig")` is the easy case. `Command::new(&binary)`, where the
        # path was resolved earlier, is just as common — `radius` and `memcached` both
        # do it, and an earlier version of this script reported both as having no peer.
        # So the absolute paths and `tool("name")` lookups those helpers use count too.
        for m in re.finditer(r'Command::new\(\s*"([^"]+)"', code):
            name = m.group(1).split("/")[-1]
            if name not in NOT_A_PEER_BINARY:
                binaries.add(name)
        if re.search(r"Command::new\(\s*&?[a-z_]", code):
            for m in re.finditer(r'"(?:/opt/homebrew/bin|/usr/local/bin|/usr/bin|/bin|/sbin|/usr/sbin)/([a-z0-9_.-]+)"', code):
                if m.group(1) not in NOT_A_PEER_BINARY:
                    binaries.add(m.group(1))
            for m in re.finditer(r'(?:tool|which_in_path|find_binary|require_tool)\(\s*"([a-z0-9_.-]+)"', code):
                if m.group(1) not in NOT_A_PEER_BINARY:
                    binaries.add(m.group(1))

        total += len(re.findall(r"#\[(?:tokio::)?test", code))
        for m in re.finditer(r"#\[ignore[^\]]*\]", code):
            ignored.append(f"{f.name}:{code[:m.start()].count(chr(10)) + 1}")

        lines = text.splitlines()
        for i, line in enumerate(lines):
            if "//" in line and line.strip().startswith("//"):
                continue  # a comment describing a gate is not a gate
            if not SKIP_MESSAGE.search(line):
                continue
            window = "\n".join(lines[i:i + 8])
            if RETURN_OK.search(window):
                skip_gates.append(f"{f.name}:{i + 1}")

    return {"binaries": binaries, "ignored": ignored, "tests": total, "skip_gates": skip_gates}


def rows() -> list[dict]:
    known = cargo_dependencies()
    known_names = set(known)
    result = []
    for protocol, state in sorted(declared_states().items()):
        tests = test_directory(protocol)
        scan = scan_tests(tests)
        test_crates = imported_crates(tests, known_names)
        src_crates = imported_crates(ROOT / "src" / "server" / protocol, known_names)

        native = http_native(protocol)
        peers = sorted(c for c in test_crates if native or c not in GENERIC_HTTP)
        generic = [] if native else sorted(c for c in test_crates if c in GENERIC_HTTP)

        result.append({
            "protocol": protocol,
            "state": state,
            "binaries": sorted(b for b in scan["binaries"] if b not in GENERIC_HTTP),
            "crates": peers,
            "circular": sorted(set(peers) & src_crates),
            "optional": sorted(c for c in peers if known.get(c)),
            "optional_status": {c: known.get(c) for c in peers},
            "generic_only": generic,
            "ignored": scan["ignored"],
            "tests": scan["tests"],
            "skip_gates": scan["skip_gates"],
            "has_tests": tests.is_dir(),
        })
    return result


def independent_peers(row: dict) -> list[str]:
    """The peers that are neither this repository's own code nor a generic HTTP client.

    A crate the server itself imports is excluded: that is the `ssh`/russh case, where
    the "peer" is the server's own library and the test proves only that a crate agrees
    with itself.
    """
    return row["binaries"] + [c for c in row["crates"] if c not in row["circular"]]


def blocking_defects(row: dict) -> list[str]:
    """Failures a script can be sure about, which `--check` fails the build on."""
    out = []
    if not independent_peers(row):
        if row["generic_only"]:
            out.append(
                "only a generic HTTP client (" + ", ".join(row["generic_only"]) + ")"
            )
        elif row["crates"]:
            out.append("only circular peers (" + ", ".join(row["circular"]) + ")")
        else:
            out.append("no independent peer found")
    if row["skip_gates"]:
        out.append("skip-and-pass gate at " + ", ".join(row["skip_gates"]))
    if row["tests"] and len(row["ignored"]) >= row["tests"]:
        out.append(f"every test #[ignore]d ({len(row['ignored'])}/{row['tests']})")
    return out


def review_flags(row: dict) -> list[str]:
    """Things a human has to read the test to settle. Deliberately NOT build-failing.

    Two of them have documented, accepted exceptions:

    * **Circular.** `quic` (quinn) and `webrtc` (webrtc-rs) both use the same crate the
      server uses, and CLAUDE.md accepts both — because the crate is driven in the
      *opposite role*, completing a real handshake, rather than called as a codec.
      No static rule can tell those two apart, so this reports and does not decide.
    * **`#[ignore]` on some tests.** A suite may legitimately ignore its
      adapter-claiming or root-requiring tests while its real evidence runs.
    """
    out = []
    peers = independent_peers(row)
    if row["circular"] and peers:
        out.append(
            "peer shared with the server: "
            + ", ".join(row["circular"])
            + " — read the test: same crate in the opposite role is the accepted quic/webrtc case"
        )
    only_optional = peers and all(row["optional_status"].get(p) is True for p in peers)
    if only_optional:
        out.append(
            "every peer is an `optional = true` dependency ("
            + ", ".join(peers)
            + ") — compiled only when its feature is on, and the blocking CI job "
            "compiles 6 of 116 protocols, so this evidence does not run there: the "
            "AMQP/lapin hole"
        )
    elif row["optional"]:
        out.append("optional dependency among the peers: " + ", ".join(row["optional"]))
    if row["ignored"] and (not row["tests"] or len(row["ignored"]) < row["tests"]):
        out.append(f"{len(row['ignored'])} of {row['tests']} tests #[ignore]d")
    if not row["has_tests"]:
        out.append("no test directory at all")
    return out


def render(selected: list[dict], show_availability: bool) -> str:
    lines = [
        "| protocol | state | peer (binary / crate) | circular | optional dep | #[ignore]d | skip gate |",
        "|---|---|---|---|---|---|---|",
    ]
    for r in selected:
        peer_parts = []
        for b in r["binaries"]:
            mark = ""
            if show_availability:
                mark = " ✓" if shutil.which(b) else " ✗"
            peer_parts.append(f"`{b}`{mark}")
        peer_parts += [f"`{c}`" for c in r["crates"]]
        if not peer_parts:
            peer_parts = ["**none**" + (f" (generic: {', '.join(r['generic_only'])})" if r["generic_only"] else "")]
        lines.append(
            "| {p} | {s} | {peer} | {circ} | {opt} | {ign} | {skip} |".format(
                p=r["protocol"],
                s=r["state"],
                peer=", ".join(peer_parts),
                circ=", ".join(r["circular"]) or "-",
                opt=", ".join(r["optional"]) or "-",
                ign=f"{len(r['ignored'])}/{r['tests']}" if r["ignored"] else "-",
                skip=", ".join(r["skip_gates"]) or "-",
            )
        )
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--all", action="store_true", help="every protocol, not only Beta")
    parser.add_argument("--check", action="store_true", help="exit 1 if a Beta rating's evidence cannot execute")
    parser.add_argument(
        "--experimental-with-evidence",
        action="store_true",
        help="Experimental protocols that already have an independent peer — promotion candidates",
    )
    parser.add_argument(
        "--no-availability",
        action="store_true",
        help="do not mark whether each binary is installed on this machine",
    )
    args = parser.parse_args()

    all_rows = rows()
    show = not args.no_availability

    if args.experimental_with_evidence:
        candidates = [
            r for r in all_rows
            if r["state"] == "Experimental" and (r["binaries"] or r["crates"])
            and not r["circular"] and not r["skip_gates"] and not r["ignored"]
        ]
        print("# Experimental protocols whose evidence already executes\n")
        print("Each has an independent peer, no circular crate, no skip gate and no `#[ignore]`.")
        print("Read the test before promoting: this cannot tell a client completing a session")
        print("from a codec being called, and that distinction is the whole rating.\n")
        print(render(candidates, show))
        return 0

    selected = all_rows if args.all else [r for r in all_rows if r["state"] == "Beta"]
    title = "every server protocol" if args.all else "every Beta server protocol"
    print(f"# Evidence behind {title}\n")
    print("Generated by `scripts/beta_evidence_table.py` — do not hand-maintain this.")
    if show:
        print("A ✓ or ✗ after a binary says whether it is installed on the machine that ran this.")
    print()
    print(render(selected, show))

    beta = [r for r in all_rows if r["state"] == "Beta"]
    problems = [(r, blocking_defects(r)) for r in beta]
    problems = [(r, d) for r, d in problems if d]
    print("\n## Beta ratings with no evidence a script can find\n")
    if problems:
        print("Each of these is either over-rated or driving its peer in a way this scan")
        print("cannot see. Read the test before concluding which.\n")
        for r, d in problems:
            print(f"- **{r['protocol']}** — {'; '.join(d)}")
    else:
        print("None.")

    flagged = [(r, review_flags(r)) for r in beta]
    flagged = [(r, fl) for r, fl in flagged if fl]
    print("\n## Beta ratings a human should re-read\n")
    if flagged:
        print("Not failures. These are the cases where whether the evidence counts depends")
        print("on what the test does with the peer, which no static scan can decide.\n")
        for r, fl in flagged:
            print(f"- **{r['protocol']}** — {'; '.join(fl)}")
    else:
        print("None.")

    counts: dict[str, int] = {}
    for r in all_rows:
        counts[r["state"]] = counts.get(r["state"], 0) + 1
    print("\n## Counts\n")
    for state, n in sorted(counts.items()):
        print(f"- {state}: {n}")

    if args.check and problems:
        print(f"\nFAIL: {len(problems)} Beta protocol(s) rest on evidence that cannot execute.", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
