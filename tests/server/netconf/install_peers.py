"""Pinned, unchanged independent NETCONF peers in an owned ROOT.

ncclient 0.7.1 (Apache-2.0) is the client that drives NetGet's server; the netconf 2.1.0
package (Apache-2.0, with sshutil 1.5.0) is the server NetGet's client is pointed at. Both
run on Paramiko 3.5.1. The four peer wheels are fetched by URL and checked against their
PyPI SHA-256 before anything is installed; binary dependencies are installed at exact
versions. Nothing is patched, monkeypatched or installed globally.

Usage: python3.10 install_peers.py /absolute/owned/root
"""
import hashlib
import pathlib
import ssl
import subprocess
import sys
import urllib.request
import venv

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
assert sys.version_info[:2] == (3, 10), "Python 3.10 required: the calibrated netconf 2.1.0 server"
ctx = ssl.create_default_context(cafile="/etc/ssl/cert.pem" if sys.platform == "darwin" else None)

WHEELS = [
    ("ncclient-0.7.1-py3-none-any.whl", 94833, "47beeeee6074bd70a9215c4d353b51c7237af3c5c15269d81692810f2aa15147",
     "https://files.pythonhosted.org/packages/5b/7c/c0c06d1696b03b901fa5a854b03e16f35c5c5cc8f00a0f43217dc467cd39/ncclient-0.7.1-py3-none-any.whl"),
    ("netconf-2.1.0-py2.py3-none-any.whl", 31553, "bcc83f00f71323da5cb4a97a6542d6b20bd55ff783c1312fdc43342e319ce81a",
     "https://files.pythonhosted.org/packages/e4/5b/e267a72f10488465d9d91d004188298200f2fe9ba36cdd76e70447040fc1/netconf-2.1.0-py2.py3-none-any.whl"),
    ("sshutil-1.5.0-py2.py3-none-any.whl", 21152, "13e86e99da73ba94241ef0387e7a5fd2f6ad2c5c7a201dd09eaaa1e9db3550ac",
     "https://files.pythonhosted.org/packages/07/bf/eacada8abe4235a42a4c742acdf636c0a47b8257cdb2d1f01ec220cfa2f5/sshutil-1.5.0-py2.py3-none-any.whl"),
    ("paramiko-3.5.1-py3-none-any.whl", 227298, "43b9a0501fc2b5e70680388d9346cf252cfb7d00b0667c39e80eb43a408b8f61",
     "https://files.pythonhosted.org/packages/15/f8/c7bd0ef12954a81a1d3cea60a13946bd9a49a0036a5927770c461eade7ae/paramiko-3.5.1-py3-none-any.whl"),
]
DEPENDENCIES = ["lxml==5.3.1", "cryptography==43.0.3", "bcrypt==4.3.0", "PyNaCl==1.6.2", "monotonic==1.6"]

wheels = root / "wheels"
wheels.mkdir(exist_ok=True)
paths = []
for name, size, digest, url in WHEELS:
    path = wheels / name
    if not path.exists():
        request = urllib.request.Request(url, headers={"User-Agent": "netget-independent-netconf-peer"})
        with urllib.request.urlopen(request, context=ctx, timeout=60) as response:
            data = response.read(size + 1)
        path.write_bytes(data)
    data = path.read_bytes()
    assert len(data) == size, f"unexpected size for {name}"
    assert hashlib.sha256(data).hexdigest() == digest, f"pinned SHA-256 mismatch: {name}"
    paths.append(str(path))

env_dir = root / "venv"
if not (env_dir / "bin" / "python").exists():
    venv.EnvBuilder(with_pip=True, clear=False).create(env_dir)
python = str(env_dir / "bin" / "python")
pip = [python, "-m", "pip", "install", "--disable-pip-version-check", "--no-input", "--quiet"]
subprocess.run(pip + ["--only-binary=:all:"] + DEPENDENCIES, check=True, timeout=600)
subprocess.run(pip + ["--no-deps"] + paths, check=True, timeout=300)
subprocess.run(
    [python, "-c", "import importlib.metadata as m; import ncclient, netconf.server, paramiko;"
     "v={p: m.version(p) for p in ['ncclient','netconf','sshutil','paramiko','lxml']};"
     "assert v=={'ncclient':'0.7.1','netconf':'2.1.0','sshutil':'1.5.0','paramiko':'3.5.1','lxml':'5.3.1'}, v; print(v)"],
    check=True,
    timeout=30,
)
print("export NETGET_NETCONF_PYTHON=" + python)
