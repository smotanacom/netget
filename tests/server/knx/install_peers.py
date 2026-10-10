"""Pinned, unchanged KNX/IP peers in an owned ROOT, used in both roles:
- xknx 3.20.0 (Python), installed with --require-hashes, which runs xknx_peer.py here;
- knxd and knxtool, which must already be installed (`apt-get install knxd knxd-tools`):
  knxd is the tunnelling gateway for NetGet's client, and a tunnel client of NetGet's gateway
  (`-b ipt:`), driven by knxtool.
Needs Python >= 3.12.

Usage: python3 install_peers.py /absolute/owned/root
Prints the NETGET_KNX_PYTHON, NETGET_KNXD and NETGET_KNXTOOL exports the tests read.
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
subprocess.run([python, "-c", "from xknx.__version__ import __version__ as v; assert v == '3.20.0', v"], check=True, timeout=60)
knxd, knxtool = shutil.which("knxd"), shutil.which("knxtool")
assert knxd and knxtool, "knxd and knxtool are required: apt-get install knxd knxd-tools"
print("export NETGET_KNX_PYTHON=" + python)
print("export NETGET_KNXD=" + knxd)
print("export NETGET_KNXTOOL=" + knxtool)
