"""Pinned, unchanged Redfish peers in an owned ROOT.

- gofish v0.26.0 (Apache-2.0, Go): `gofish/` is a small program using its public API; go.sum
  pins the module hash and `-mod=readonly` refuses anything else. Needs Go >= 1.22.
- DMTF redfishtool 1.1.8 (BSD-3, Python CLI), hash-pinned wheel.
- DMTF Redfish-Mockup-Server 1.3.0 (BSD-3) with its bundled public-rackmount1 mockup,
  hash-pinned release tarball, run unchanged by NetGet's client tests.

Usage: python3 install_peers.py /absolute/owned/root   (Python >= 3.10)
"""
import hashlib, os, pathlib, ssl, subprocess, sys, tarfile, urllib.request, venv

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
assert sys.version_info >= (3, 10), "Python >= 3.10 required"
here = pathlib.Path(__file__).resolve().parent
ctx = ssl.create_default_context(cafile="/etc/ssl/cert.pem" if sys.platform == "darwin" else None)


def fetch(name, url, digest, limit):
    path = root / name
    if not path.exists():
        with urllib.request.urlopen(urllib.request.Request(url, headers={"User-Agent": "netget-redfish-peer"}), context=ctx, timeout=60) as r:
            data = r.read(limit + 1)
        assert len(data) <= limit, "bounded download"
        path.write_bytes(data)
    assert hashlib.sha256(path.read_bytes()).hexdigest() == digest, "pinned SHA-256 mismatch: " + name
    return path


gofish = root / "gofish-peer"
env = dict(os.environ, GOFLAGS="-mod=readonly", CGO_ENABLED="0")
subprocess.run(["go", "build", "-o", str(gofish), "."], cwd=here / "gofish", env=env, check=True, timeout=900)

wheel = fetch("redfishtool-1.1.8-py3-none-any.whl",
              "https://files.pythonhosted.org/packages/py3/r/redfishtool/redfishtool-1.1.8-py3-none-any.whl",
              "23c9e4975bc6b2ec4c8772ebae2d51e068e2101d30ddf4a68c7ad0cabca73783", 1_000_000)
tarball = fetch("Redfish-Mockup-Server-1.3.0.tar.gz",
                "https://github.com/DMTF/Redfish-Mockup-Server/archive/refs/tags/1.3.0.tar.gz",
                "71aea04e58580d2c90a94e28cc8a94c6a455050a762f636fd9935ead3f5b96c6", 20_000_000)
mockup = root / "Redfish-Mockup-Server-1.3.0"
if not mockup.exists():
    with tarfile.open(tarball) as t:
        for m in t.getmembers():
            p = pathlib.PurePosixPath(m.name)
            assert not p.is_absolute() and ".." not in p.parts and not m.issym() and not m.islnk()
        t.extractall(root)
venv_dir = root / "venv"
if not (venv_dir / "bin" / "python").exists():
    venv.EnvBuilder(with_pip=True).create(venv_dir)
python = str(venv_dir / "bin" / "python")
subprocess.run([python, "-m", "pip", "install", "--quiet", "--disable-pip-version-check", str(wheel),
                "requests==2.32.3", "python-dateutil==2.9.0.post0", "grequests==0.7.0", "multipart==1.2.1"],
               check=True, timeout=900)
subprocess.run([python, "-c", "import importlib.metadata as m; assert m.version('redfishtool') == '1.1.8'"], check=True, timeout=30)
print("export NETGET_REDFISH_GOFISH=" + str(gofish))
print("export NETGET_REDFISH_PYTHON=" + python)
print("export NETGET_REDFISH_TOOL=" + str(venv_dir / "bin" / "redfishtool"))
print("export NETGET_REDFISH_MOCKUP=" + str(mockup))
