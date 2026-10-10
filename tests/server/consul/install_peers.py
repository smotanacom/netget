"""Pinned, unchanged Consul peers in an owned ROOT, used in both roles:
- the official consul 1.20.2 binary from releases.hashicorp.com (sha256 as its SHA256SUMS
  publishes it): its CLI is a Go api client of NetGet's agent, and `consul agent -dev` is the
  real agent NetGet's client talks to;
- py-consul 1.7.1, installed with --require-hashes, which runs py_consul_peer.py here.
Linux amd64 only (the consul binary). Needs Python >= 3.12.

Usage: python3 install_peers.py /absolute/owned/root
Prints the NETGET_CONSUL_BIN and NETGET_CONSUL_PYTHON exports the tests read.
"""
import hashlib, io, pathlib, shutil, subprocess, sys, urllib.request, venv, zipfile

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
here = pathlib.Path(__file__).resolve().parent
URL = "https://releases.hashicorp.com/consul/1.20.2/consul_1.20.2_linux_amd64.zip"
SHA = "1bf7ddf332f02e6e36082b0fdf6c3e8ce12a391e7ec7dafd3237bb12766a7fd5"
binary = root / "consul"
if not binary.exists():
    with urllib.request.urlopen(URL, timeout=600) as r:
        data = r.read()
    got = hashlib.sha256(data).hexdigest()
    assert got == SHA, f"{URL}: sha256 {got}, expected {SHA}"
    with zipfile.ZipFile(io.BytesIO(data)) as z, z.open("consul") as src, open(binary, "wb") as dst:
        shutil.copyfileobj(src, dst)
    binary.chmod(0o755)
subprocess.run([str(binary), "version"], check=True, timeout=60, stdout=subprocess.DEVNULL)
env = root / "venv"
if not (env / "bin" / "python").exists():
    venv.EnvBuilder(with_pip=True).create(env)
python = str(env / "bin" / "python")
subprocess.run([python, "-m", "pip", "install", "--quiet", "--disable-pip-version-check", "--require-hashes", "-r", str(here / "requirements.txt")], check=True, timeout=900)
subprocess.run([python, "-c", "import consul"], check=True, timeout=60)
print("export NETGET_CONSUL_BIN=" + str(binary))
print("export NETGET_CONSUL_PYTHON=" + python)
