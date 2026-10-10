"""Pinned, unchanged ZeroMQ peers in an owned ROOT:
- pyzmq 27.2.0 (BSD, bundling libzmq 4.3.5), installed with --require-hashes, which runs
  peer.py in this directory;
- go-zeromq/zmq4 v0.17.0, a pure-Go ZMTP implementation, built from peer/ (go.sum-pinned).
Needs Python >= 3.12 and Go 1.24 or newer on PATH.

Usage: python3 install_peers.py /absolute/owned/root
Prints the NETGET_ZEROMQ_PYTHON and NETGET_ZEROMQ_GO_PEER exports the tests read.
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
subprocess.run([python, "-c", "import zmq; assert zmq.pyzmq_version() == '27.2.0', zmq.pyzmq_version()"], check=True, timeout=30)
peer = root / "zeromq-peer"
build_env = dict(os.environ, GOFLAGS="-mod=readonly", CGO_ENABLED="0")
subprocess.run(["go", "build", "-trimpath", "-o", str(peer), "."], cwd=here / "peer", env=build_env, check=True, timeout=900)
subprocess.run(["go", "mod", "verify"], cwd=here / "peer", env=build_env, check=True, timeout=300)
print("export NETGET_ZEROMQ_PYTHON=" + python)
print("export NETGET_ZEROMQ_GO_PEER=" + str(peer))
