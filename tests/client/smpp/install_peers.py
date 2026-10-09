"""Pinned, unchanged SMPP peers in an owned ROOT:
- smpplib 2.2.4 (LGPL-3.0, Python) with six, installed with --require-hashes, which runs
  smpplib_peer.py in this directory;
- linxGnu/gosmpp v0.3.1 as an ESME, built from peer/ (go.sum-pinned);
- Melrose Labs' SMSC simulator, the single C++ file gosmpp v0.3.1 ships as
  example/smsc_simulator/smsc.cpp, compiled unchanged from the go.sum-verified module. It
  listens on the fixed ports 2775 (SMPP) and 8775 (admin).
Needs Python >= 3.10, Go 1.24 or newer and g++ on PATH.

Usage: python3 install_peers.py /absolute/owned/root
Prints the NETGET_SMPP_PYTHON, NETGET_SMPP_GO_PEER and NETGET_SMPP_SMSC_SIM exports.
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
subprocess.run([python, "-c", "import importlib.metadata as m; assert m.version('smpplib') == '2.2.4'"], check=True, timeout=30)
peer = root / "smpp-peer"
build_env = dict(os.environ, GOFLAGS="-mod=readonly", CGO_ENABLED="0")
subprocess.run(["go", "build", "-trimpath", "-o", str(peer), "."], cwd=here / "peer", env=build_env, check=True, timeout=900)
subprocess.run(["go", "mod", "verify"], cwd=here / "peer", env=build_env, check=True, timeout=300)
gosmpp = subprocess.run(["go", "list", "-m", "-f", "{{.Dir}}", "github.com/linxGnu/gosmpp"], cwd=here / "peer", env=build_env, check=True, timeout=300, capture_output=True, text=True).stdout.strip()
sim = root / "smsc-sim"
subprocess.run(["g++", "-O2", "-o", str(sim), str(pathlib.Path(gosmpp) / "example" / "smsc_simulator" / "smsc.cpp")], check=True, timeout=900)
print("export NETGET_SMPP_PYTHON=" + python)
print("export NETGET_SMPP_SMSC_SIM=" + str(sim))
print("export NETGET_SMPP_GO_PEER=" + str(peer))
