"""Pinned, unchanged EPP peers in an owned root: pyepp 0.2.0 (InternetNZ, MIT) with its
dependencies installed with --require-hashes from requirements.txt, and the test registry in
registry/ built on epp-lib v0.2.0 (the Swedish Internet Foundation, MIT; go.sum-pinned).

Usage: python3 install_peers.py /absolute/owned/root     (needs Go 1.23 or newer on PATH)
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
subprocess.run([python, "-c", "import importlib.metadata as m, pyepp; assert m.version('pyepp') == '0.2.0'"], check=True, timeout=30)
registry = root / "bin" / "registry"
registry.parent.mkdir(exist_ok=True)
build_env = dict(os.environ, GOFLAGS="-mod=readonly", CGO_ENABLED="0")
subprocess.run(["go", "build", "-trimpath", "-o", str(registry), "."], cwd=here / "registry", env=build_env, check=True, timeout=900)
subprocess.run(["go", "mod", "verify"], cwd=here / "registry", env=build_env, check=True, timeout=300)
print("export NETGET_EPP_PYTHON=" + python)
print("export NETGET_EPP_REGISTRY=" + str(registry))
