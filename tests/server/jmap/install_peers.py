"""Pinned, unchanged JMAP peers in an owned root: the Stalwart 0.16.24 mail server release
binary (AGPL-3.0, SHA-256 pinned per platform) and jmapc 0.3.0 (MIT) with its dependencies,
installed with --require-hashes from requirements.txt beside this file.

Usage: python3 install_peers.py /absolute/owned/root
"""
import hashlib, pathlib, platform, ssl, subprocess, sys, tarfile, urllib.request, venv

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
here = pathlib.Path(__file__).resolve().parent
RELEASE = "https://github.com/stalwartlabs/stalwart/releases/download/v0.16.24/"
STALWART = {
    ("darwin", "arm64"): ("stalwart-aarch64-apple-darwin.tar.gz", "1af63fff943ce5d0840535460ad3df30f6f493e6ce6cfce6fd2e52ac7cc26cfc"),
    ("darwin", "x86_64"): ("stalwart-x86_64-apple-darwin.tar.gz", "3f21b0d59d77a2bb8541431e509ea1a0c2e38d71e913b7d24883487864e0449f"),
    ("linux", "x86_64"): ("stalwart-x86_64-unknown-linux-gnu.tar.gz", "51392691d4ab67864e84af5215277dbd4a069ffaf9a0ceb94a07462e22487843"),
    ("linux", "aarch64"): ("stalwart-aarch64-unknown-linux-gnu.tar.gz", "4e025971a6591d7a3fb81883c556d038fa246ee8164ebda0a2fa682e5d317650"),
}
key = (sys.platform, platform.machine())
assert key in STALWART, f"no pinned Stalwart for {key}; pinned: {sorted(STALWART)}"
name, digest = STALWART[key]
archive = root / name
ctx = ssl.create_default_context(cafile="/etc/ssl/cert.pem" if sys.platform == "darwin" else None)
if not archive.exists():
    with urllib.request.urlopen(urllib.request.Request(RELEASE + name, headers={"User-Agent": "netget-jmap-peer"}), context=ctx, timeout=300) as r:
        archive.write_bytes(r.read(200_000_000))
assert hashlib.sha256(archive.read_bytes()).hexdigest() == digest, "pinned SHA-256 mismatch: " + name
bindir = root / "bin"
bindir.mkdir(exist_ok=True)
with tarfile.open(archive) as t:
    member = t.getmember("stalwart")
    assert member.isfile()
    t.extract(member, bindir, filter="data")
binary = bindir / "stalwart"
binary.chmod(0o755)
version = subprocess.run([str(binary), "--version"], capture_output=True, text=True, timeout=30).stdout
assert "0.16.24" in version, version

env = root / "venv"
if not (env / "bin" / "python").exists():
    venv.EnvBuilder(with_pip=True).create(env)
python = str(env / "bin" / "python")
subprocess.run([python, "-m", "pip", "install", "--quiet", "--disable-pip-version-check", "--require-hashes", "-r", str(here / "requirements.txt")], check=True, timeout=900)
subprocess.run([python, "-c", "import importlib.metadata as m, jmapc; assert m.version('jmapc') == '0.3.0'"], check=True, timeout=30)
print("export NETGET_JMAP_STALWART=" + str(binary))
print("export NETGET_JMAP_PYTHON=" + python)
