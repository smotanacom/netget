"""Pinned, unchanged a2a-sdk 1.2.1 (Apache-2.0; A2A protocol 1.0) with its http-server extra
and uvicorn 0.34.0 in an owned venv; both agent (server) and client roles.

Usage: python3 install_peers.py /absolute/owned/root   (Python >= 3.10)
"""
import hashlib, pathlib, ssl, subprocess, sys, urllib.request, venv

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
assert sys.version_info >= (3, 10), "Python >= 3.10 required"
name = "a2a_sdk-1.2.1-py3-none-any.whl"
url = "https://files.pythonhosted.org/packages/cd/71/eca113a1bf8b8896389181d0cc68692011ccf0fba7b32c5746c32a04518d/" + name
digest = "5be3ed614f0dfb8f1e13c201dc603f7e01f364613951c1d492fddd1ffc010e29"
wheel = root / name
if not wheel.exists():
    ctx = ssl.create_default_context(cafile="/etc/ssl/cert.pem" if sys.platform == "darwin" else None)
    with urllib.request.urlopen(urllib.request.Request(url, headers={"User-Agent": "netget-a2a-peer"}), context=ctx, timeout=60) as r:
        wheel.write_bytes(r.read(400000))
assert hashlib.sha256(wheel.read_bytes()).hexdigest() == digest, "pinned SHA-256 mismatch"
env = root / "venv"
if not (env / "bin" / "python").exists():
    venv.EnvBuilder(with_pip=True).create(env)
python = str(env / "bin" / "python")
subprocess.run([python, "-m", "pip", "install", "--quiet", "--disable-pip-version-check", f"{wheel}[http-server]", "uvicorn==0.34.0"], check=True, timeout=900)
subprocess.run([python, "-c", "import importlib.metadata as m; assert m.version('a2a-sdk') == '1.2.1'"], check=True, timeout=30)
print("export NETGET_A2A_PYTHON=" + python)
