"""Pinned, unchanged AMQP 1.0 peers in an owned ROOT:
- rhea 3.0.5 (Apache-2.0, JavaScript), installed with `npm ci` from js/package-lock.json, whose
  integrity hashes pin every package; it is both a client and the broker for NetGet's client.
  Needs Node >= 18.
- go-amqp v1.7.0 (MIT, Go): `goamqp/` is a small program on its public API; go.sum pins every
  module and `-mod=readonly` refuses anything else. Needs Go >= 1.25.

Usage: python3 install_peers.py /absolute/owned/root   (Python >= 3.10)
"""
import os, pathlib, shutil, subprocess, sys

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
assert sys.version_info >= (3, 10), "Python >= 3.10 required"
here = pathlib.Path(__file__).resolve().parent
js = root / "js"
js.mkdir(exist_ok=True)
for f in ("package.json", "package-lock.json"):
    shutil.copy(here / "js" / f, js / f)
subprocess.run(["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"], cwd=js, check=True, timeout=900)
env = dict(os.environ, GOFLAGS="-mod=readonly", CGO_ENABLED="0")
subprocess.run(["go", "build", "-o", str(root / "goamqp"), "."], cwd=here / "goamqp", env=env, check=True, timeout=900)
print("export NETGET_AMQP1_NODE_MODULES=" + str(js / "node_modules"))
print("export NETGET_AMQP1_GOAMQP=" + str(root / "goamqp"))
