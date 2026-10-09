"""Pinned, unchanged CalDAV/CardDAV peers in an owned venv: python caldav 3.3.1 (Apache-2.0 /
GPL-3) and vdirsyncer 0.21.0 (BSD-3) as clients of NetGet's servers, Radicale 3.8.1 (GPL-3) as
the server for NetGet's clients. The three wheels are hash-pinned; their dependencies come from
PyPI. Shared by tests/server/carddav.

Usage: python3 install_peers.py /absolute/owned/root   (Python >= 3.10)
"""
import hashlib, pathlib, ssl, subprocess, sys, urllib.request, venv

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
assert sys.version_info >= (3, 10), "Python >= 3.10 required"
WHEELS = [
    ("caldav-3.3.1-py3-none-any.whl", "https://files.pythonhosted.org/packages/59/7b/1a80286115026c556a750f93bbb9c8a79ac84e9702687623075d596c8f35/", "f469371d30a5902a71ba383d4b1b030ef72053b882a351b285cb3e430babdcda"),
    ("vdirsyncer-0.21.0-py3-none-any.whl", "https://files.pythonhosted.org/packages/d0/4e/7cd98d24a9ac5673ab63bfabdfea11b6d7ff1825227ad88813942e027fd1/", "0f7320b0872c7ed804d146454107247f19f9e832815492aeb64fa8d2d09f4b6e"),
    ("radicale-3.8.1-py3-none-any.whl", "https://files.pythonhosted.org/packages/3d/c5/e370d69abd9f860656c6f580ac3883f0dcd4b4c3c17a8f00d89dea3d3691/", "12201ff48e2e3347b855d9846f5f446b8b88c177e5d1e5f925bacf46688bfb18"),
]
ctx = ssl.create_default_context(cafile="/etc/ssl/cert.pem" if sys.platform == "darwin" else None)
paths = []
for name, base, digest in WHEELS:
    wheel = root / name
    if not wheel.exists():
        with urllib.request.urlopen(urllib.request.Request(base + name, headers={"User-Agent": "netget-dav-peer"}), context=ctx, timeout=60) as r:
            wheel.write_bytes(r.read(5_000_000))
    assert hashlib.sha256(wheel.read_bytes()).hexdigest() == digest, "pinned SHA-256 mismatch: " + name
    paths.append(str(wheel))
env = root / "venv"
if not (env / "bin" / "python").exists():
    venv.EnvBuilder(with_pip=True).create(env)
python = str(env / "bin" / "python")
subprocess.run([python, "-m", "pip", "install", "--quiet", "--disable-pip-version-check", *paths], check=True, timeout=900)
subprocess.run([python, "-c", "import importlib.metadata as m; assert (m.version('caldav'), m.version('vdirsyncer'), m.version('radicale')) == ('3.3.1', '0.21.0', '3.8.1')"], check=True, timeout=30)
print("export NETGET_DAV_BIN=" + str(env / "bin"))
