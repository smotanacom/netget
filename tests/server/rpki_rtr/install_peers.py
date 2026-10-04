"""Pinned, unchanged independent RPKI-RTR peers in an owned ROOT.

StayRTR 0.6.4 (BSD-3-Clause, Go): `stayrtr` is the cache NetGet's router talks to and
`rtrdump` a router that queries NetGet's cache. RTRlib 0.8.0 (MIT, C): `rtrclient` is a second,
independent router. Sources are fetched by URL and checked against pinned SHA-256 before
building; StayRTR's Go dependencies are verified by its own go.sum. No source is patched.

Requires Go >= 1.24, cmake and a C compiler.
Usage: python3 install_peers.py /absolute/owned/root
"""
import hashlib
import os
import pathlib
import shutil
import ssl
import subprocess
import sys
import tarfile
import urllib.request
import zipfile

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
ctx = ssl.create_default_context(cafile="/etc/ssl/cert.pem" if sys.platform == "darwin" else None)


def fetch(name, url, digest, limit):
    path = root / name
    if not path.exists():
        request = urllib.request.Request(url, headers={"User-Agent": "netget-independent-rpki-rtr-peer"})
        with urllib.request.urlopen(request, context=ctx, timeout=120) as response:
            data = response.read(limit + 1)
        assert len(data) <= limit, "bounded peer download"
        path.write_bytes(data)
    assert hashlib.sha256(path.read_bytes()).hexdigest() == digest, "pinned SHA-256 mismatch: " + name
    return path


stay_zip = fetch(
    "stayrtr-v0.6.4.zip",
    "https://proxy.golang.org/github.com/bgp/stayrtr/@v/v0.6.4.zip",
    "4c22b7aa023858fc03997d41a611da92095d215becdbed1d8e4f7ed91aa23241",
    4 * 1024 * 1024,
)
rtrlib_tar = fetch(
    "rtrlib-0.8.0.tar.gz",
    "https://github.com/rtrlib/rtrlib/archive/refs/tags/v0.8.0.tar.gz",
    "8cc99343dc3ea8908cd9710ba1f72a1ddce591bf80bfd7d656dbc4568f560ada",
    2 * 1024 * 1024,
)

source = root / "stayrtr-src"
module = source / "github.com/bgp/stayrtr@v0.6.4"
if not module.exists():
    with zipfile.ZipFile(stay_zip) as archive:
        for entry in archive.infolist():
            assert entry.filename.startswith("github.com/bgp/stayrtr@v0.6.4/") and ".." not in entry.filename
        archive.extractall(source)
    for path in module.rglob("*"):
        path.chmod(path.stat().st_mode | 0o200)
assert b"Redistribution and use in source and binary forms" in (module / "LICENSE.txt").read_bytes()  # BSD-3-Clause
go = shutil.which("go")
assert go, "Go >= 1.24 required"
env = os.environ.copy()
env.update(GOPATH=str(root / "go-path"), GOMODCACHE=str(root / "go-modules"), GOCACHE=str(root / "go-cache"), GOFLAGS="-mod=readonly", GOTOOLCHAIN="local")
bin_dir = root / "bin"
bin_dir.mkdir(exist_ok=True)
subprocess.run([go, "build", "-o", str(bin_dir) + "/", "./cmd/stayrtr", "./cmd/rtrdump"], cwd=module, env=env, check=True, timeout=900)

rtrlib = root / "rtrlib-src"
if not rtrlib.exists():
    with tarfile.open(rtrlib_tar) as archive:
        for member in archive.getmembers():
            assert member.name.startswith("rtrlib-0.8.0") and ".." not in member.name
        archive.extractall(rtrlib)
src = rtrlib / "rtrlib-0.8.0"
assert b"MIT" in (src / "LICENSE").read_bytes()
build = root / "rtrlib-build"
cmake = shutil.which("cmake")
assert cmake, "cmake required"
# CMake 4 refuses the project's old cmake_minimum_required without this policy floor.
subprocess.run(
    [cmake, "-S", str(src), "-B", str(build), "-DRTRLIB_TRANSPORT_SSH=No", "-DCMAKE_BUILD_TYPE=Release", "-DUNIT_TESTING=No", "-DCMAKE_POLICY_VERSION_MINIMUM=3.5"],
    check=True,
    timeout=300,
)
subprocess.run([cmake, "--build", str(build), "--target", "rtrclient", "-j4"], check=True, timeout=900)
rtrclient = build / "tools" / "rtrclient"
assert rtrclient.exists()
version = subprocess.run([str(bin_dir / "stayrtr"), "-version"], capture_output=True, text=True, timeout=10).stdout.strip()
assert "0.6.4" in version, version
print("export NETGET_STAYRTR=" + str(bin_dir / "stayrtr"))
print("export NETGET_RTRDUMP=" + str(bin_dir / "rtrdump"))
print("export NETGET_RTRCLIENT=" + str(rtrclient))
