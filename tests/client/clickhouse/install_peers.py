"""Pinned, unchanged ClickHouse peers in an owned ROOT:
- the official ClickHouse 24.8.14.39 LTS static build (Apache-2.0), its one binary serving as
  `clickhouse client` and `clickhouse server`, from the release tarball verified by SHA-256;
- Python clickhouse-driver 0.2.11 (MIT), with pytz and tzlocal, installed with
  --require-hashes (wheels for CPython 3.12 and 3.13 on x86_64 Linux).

Usage: python3 install_peers.py /absolute/owned/root
Prints the NETGET_CLICKHOUSE_BIN and NETGET_CLICKHOUSE_PYTHON exports the tests read.
"""
import hashlib, os, pathlib, ssl, subprocess, sys, tarfile, urllib.request, venv

VERSION = "24.8.14.39"
URL = f"https://github.com/ClickHouse/ClickHouse/releases/download/v{VERSION}-lts/clickhouse-common-static-{VERSION}-amd64.tgz"
SHA256 = "78609e9a081a3be5e61a688785684554182c1d5c8915f415747d3371e26e58fd"

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
here = pathlib.Path(__file__).resolve().parent
binary = root / "clickhouse"
if not binary.exists():
    archive = root / "clickhouse.tgz"
    if not archive.exists() or hashlib.sha256(archive.read_bytes()).hexdigest() != SHA256:
        with urllib.request.urlopen(urllib.request.Request(URL, headers={"User-Agent": "netget-clickhouse-peer"}), context=ssl.create_default_context(), timeout=600) as r:
            archive.write_bytes(r.read())
    assert hashlib.sha256(archive.read_bytes()).hexdigest() == SHA256, "pinned SHA-256 mismatch for the ClickHouse release"
    with tarfile.open(archive) as t:
        member = t.getmember(f"clickhouse-common-static-{VERSION}/usr/bin/clickhouse")
        with t.extractfile(member) as src:
            binary.write_bytes(src.read())
    binary.chmod(0o755)
    archive.unlink()
subprocess.run([str(binary), "--version"], check=True, timeout=60)
env = root / "venv"
if not (env / "bin" / "python").exists():
    venv.EnvBuilder(with_pip=True).create(env)
python = str(env / "bin" / "python")
subprocess.run([python, "-m", "pip", "install", "--quiet", "--disable-pip-version-check", "--require-hashes", "-r", str(here / "requirements.txt")], check=True, timeout=900)
subprocess.run([python, "-c", "import importlib.metadata as m; assert m.version('clickhouse-driver') == '0.2.11'"], check=True, timeout=30)
print("export NETGET_CLICKHOUSE_BIN=" + str(binary))
print("export NETGET_CLICKHOUSE_PYTHON=" + python)
