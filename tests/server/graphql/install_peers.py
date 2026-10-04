"""Pinned, unchanged GraphQL peers in an owned venv: gql 4.4.0 (client, with its requests
transport) and strawberry-graphql 0.330.2 (server, its asgi extra under uvicorn 0.34.0). Both MIT.

Usage: python3 install_peers.py /absolute/owned/root   (Python >= 3.10)
"""
import hashlib, pathlib, ssl, subprocess, sys, urllib.request, venv

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
assert sys.version_info >= (3, 10), "Python >= 3.10 required"
WHEELS = [
    ("gql-4.4.0-py3-none-any.whl",
     "https://files.pythonhosted.org/packages/75/c4/3c28d6e7d73072d1d94f4a4162f5ef6031f7b5a6fd420841fb4b78e8b9f7/",
     "150ffd909a8814f14c9ca19ec9410afd572443253789d57e7efd6b92cc24d8f7"),
    ("strawberry_graphql-0.330.2-py3-none-any.whl",
     "https://files.pythonhosted.org/packages/eb/86/e97d917eecef6c78b09cd7f34624cd8ed2d5cfc073b8bfe626daf2f44f68/",
     "3f332b7d54d6f7c10030e66786add793cdd632f63d451d587163906dc7f97648"),
]
ctx = ssl.create_default_context(cafile="/etc/ssl/cert.pem" if sys.platform == "darwin" else None)
paths = []
for name, base, digest in WHEELS:
    wheel = root / name
    if not wheel.exists():
        with urllib.request.urlopen(urllib.request.Request(base + name, headers={"User-Agent": "netget-graphql-peer"}), context=ctx, timeout=60) as r:
            wheel.write_bytes(r.read(4_000_000))
    assert hashlib.sha256(wheel.read_bytes()).hexdigest() == digest, f"pinned SHA-256 mismatch for {name}"
    paths.append(wheel)
env = root / "venv"
if not (env / "bin" / "python").exists():
    venv.EnvBuilder(with_pip=True).create(env)
python = str(env / "bin" / "python")
subprocess.run([python, "-m", "pip", "install", "--quiet", "--disable-pip-version-check",
                f"{paths[0]}[requests]", f"{paths[1]}[asgi]", "uvicorn==0.34.0"], check=True, timeout=900)
subprocess.run([python, "-c", "import importlib.metadata as m; assert m.version('gql') == '4.4.0' and m.version('strawberry-graphql') == '0.330.2'"], check=True, timeout=30)
print("export NETGET_GRAPHQL_PYTHON=" + python)
