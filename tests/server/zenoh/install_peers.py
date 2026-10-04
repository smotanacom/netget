"""Pinned, unchanged zenoh-pico 1.10.1 (EPL-2.0/Apache-2.0, C) built into an owned ROOT from its
hash-pinned release tarball with its examples (z_pub, z_sub, z_get, z_queryable): an
implementation of Zenoh independent of the Rust runtime NetGet uses. Needs cmake and a C
compiler.

Usage: python3 install_peers.py /absolute/owned/root
"""
import hashlib, os, pathlib, ssl, subprocess, sys, tarfile, urllib.request

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
URL = "https://github.com/eclipse-zenoh/zenoh-pico/archive/refs/tags/1.10.1.tar.gz"
DIGEST = "eba95fa8c95c7972a7c12e7173ece23092ce5c376c8d749748f0e152526622df"
ctx = ssl.create_default_context(cafile="/etc/ssl/cert.pem" if sys.platform == "darwin" else None)
archive = root / "zenoh-pico-1.10.1.tar.gz"
if not archive.exists():
    with urllib.request.urlopen(urllib.request.Request(URL, headers={"User-Agent": "netget-zenoh-peer"}), context=ctx, timeout=120) as r:
        archive.write_bytes(r.read(30_000_000))
assert hashlib.sha256(archive.read_bytes()).hexdigest() == DIGEST, "pinned SHA-256 mismatch"
src = root / "zenoh-pico"
if not src.exists():
    with tarfile.open(archive) as t:
        top = t.getnames()[0].split("/")[0]
        t.extractall(root, filter="data")
    (root / top).rename(src)
build = src / "build"
examples = build / "examples"
if not (examples / "z_queryable").exists():
    build.mkdir(exist_ok=True)
    for cmd in (["cmake", "..", "-DBUILD_EXAMPLES=ON", "-DBUILD_TESTING=OFF", "-DCMAKE_BUILD_TYPE=Release"],
                ["make", f"-j{os.cpu_count() or 4}"]):
        r = subprocess.run(cmd, cwd=build, capture_output=True, text=True, timeout=1800)
        if r.returncode != 0:
            sys.exit(f"{' '.join(cmd)} failed:\n{r.stdout[-4000:]}\n{r.stderr[-4000:]}")
for name in ("z_pub", "z_sub", "z_get", "z_queryable"):
    assert (examples / name).exists(), name
print("export NETGET_ZENOH_PICO=" + str(examples))
