"""Pinned, unchanged WAMP peers in an owned ROOT.

- nexus v3.3.0 (MIT, Go): `nexus/` is a small program on its public client and router API;
  go.sum pins every module and `-mod=readonly` refuses anything else. Needs Go >= 1.25.
- autobahn-python 24.4.2 (MIT), the pure-Python wheel, hash-pinned; its dependencies come
  from PyPI.

Usage: python3 install_peers.py /absolute/owned/root   (Python >= 3.10)
"""
import hashlib, os, pathlib, ssl, subprocess, sys, urllib.request, venv

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
assert sys.version_info >= (3, 10), "Python >= 3.10 required"
here = pathlib.Path(__file__).resolve().parent
env = dict(os.environ, GOFLAGS="-mod=readonly", CGO_ENABLED="0")
subprocess.run(["go", "build", "-o", str(root / "nexus"), "."], cwd=here / "nexus", env=env, check=True, timeout=900)
ctx = ssl.create_default_context(cafile="/etc/ssl/cert.pem" if sys.platform == "darwin" else None)
name = "autobahn-24.4.2-py2.py3-none-any.whl"
url = "https://files.pythonhosted.org/packages/13/ee/a6475f39ef6c6f41c33da6b193e0ffd2c6048f52e1698be6253c59301b72/" + name
wheel = root / name
if not wheel.exists():
    with urllib.request.urlopen(urllib.request.Request(url, headers={"User-Agent": "netget-wamp-peer"}), context=ctx, timeout=60) as r:
        wheel.write_bytes(r.read(5_000_000))
assert hashlib.sha256(wheel.read_bytes()).hexdigest() == "c56a2abe7ac78abbfb778c02892d673a4de58fd004d088cd7ab297db25918e81", "pinned SHA-256 mismatch: " + name
venv_dir = root / "venv"
if not (venv_dir / "bin" / "python").exists():
    venv.EnvBuilder(with_pip=True).create(venv_dir)
python = str(venv_dir / "bin" / "python")
subprocess.run([python, "-m", "pip", "install", "--quiet", "--disable-pip-version-check", str(wheel)], check=True, timeout=900)
subprocess.run([python, "-c", "import importlib.metadata as m; assert m.version('autobahn') == '24.4.2'; import autobahn.asyncio.wamp"], check=True, timeout=60)
print("export NETGET_WAMP_NEXUS=" + str(root / "nexus"))
print("export NETGET_WAMP_PYTHON=" + python)
