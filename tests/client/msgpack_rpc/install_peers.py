"""MessagePack-RPC peers in an owned ROOT:
- ugorji/go v1.3.2's MsgpackSpecRpc codec, built from peer/ (go.sum-pinned), as client and
  server;
- Neovim, which must already be installed (`apt-get install neovim`, `brew install neovim`):
  it is a MessagePack-RPC client through rpcrequest/rpcnotify and a server through --listen.
Needs Go 1.24 or newer on PATH.

Usage: python3 install_peers.py /absolute/owned/root
Prints the NETGET_MSGPACK_PEER and NETGET_MSGPACK_NVIM exports the tests read.
"""
import os, pathlib, shutil, subprocess, sys

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
here = pathlib.Path(__file__).resolve().parent
peer = root / "msgpack-peer"
env = dict(os.environ, GOFLAGS="-mod=readonly", CGO_ENABLED="0")
subprocess.run(["go", "build", "-trimpath", "-o", str(peer), "."], cwd=here / "peer", env=env, check=True, timeout=900)
subprocess.run(["go", "mod", "verify"], cwd=here / "peer", env=env, check=True, timeout=300)
nvim = shutil.which("nvim")
assert nvim, "nvim is required: apt-get install neovim (or brew install neovim)"
subprocess.run([nvim, "--version"], check=True, timeout=60, stdout=subprocess.DEVNULL)
print("export NETGET_MSGPACK_PEER=" + str(peer))
print("export NETGET_MSGPACK_NVIM=" + nvim)
