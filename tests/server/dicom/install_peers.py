"""Pinned, unchanged DICOM peer in an owned venv: pynetdicom 3.0.4 (MIT) on pydicom 3.0.2 (MIT),
used as the SCU against NetGet's SCP and as the SCP for NetGet's SCU. Both wheels are
hash-pinned and installed without dependencies (neither has a required one).

Usage: python3 install_peers.py /absolute/owned/root   (Python >= 3.10)
"""
import hashlib, pathlib, ssl, subprocess, sys, urllib.request, venv

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
assert sys.version_info >= (3, 10), "Python >= 3.10 required"
WHEELS = [
    ("pydicom-3.0.2-py3-none-any.whl", "https://files.pythonhosted.org/packages/46/e0/60466c6d712dad2cf807df315e39863e91609ffd1064ecb835994460bbda/", "abf971a5440f84dbaf42c4b6758e30e62480902584f8b270b9a5d146e278a07b"),
    ("pynetdicom-3.0.4-py3-none-any.whl", "https://files.pythonhosted.org/packages/63/77/9741d8bb92a44fefd080ee54017707609690cb848fca5fe89f6608e4df99/", "bc3f8869db4c90634336dfb02d7b6c249771e8b167e841254997a315d8e16f72"),
]
ctx = ssl.create_default_context(cafile="/etc/ssl/cert.pem" if sys.platform == "darwin" else None)
paths = []
for name, base, digest in WHEELS:
    wheel = root / name
    if not wheel.exists():
        with urllib.request.urlopen(urllib.request.Request(base + name, headers={"User-Agent": "netget-dicom-peer"}), context=ctx, timeout=60) as r:
            wheel.write_bytes(r.read(10_000_000))
    assert hashlib.sha256(wheel.read_bytes()).hexdigest() == digest, "pinned SHA-256 mismatch: " + name
    paths.append(str(wheel))
env = root / "venv"
if not (env / "bin" / "python").exists():
    venv.EnvBuilder(with_pip=True).create(env)
python = str(env / "bin" / "python")
subprocess.run([python, "-m", "pip", "install", "--quiet", "--disable-pip-version-check", "--no-deps", *paths], check=True, timeout=900)
subprocess.run([python, "-c", "import importlib.metadata as m; assert (m.version('pydicom'), m.version('pynetdicom')) == ('3.0.2', '3.0.4')"], check=True, timeout=30)
print("export NETGET_DICOM_PYTHON=" + python)
