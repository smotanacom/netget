"""Pinned, unchanged A2S peers in an owned root: python-a2s 1.4.2 (installed with
--require-hashes) and a binary built from peer/ on github.com/woozymasta/a2s v0.4.0
(go.sum-pinned), whose client mode is woozymasta's client and whose server mode is its
UDP server.

Usage: python3 install_peers.py /absolute/owned/root     (needs Go 1.25 or newer on PATH)
Prints the NETGET_A2S_PYTHON and NETGET_A2S_PEER exports the tests read.
"""
import os, pathlib, subprocess, sys, venv

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
here = pathlib.Path(__file__).resolve().parent
env = root / "venv"
if not (env / "bin" / "python").exists():
    venv.EnvBuilder(with_pip=True).create(env)
python = str(env / "bin" / "python")
subprocess.run([python, "-m", "pip", "install", "--quiet", "--disable-pip-version-check", "--require-hashes", "-r", str(here / "requirements.txt")], check=True, timeout=900)
subprocess.run([python, "-c", "import importlib.metadata as m; assert m.version('python-a2s') == '1.4.2'"], check=True, timeout=30)
peer = root / "bin" / "a2s-peer"
peer.parent.mkdir(exist_ok=True)
build_env = dict(os.environ, GOFLAGS="-mod=readonly", CGO_ENABLED="0")
subprocess.run(["go", "build", "-trimpath", "-o", str(peer), "."], cwd=here / "peer", env=build_env, check=True, timeout=900)
subprocess.run(["go", "mod", "verify"], cwd=here / "peer", env=build_env, check=True, timeout=300)
print("export NETGET_A2S_PYTHON=" + python)
print("export NETGET_A2S_PEER=" + str(peer))
