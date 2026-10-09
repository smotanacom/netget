"""Build the 9P peer in an owned ROOT: tests/client/ninep/peer, a small Go program over
unchanged 9fans.net/go v0.0.7 (plan9/client) and github.com/knusbaum/go9p v1.18.0 (client and
in-memory file server), pinned by go.sum. Needs Go 1.24 or newer on PATH.

Usage: python3 install_peers.py /absolute/owned/root
Prints the NETGET_NINEP_PEER export the tests read.
"""
import os, pathlib, subprocess, sys

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
here = pathlib.Path(__file__).resolve().parent / "peer"
peer = root / "ninep-peer"
env = dict(os.environ, GOFLAGS="-mod=readonly", CGO_ENABLED="0")
subprocess.run(["go", "build", "-trimpath", "-o", str(peer), "."], cwd=here, env=env, check=True, timeout=900)
subprocess.run(["go", "mod", "verify"], cwd=here, env=env, check=True, timeout=300)
print("export NETGET_NINEP_PEER=" + str(peer))
