"""Build the libp2p peer (go-libp2p v0.41.1, go.sum-pinned) into an owned ROOT.
Needs Go 1.24 or newer on PATH.

Usage: python3 install_peers.py /absolute/owned/root
Prints the NETGET_LIBP2P_GO_PEER export the server and client tests read.
"""
import os, pathlib, subprocess, sys

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
here = pathlib.Path(__file__).resolve().parent / "peer"
peer = root / "libp2p-peer"
env = dict(os.environ, GOFLAGS="-mod=readonly", CGO_ENABLED="0")
subprocess.run(["go", "build", "-trimpath", "-o", str(peer), "."], cwd=here, env=env, check=True, timeout=1800)
subprocess.run(["go", "mod", "verify"], cwd=here, env=env, check=True, timeout=300)
print("export NETGET_LIBP2P_GO_PEER=" + str(peer))
