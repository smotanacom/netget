"""Build the PFCP peer (wmnsk/go-pfcp v0.0.24, go.sum-pinned) into an owned ROOT.
Needs Go 1.24 or newer on PATH.

Usage: python3 install_peers.py /absolute/owned/root
Prints the NETGET_PFCP_GO_PEER export the server and client tests read.
"""
import os, pathlib, subprocess, sys

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
here = pathlib.Path(__file__).resolve().parent / "peer"
peer = root / "pfcp-peer"
env = dict(os.environ, GOFLAGS="-mod=readonly", CGO_ENABLED="0")
subprocess.run(["go", "build", "-trimpath", "-o", str(peer), "."], cwd=here, env=env, check=True, timeout=900)
subprocess.run(["go", "mod", "verify"], cwd=here, env=env, check=True, timeout=300)
print("export NETGET_PFCP_GO_PEER=" + str(peer))
