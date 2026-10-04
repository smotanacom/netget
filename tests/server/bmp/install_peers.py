"""Pinned, unchanged BMP peers built into an owned ROOT from `tools/` (go.mod and go.sum pin
every module; `-mod=readonly` refuses anything else). Needs Go >= 1.25.

- GoBGP v4.9.0 (Apache-2.0, Go): gobgpd, a BGP speaker exporting BMP to NetGet's collector, and
  the gobgp CLI that feeds its peer routes.
- gobmp v1.1.0 (Apache-2.0, Go): a BMP collector that parses what NetGet's exporter sends.

Usage: python3 install_peers.py /absolute/owned/root
"""
import os, pathlib, subprocess, sys

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
tools = pathlib.Path(__file__).resolve().parent / "tools"
env = dict(os.environ, GOFLAGS="-mod=readonly", CGO_ENABLED="0")
for name, package in (("gobgpd", "github.com/osrg/gobgp/v4/cmd/gobgpd"),
                      ("gobgp", "github.com/osrg/gobgp/v4/cmd/gobgp"),
                      ("gobmp", "github.com/sbezverk/gobmp/cmd/gobmp")):
    subprocess.run(["go", "build", "-o", str(root / name), package], cwd=tools, env=env, check=True, timeout=1800)
out = subprocess.run([str(root / "gobgpd"), "--version"], check=True, timeout=30, capture_output=True, text=True).stdout
assert "4.9.0" in out, out
print("export NETGET_BMP_GOBGPD=" + str(root / "gobgpd"))
print("export NETGET_BMP_GOBGP=" + str(root / "gobgp"))
print("export NETGET_BMP_GOBMP=" + str(root / "gobmp"))
