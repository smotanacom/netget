"""Pinned, unchanged FIX peer in an owned ROOT: QuickFIX/Go v0.9.12 (Apache-style QuickFIX
licence). `qfgo/` is a small program on its public API; go.sum pins every module and
`-mod=readonly` refuses anything else. The FIX44.xml data dictionary it validates against is
copied from the verified module. Needs Go >= 1.25.

Usage: python3 install_peers.py /absolute/owned/root   (Python >= 3.10)
"""
import os, pathlib, shutil, subprocess, sys

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
assert sys.version_info >= (3, 10), "Python >= 3.10 required"
src = pathlib.Path(__file__).resolve().parent / "qfgo"
env = dict(os.environ, GOFLAGS="-mod=readonly", CGO_ENABLED="0")
subprocess.run(["go", "build", "-o", str(root / "qfgo"), "."], cwd=src, env=env, check=True, timeout=900)
module = subprocess.run(["go", "list", "-m", "-f", "{{.Dir}}", "github.com/quickfixgo/quickfix"], cwd=src, env=env,
                        check=True, timeout=300, capture_output=True, text=True).stdout.strip()
shutil.copyfile(pathlib.Path(module) / "spec" / "FIX44.xml", root / "FIX44.xml")
print("export NETGET_FIX_QFGO=" + str(root / "qfgo"))
print("export NETGET_FIX_DICTIONARY=" + str(root / "FIX44.xml"))
