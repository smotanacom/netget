"""Pinned, unchanged Minecraft peers in an owned ROOT:
- mcstatus 14.2.0 (Python, MIT) with its two dependencies, installed with --require-hashes;
- node-minecraft-protocol 1.68.0 (JavaScript, BSD-3-Clause), installed with `npm ci` from
  js/package-lock.json, whose integrity hashes pin every package. Needs Node >= 18 on PATH.

Usage: python3 install_peers.py /absolute/owned/root
Prints the NETGET_MINECRAFT_PYTHON, NETGET_MINECRAFT_NODE_MODULES and NETGET_MINECRAFT_PEER
exports the tests read.
"""
import pathlib, shutil, subprocess, sys, venv

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
here = pathlib.Path(__file__).resolve().parent
env = root / "venv"
if not (env / "bin" / "python").exists():
    venv.EnvBuilder(with_pip=True).create(env)
python = str(env / "bin" / "python")
subprocess.run([python, "-m", "pip", "install", "--quiet", "--disable-pip-version-check", "--require-hashes", "-r", str(here / "requirements.txt")], check=True, timeout=900)
subprocess.run([python, "-c", "import importlib.metadata as m; assert m.version('mcstatus') == '14.2.0'"], check=True, timeout=30)
js = root / "js"
js.mkdir(exist_ok=True)
for f in ("package.json", "package-lock.json", "peer.cjs"):
    shutil.copy(here / "js" / f, js / f)
subprocess.run(["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"], cwd=js, check=True, timeout=900)
subprocess.run(["node", "-e", "if (require('minecraft-protocol/package.json').version !== '1.68.0') process.exit(1)"], cwd=js, check=True, timeout=30)
print("export NETGET_MINECRAFT_PYTHON=" + python)
print("export NETGET_MINECRAFT_NODE_MODULES=" + str(js / "node_modules"))
print("export NETGET_MINECRAFT_PEER=" + str(js / "peer.cjs"))
