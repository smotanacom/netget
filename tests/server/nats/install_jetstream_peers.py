"""Pinned, unchanged JetStream clients in an owned ROOT:
- nats.go v1.54.0's jetstream package, built from jetstream_peer/ (go.sum-pinned; its
  dependencies require Go 1.26, which Go fetches by itself under GOTOOLCHAIN=auto);
- nats-py 2.16.0, installed with --require-hashes, which runs jetstream_peer.py here.
Needs Python >= 3.12 and Go on PATH.

Usage: python3 install_jetstream_peers.py /absolute/owned/root
Prints the NETGET_JS_GO_PEER and NETGET_JS_PYTHON exports the tests read.
"""
import os, pathlib, subprocess, sys, venv

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
here = pathlib.Path(__file__).resolve().parent
env = root / "venv"
if not (env / "bin" / "python").exists():
    venv.EnvBuilder(with_pip=True).create(env)
python = str(env / "bin" / "python")
subprocess.run([python, "-m", "pip", "install", "--quiet", "--disable-pip-version-check", "--require-hashes", "-r", str(here / "jetstream_requirements.txt")], check=True, timeout=900)
subprocess.run([python, "-c", "import nats.js"], check=True, timeout=60)
peer = root / "nats-jetstream-peer"
build_env = dict(os.environ, GOFLAGS="-mod=readonly", CGO_ENABLED="0")
subprocess.run(["go", "build", "-trimpath", "-o", str(peer), "."], cwd=here / "jetstream_peer", env=build_env, check=True, timeout=900)
subprocess.run(["go", "mod", "verify"], cwd=here / "jetstream_peer", env=build_env, check=True, timeout=300)
print("export NETGET_JS_GO_PEER=" + str(peer))
print("export NETGET_JS_PYTHON=" + python)
