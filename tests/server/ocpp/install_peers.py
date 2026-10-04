"""Pinned, unchanged python ocpp 2.1.0 (MIT, schema-validating) with websockets 15.0.1 in an
owned venv; both charge point and central system roles.

Usage: python3 install_peers.py /absolute/owned/root   (Python >= 3.11)
"""
import hashlib, pathlib, ssl, subprocess, sys, urllib.request, venv

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
assert sys.version_info >= (3, 11), "Python >= 3.11 required (ocpp 2.1.0)"
name = "ocpp-2.1.0-py3-none-any.whl"
url = "https://files.pythonhosted.org/packages/7e/5d/9eb35db3947f816c8edd3b99cb94911796aae8a6dbc8d0a5befdea7989f6/" + name
digest = "0e452c14c21e2995431334ce1513d5ad8246ecb5c8fb90ed4143e2ecdd9e2845"
wheel = root / name
if not wheel.exists():
    ctx = ssl.create_default_context(cafile="/etc/ssl/cert.pem" if sys.platform == "darwin" else None)
    with urllib.request.urlopen(urllib.request.Request(url, headers={"User-Agent": "netget-ocpp-peer"}), context=ctx, timeout=60) as r:
        wheel.write_bytes(r.read(600000))
assert hashlib.sha256(wheel.read_bytes()).hexdigest() == digest, "pinned SHA-256 mismatch"
env = root / "venv"
if not (env / "bin" / "python").exists():
    venv.EnvBuilder(with_pip=True).create(env)
python = str(env / "bin" / "python")
pip = [python, "-m", "pip", "install", "--quiet", "--disable-pip-version-check"]
subprocess.run(pip + ["websockets==15.0.1", "jsonschema>=4.23,<5"], check=True, timeout=600)
subprocess.run(pip + ["--no-deps", str(wheel)], check=True, timeout=300)
subprocess.run([python, "-c", "import importlib.metadata as m; assert m.version('ocpp')=='2.1.0' and m.version('websockets')=='15.0.1'"], check=True, timeout=30)
print("export NETGET_OCPP_PYTHON=" + python)
