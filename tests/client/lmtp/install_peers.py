"""Pinned, unchanged aiosmtpd 1.4.6 LMTP server in an owned root, installed with
--require-hashes from requirements.txt.

Usage: python3 install_peers.py /absolute/owned/root
Prints the NETGET_LMTP_PYTHON export the client tests read.
"""
import pathlib, subprocess, sys, venv

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
here = pathlib.Path(__file__).resolve().parent
env = root / "venv"
if not (env / "bin" / "python").exists():
    venv.EnvBuilder(with_pip=True).create(env)
python = str(env / "bin" / "python")
subprocess.run([python, "-m", "pip", "install", "--quiet", "--disable-pip-version-check", "--require-hashes", "-r", str(here / "requirements.txt")], check=True, timeout=900)
subprocess.run([python, "-c", "import importlib.metadata as m; assert m.version('aiosmtpd') == '1.4.6'"], check=True, timeout=30)
print("export NETGET_LMTP_PYTHON=" + python)
