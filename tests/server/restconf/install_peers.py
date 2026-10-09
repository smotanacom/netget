"""Pinned, unchanged FreeCONF RESTCONF (restconf and yang modules, Apache-2.0, Go) built into an
owned ROOT from `fc/` (go.mod and go.sum pin every module; `-mod=readonly` refuses anything
else): one binary that serves FreeCONF's car example over RESTCONF, or runs FreeCONF's RESTCONF
client against a URL. Needs Go >= 1.25.

Usage: python3 install_peers.py /absolute/owned/root
"""
import os, pathlib, subprocess, sys

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
src = pathlib.Path(__file__).resolve().parent / "fc"
env = dict(os.environ, GOFLAGS="-mod=readonly", CGO_ENABLED="0")
subprocess.run(["go", "build", "-o", str(root / "fc"), "."], cwd=src, env=env, check=True, timeout=1800)
print("export NETGET_RESTCONF_FREECONF=" + str(root / "fc"))
