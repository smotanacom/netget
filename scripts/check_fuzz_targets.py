#!/usr/bin/env python3
"""Validate Cargo fuzz binaries and sources, and generate the workflow target matrix.

This source-only CI check uses Python's standard library (including on Ubuntu 22.04's
Python 3.10). Full dispatches derive their matrix from this list, so a new Cargo target
cannot silently miss the workflow. Optional single-target runs must name a declared target.
"""

import argparse
from collections import Counter
import json
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

    if not declared:
        raise ValueError("missing fuzz binaries")
    sources = [p.stem for p in (root / "fuzz/fuzz_targets").glob("*.rs")]

    errors = []
    duplicates = sorted(name for name, count in Counter(declared).items() if count > 1)
    if duplicates:
        errors.append(f"duplicate Cargo targets: {', '.join(duplicates)}")
    missing = sorted(set(declared) - set(sources))
    extra = sorted(set(sources) - set(declared))
    if missing:
        errors.append(f"sources missing Cargo targets: {', '.join(missing)}")
    if extra:
        errors.append(f"sources have undeclared targets: {', '.join(extra)}")
    if errors:
        raise ValueError("\n".join(errors))
    return declared


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--json", action="store_true", help="emit the selected workflow matrix")
    parser.add_argument("--target", default="", help="select one declared target; empty selects all")
    args = parser.parse_args()
    try:
        found = targets(Path(__file__).resolve().parents[1])
        if args.target and args.target not in found:
            raise ValueError(f"unknown fuzz target: {args.target}; choose from {', '.join(found)}")
    except (OSError, ValueError) as error:
        print(error, file=sys.stderr)
        sys.exit(1)
    if args.json:
        print(json.dumps([args.target] if args.target else found))
    else:
        print(f"All {len(found)} fuzz targets have Cargo declarations and sources; full runs schedule all.")
