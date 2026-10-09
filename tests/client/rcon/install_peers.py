"""Pinned, unchanged RCON peers in an owned root: the Python rcon package 2.4.9 (installed with
--require-hashes) and a binary built from peer/ on github.com/gorcon/rcon v1.4.0 (go.sum-pinned),
whose client mode is gorcon's client and whose server mode is gorcon's rcontest server.

Usage: python3 install_peers.py /absolute/owned/root     (needs Go 1.23 or newer on PATH)
Prints the NETGET_RCON_PYTHON and NETGET_RCON_PEER exports the tests read.
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
subprocess.run([python, "-c", "import importlib.metadata as m; assert m.version('rcon') == '2.4.9'"], check=True, timeout=30)
peer = root / "bin" / "rcon-peer"
peer.parent.mkdir(exist_ok=True)
build_env = dict(os.environ, GOFLAGS="-mod=readonly", CGO_ENABLED="0")
subprocess.run(["go", "build", "-trimpath", "-o", str(peer), "."], cwd=here / "peer", env=build_env, check=True, timeout=900)
subprocess.run(["go", "mod", "verify"], cwd=here / "peer", env=build_env, check=True, timeout=300)
print("export NETGET_RCON_PYTHON=" + python)
print("export NETGET_RCON_PEER=" + str(peer))
