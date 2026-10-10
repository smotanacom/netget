"""Install the Pulsar tests' peers into an owned ROOT:

- Apache Pulsar 4.0.6 (binary tarball, SHA-512 checked against the release's own checksum):
  `bin/pulsar standalone` for the client tests, `bin/pulsar-client` (the Java client) for the
  server tests. Needs Java 17+.
- The official Python client, pulsar-client 3.13.0 (a binding to the C++ library), in a venv.

Usage: python3 install_peers.py /absolute/owned/root
Prints the NETGET_PULSAR_HOME and NETGET_PULSAR_PYTHON exports the tests read.
"""
import hashlib, pathlib, subprocess, sys, tarfile, urllib.request

VERSION = "4.0.6"
URL = f"https://archive.apache.org/dist/pulsar/pulsar-{VERSION}/apache-pulsar-{VERSION}-bin.tar.gz"
SHA512 = ("ac174f604e94473215e07024d05853dbf24bf1520ec222c8bb0d87707a5f1267"
          "49df7d5733e06a2423456421489ed371e122f4f5a7793a5146a5a1b47f437e90")

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
home = root / f"apache-pulsar-{VERSION}"
if not (home / "bin" / "pulsar").exists():
    archive = root / "pulsar.tar.gz"
    h = hashlib.sha512()
    with urllib.request.urlopen(URL, timeout=900) as r, open(archive, "wb") as f:
        while chunk := r.read(1 << 20):
            h.update(chunk)
            f.write(chunk)
    if h.hexdigest() != SHA512:
        sys.exit(f"{URL}: sha512 {h.hexdigest()}, expected {SHA512}")
    with tarfile.open(archive) as t:
        t.extractall(root, filter="data")
    archive.unlink()
venv = root / "venv"
if not (venv / "bin" / "python").exists():
    subprocess.run([sys.executable, "-m", "venv", str(venv)], check=True, timeout=300)
subprocess.run([str(venv / "bin" / "pip"), "install", "--quiet", "pulsar-client==3.13.0"],
               check=True, timeout=900)
subprocess.run([str(venv / "bin" / "python"), "-c", "import pulsar"], check=True, timeout=60)
print("export NETGET_PULSAR_HOME=" + str(home))
print("export NETGET_PULSAR_PYTHON=" + str(venv / "bin" / "python"))
