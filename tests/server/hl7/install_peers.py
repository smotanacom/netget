"""Pinned, unchanged python-hl7 0.4.5 (BSD) in an owned venv; MLLP client and server.

Usage: python3 install_peers.py /absolute/owned/root   (Python >= 3.9)
"""
import hashlib, pathlib, ssl, subprocess, sys, urllib.request, venv

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
assert sys.version_info >= (3, 9), "Python >= 3.9 required"
name = "hl7-0.4.5-py2.py3-none-any.whl"
url = "https://files.pythonhosted.org/packages/8d/2b/024977b044ca3aa79153e0a4ff13cc465064ccf9cf7b60df46e6eab8340e/" + name
digest = "f46bf7c165801fd40e8fcc9f2be41c5794415039adc97646aee7ee6b20ce8ccd"
wheel = root / name
if not wheel.exists():
    ctx = ssl.create_default_context(cafile="/etc/ssl/cert.pem" if sys.platform == "darwin" else None)
    with urllib.request.urlopen(urllib.request.Request(url, headers={"User-Agent": "netget-hl7-peer"}), context=ctx, timeout=60) as r:
        wheel.write_bytes(r.read(65536))
assert hashlib.sha256(wheel.read_bytes()).hexdigest() == digest, "pinned SHA-256 mismatch"
env = root / "venv"
if not (env / "bin" / "python").exists():
    venv.EnvBuilder(with_pip=True).create(env)
python = str(env / "bin" / "python")
subprocess.run([python, "-m", "pip", "install", "--quiet", "--disable-pip-version-check", "--no-deps", str(wheel)], check=True, timeout=300)
subprocess.run([python, "-c", "import importlib.metadata as m, hl7.mllp; assert m.version('hl7') == '0.4.5'"], check=True, timeout=30)
print("export NETGET_HL7_PYTHON=" + python)
