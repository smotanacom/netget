"""Pinned, unchanged flup 1.0.3 (BSD) — a WSGI FastCGI server — in an owned venv, for NetGet's
FastCGI client. NetGet's FastCGI responder is tested with nginx, the system package.

Usage: python3 install_peers.py /absolute/owned/root   (Python >= 3.8)
"""
import hashlib, pathlib, ssl, subprocess, sys, urllib.request, venv

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
name = "flup-1.0.3-py3-none-any.whl"
url = "https://files.pythonhosted.org/packages/88/e5/17bcf4431e811ffaec213feea7609a6f003084006d2e210f53cee09095d9/" + name
digest = "ca9fd78e1cc0431da1236f73fafd1c01db684675b4d369460d5f5c62e6f0b8d6"
wheel = root / name
if not wheel.exists():
    ctx = ssl.create_default_context(cafile="/etc/ssl/cert.pem" if sys.platform == "darwin" else None)
    with urllib.request.urlopen(urllib.request.Request(url, headers={"User-Agent": "netget-fastcgi-peer"}), context=ctx, timeout=60) as r:
        wheel.write_bytes(r.read(1_000_000))
assert hashlib.sha256(wheel.read_bytes()).hexdigest() == digest, "pinned SHA-256 mismatch"
env = root / "venv"
if not (env / "bin" / "python").exists():
    venv.EnvBuilder(with_pip=True).create(env)
python = str(env / "bin" / "python")
subprocess.run([python, "-m", "pip", "install", "--quiet", "--disable-pip-version-check", "--no-deps", str(wheel)], check=True, timeout=600)
subprocess.run([python, "-c", "import importlib.metadata as m; assert m.version('flup') == '1.0.3'"], check=True, timeout=30)
print("export NETGET_FASTCGI_PYTHON=" + python)
