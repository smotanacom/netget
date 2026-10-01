#!/usr/bin/env python3
"""Keep the on-demand fuzz matrix, Cargo binaries, and target sources in agreement.

This source-only CI check uses Python's standard library (including on Ubuntu 22.04's
Python 3.10). The deliberately narrow readers fail closed on unfamiliar declarations.
"""

from collections import Counter
from pathlib import Path
import re
import sys


def targets(root):
    manifest = (root / "fuzz/Cargo.toml").read_text()
    bins = re.split(r"(?m)^\[\[bin\]\]\s*$", manifest)[1:]
    declared = []
    for block in bins:
        name = re.search(r'^name\s*=\s*"([a-z0-9_]+)"\s*$', block, re.M)
        path = re.search(r'^path\s*=\s*"([^"]+)"\s*$', block, re.M)
        if not name or not path:
            raise ValueError("each fuzz [[bin]] must declare a literal name and path")
        if path[1] != f"fuzz_targets/{name[1]}.rs":
            raise ValueError(f"unexpected source path for {name[1]}: {path[1]}")
        declared.append(name[1])

    workflow = (root / ".github/workflows/fuzz.yml").read_text()
    matrix = re.search(r"(?m)^        target:\s*\n((?:          - [a-z0-9_]+\s*\n)+)", workflow)
    if not declared or not matrix:
        raise ValueError("missing fuzz binaries or literal workflow target matrix")
    scheduled = re.findall(r"- ([a-z0-9_]+)", matrix[1])
    sources = [p.stem for p in (root / "fuzz/fuzz_targets").glob("*.rs")]

    errors = []
    for label, values in [("Cargo", declared), ("workflow", scheduled)]:
        duplicates = sorted(name for name, count in Counter(values).items() if count > 1)
        if duplicates:
            errors.append(f"duplicate {label} targets: {', '.join(duplicates)}")
    for label, values in [("workflow", scheduled), ("sources", sources)]:
        missing = sorted(set(declared) - set(values))
        extra = sorted(set(values) - set(declared))
        if missing:
            errors.append(f"{label} missing Cargo targets: {', '.join(missing)}")
        if extra:
            errors.append(f"{label} has undeclared targets: {', '.join(extra)}")
    if errors:
        raise ValueError("\n".join(errors))
    return declared


if __name__ == "__main__":
    try:
        found = targets(Path(__file__).resolve().parents[1])
    except (OSError, ValueError) as error:
        print(error, file=sys.stderr)
        sys.exit(1)
    print(f"All {len(found)} fuzz targets have Cargo declarations, sources, and workflow jobs.")
