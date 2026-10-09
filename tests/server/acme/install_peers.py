"""Pinned, unchanged ACME peers in an owned ROOT.

- lego v4.35.2 (MIT, Go) as a client of NetGet's CA, and Pebble v2.10.1 with
  pebble-challtestsrv (MPL-2.0, Go) as the CA and DNS for NetGet's client. `tools/go.mod`
  and `tools/go.sum` pin every module; `-mod=readonly` refuses anything else. Pebble's test
  certificates are copied from the verified module. Needs Go >= 1.25.
- certbot 5.8.0 and its acme library 5.8.0 (Apache-2.0, Python), hash-pinned wheels; their
  dependencies come from PyPI.

Usage: python3 install_peers.py /absolute/owned/root   (Python >= 3.10)
"""
import hashlib, os, pathlib, shutil, ssl, subprocess, sys, urllib.request, venv

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
assert sys.version_info >= (3, 10), "Python >= 3.10 required"
tools = pathlib.Path(__file__).resolve().parent / "tools"
env = dict(os.environ, GOFLAGS="-mod=readonly", CGO_ENABLED="0")
for name, package in (("lego", "github.com/go-acme/lego/v4/cmd/lego"),
                      ("pebble", "github.com/letsencrypt/pebble/v2/cmd/pebble"),
                      ("pebble-challtestsrv", "github.com/letsencrypt/pebble/v2/cmd/pebble-challtestsrv")):
    subprocess.run(["go", "build", "-o", str(root / name), package], cwd=tools, env=env, check=True, timeout=1800)
module = subprocess.run(["go", "list", "-m", "-f", "{{.Dir}}", "github.com/letsencrypt/pebble/v2"], cwd=tools, env=env,
                        check=True, timeout=300, capture_output=True, text=True).stdout.strip()
certs = root / "pebble-certs"
certs.mkdir(exist_ok=True)
for src, dst in (("test/certs/pebble.minica.pem", "minica.pem"), ("test/certs/localhost/cert.pem", "cert.pem"),
                 ("test/certs/localhost/key.pem", "key.pem")):
    shutil.copyfile(pathlib.Path(module) / src, certs / dst)

ctx = ssl.create_default_context(cafile="/etc/ssl/cert.pem" if sys.platform == "darwin" else None)
WHEELS = [
    ("acme-5.8.0-py3-none-any.whl", "https://files.pythonhosted.org/packages/73/f8/4e40a19169b540aca57e5d1f260dfb184a2cf27c8a3c5165a28bcba975d9/", "637501767248156545d85c23b806381ac346c5eac759f8e35052af85b7bb3933"),
    ("certbot-5.8.0-py3-none-any.whl", "https://files.pythonhosted.org/packages/71/a9/7742882b276c06e430fb23da13e4512b3cd89d4aa6fb9ddfd7b96812bbff/", "c06793e6a0169b07ee09e11e6a017c7d5a77310055d693fffd66c53363ab87ff"),
]
paths = []
for name, base, digest in WHEELS:
    wheel = root / name
    if not wheel.exists():
        with urllib.request.urlopen(urllib.request.Request(base + name, headers={"User-Agent": "netget-acme-peer"}), context=ctx, timeout=60) as r:
            wheel.write_bytes(r.read(5_000_000))
    assert hashlib.sha256(wheel.read_bytes()).hexdigest() == digest, "pinned SHA-256 mismatch: " + name
    paths.append(str(wheel))
venv_dir = root / "venv"
if not (venv_dir / "bin" / "python").exists():
    venv.EnvBuilder(with_pip=True).create(venv_dir)
python = str(venv_dir / "bin" / "python")
subprocess.run([python, "-m", "pip", "install", "--quiet", "--disable-pip-version-check", *paths], check=True, timeout=900)
subprocess.run([python, "-c", "import importlib.metadata as m; assert (m.version('certbot'), m.version('acme')) == ('5.8.0', '5.8.0')"], check=True, timeout=30)
out = subprocess.run([str(root / "lego"), "--version"], check=True, timeout=30, capture_output=True, text=True).stdout
assert "v4.35.2" in out, out
print("export NETGET_ACME_LEGO=" + str(root / "lego"))
print("export NETGET_ACME_PEBBLE=" + str(root / "pebble"))
print("export NETGET_ACME_CHALLTESTSRV=" + str(root / "pebble-challtestsrv"))
print("export NETGET_ACME_PEBBLE_CERTS=" + str(certs))
print("export NETGET_ACME_CERTBOT=" + str(venv_dir / "bin" / "certbot"))
