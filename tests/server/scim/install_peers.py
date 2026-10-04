"""Pinned, unchanged python-scim peers (Apache-2.0) in an owned venv: scim2-tester 0.5.1 (a
SCIM compliance checker) and scim2-cli 0.4.0 against NetGet's service, scim2-server 0.4.0 for
NetGet's client. The three wheels are hash-pinned; their dependencies come from PyPI.

Usage: python3 install_peers.py /absolute/owned/root   (Python >= 3.11)
"""
import hashlib, pathlib, ssl, subprocess, sys, urllib.request, venv

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
assert sys.version_info >= (3, 11), "Python >= 3.11 required"
WHEELS = [
    ("scim2_tester-0.5.1-py3-none-any.whl", "https://files.pythonhosted.org/packages/c4/79/69f23f8c4245d39515ee2a5687e74d25f2133256d103bfeef16d9d113605/", "ee9d649bf02b2e5e3b61a6f2b5b4ffa23bf350ea730ccc5ae6d9e97abf6ce484"),
    ("scim2_cli-0.4.0-py3-none-any.whl", "https://files.pythonhosted.org/packages/35/13/e89b08a3710a0aad4916c9ab2c4a0c17d26c198648b1b06bff0606ba903f/", "201b3cd7f1d9bc262817ebc1d406697de1378c2baec6f83e1cccdd495370c928"),
    ("scim2_server-0.4.0-py3-none-any.whl", "https://files.pythonhosted.org/packages/b2/62/df9bde15bf9af425dce1d4a620265e9b409b0b9830fe0ca2e30a47e043a0/", "8ed566adf0bed129f13c75b795c869de26b371780082a08c5e02e63bf66d4d52"),
]
ctx = ssl.create_default_context(cafile="/etc/ssl/cert.pem" if sys.platform == "darwin" else None)
paths = []
for name, base, digest in WHEELS:
    wheel = root / name
    if not wheel.exists():
        with urllib.request.urlopen(urllib.request.Request(base + name, headers={"User-Agent": "netget-scim-peer"}), context=ctx, timeout=60) as r:
            wheel.write_bytes(r.read(2_000_000))
    assert hashlib.sha256(wheel.read_bytes()).hexdigest() == digest, "pinned SHA-256 mismatch: " + name
    paths.append(str(wheel))
env = root / "venv"
if not (env / "bin" / "python").exists():
    venv.EnvBuilder(with_pip=True).create(env)
python = str(env / "bin" / "python")
subprocess.run([python, "-m", "pip", "install", "--quiet", "--disable-pip-version-check", *paths], check=True, timeout=900)
subprocess.run([python, "-c", "import importlib.metadata as m; assert (m.version('scim2-tester'), m.version('scim2-cli'), m.version('scim2-server')) == ('0.5.1', '0.4.0', '0.4.0')"], check=True, timeout=30)
print("export NETGET_SCIM_BIN=" + str(env / "bin"))
