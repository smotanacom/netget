"""Pinned, unchanged Socket.IO peers in an owned ROOT:
- python-socketio 5.17.0 and python-engineio 4.14.0 (MIT), hash-pinned wheels, with the sync
  client's transports (requests, websocket-client) and uvicorn 0.34.0 for its ASGI server;
- the reference socket.io-client 4.8.4 (MIT, JavaScript), installed with `npm ci` from
  js/package-lock.json, whose integrity hashes pin every package. Needs Node >= 18.

Usage: python3 install_peers.py /absolute/owned/root   (Python >= 3.10)
"""
import hashlib, pathlib, shutil, ssl, subprocess, sys, urllib.request, venv

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
assert sys.version_info >= (3, 10), "Python >= 3.10 required"
here = pathlib.Path(__file__).resolve().parent
WHEELS = [
    ("python_socketio-5.17.0-py3-none-any.whl", "https://files.pythonhosted.org/packages/ee/be/44b558c944bc16618483967ecd3424c578705aa33ceee7df8c1e4ab43ea0/", "b5826fd2f8aa02e11347816349b74ac6b53e8a4f4e4b1cf1388e1aff19b7f3f4"),
    ("python_engineio-4.14.0-py3-none-any.whl", "https://files.pythonhosted.org/packages/e5/de/07cfd386974c2a26a7bde41f2111be29bbfc92b9ea0bb76694415a4a1a78/", "9f0fe275fb7d67bfc1a632421adf22949fd4843bd9c458c004b0a89cede302a2"),
]
ctx = ssl.create_default_context(cafile="/etc/ssl/cert.pem" if sys.platform == "darwin" else None)
paths = []
for name, base, digest in WHEELS:
    wheel = root / name
    if not wheel.exists():
        with urllib.request.urlopen(urllib.request.Request(base + name, headers={"User-Agent": "netget-socketio-peer"}), context=ctx, timeout=60) as r:
            wheel.write_bytes(r.read(2_000_000))
    assert hashlib.sha256(wheel.read_bytes()).hexdigest() == digest, "pinned SHA-256 mismatch: " + name
    paths.append(str(wheel))
env = root / "venv"
if not (env / "bin" / "python").exists():
    venv.EnvBuilder(with_pip=True).create(env)
python = str(env / "bin" / "python")
subprocess.run([python, "-m", "pip", "install", "--quiet", "--disable-pip-version-check", *paths,
                "requests==2.32.3", "websocket-client==1.8.0", "uvicorn==0.34.0"], check=True, timeout=900)
subprocess.run([python, "-c", "import importlib.metadata as m; assert m.version('python-socketio') == '5.17.0' and m.version('python-engineio') == '4.14.0'"], check=True, timeout=30)
js = root / "js"
js.mkdir(exist_ok=True)
for f in ("package.json", "package-lock.json"):
    shutil.copy(here / "js" / f, js / f)
subprocess.run(["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"], cwd=js, check=True, timeout=900)
print("export NETGET_SOCKETIO_PYTHON=" + python)
print("export NETGET_SOCKETIO_NODE_MODULES=" + str(js / "node_modules"))
