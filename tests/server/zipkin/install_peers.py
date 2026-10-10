"""Pinned, unchanged Zipkin reporters in an owned ROOT:
- openzipkin/zipkin-go v0.4.3 (tracer, HTTP reporter, model decoder), built from peer/
  (go.sum-pinned);
- OpenTelemetry Python 1.45.1's Zipkin JSON exporter, installed with --require-hashes, which
  runs otel_peer.py in this directory.
Needs Python >= 3.12 and Go 1.24 or newer on PATH.

Usage: python3 install_peers.py /absolute/owned/root
Prints the NETGET_ZIPKIN_GO_PEER and NETGET_ZIPKIN_PYTHON exports the tests read.
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
subprocess.run([python, "-c", "import opentelemetry.exporter.zipkin.json"], check=True, timeout=60)
peer = root / "zipkin-peer"
build_env = dict(os.environ, GOFLAGS="-mod=readonly", CGO_ENABLED="0")
subprocess.run(["go", "build", "-trimpath", "-o", str(peer), "."], cwd=here / "peer", env=build_env, check=True, timeout=900)
subprocess.run(["go", "mod", "verify"], cwd=here / "peer", env=build_env, check=True, timeout=300)
print("export NETGET_ZIPKIN_GO_PEER=" + str(peer))
print("export NETGET_ZIPKIN_PYTHON=" + python)
