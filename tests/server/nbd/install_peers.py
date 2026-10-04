"""Pinned, unchanged NBD peers built into an owned ROOT from hash-pinned release tarballs
(download.libguestfs.org):

- libnbd 1.24.3 (LGPL-2.1+, C): nbdinfo, nbdcopy and nbddump, clients of NetGet's server.
- nbdkit 1.48.1 (BSD-3-Clause, C): the server with its data plugin and error filter, for
  NetGet's client. Only the server, that plugin and that filter are needed, so the build runs
  `make -k` and checks for those three (some unrelated plugins do not build on macOS).

Needs a C toolchain, pkg-config and GnuTLS headers (Homebrew gnutls, or libgnutls28-dev).

Usage: python3 install_peers.py /absolute/owned/root
"""
import hashlib, os, pathlib, ssl, subprocess, sys, tarfile, urllib.request

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
SOURCES = {
    "libnbd": ("https://download.libguestfs.org/libnbd/1.24-stable/libnbd-1.24.3.tar.gz",
               "da36e77aff952f818222a5a0e6c726b3d492405cb1934a9c81f8e4c0071bb26a"),
    "nbdkit": ("https://download.libguestfs.org/nbdkit/1.48-stable/nbdkit-1.48.1.tar.gz",
               "21ddcdd0bb3cd49ec47aa2d7ca3e69941426e991932b44dc34a856d063f41ead"),
}
ctx = ssl.create_default_context(cafile="/etc/ssl/cert.pem" if sys.platform == "darwin" else None)
trees = {}
for name, (url, digest) in SOURCES.items():
    archive = root / f"{name}.tar.gz"
    if not archive.exists():
        with urllib.request.urlopen(urllib.request.Request(url, headers={"User-Agent": "netget-nbd-peer"}), context=ctx, timeout=120) as r:
            archive.write_bytes(r.read(50_000_000))
    assert hashlib.sha256(archive.read_bytes()).hexdigest() == digest, "pinned SHA-256 mismatch: " + name
    dest = root / name
    if not dest.exists():
        with tarfile.open(archive) as t:
            top = t.getnames()[0].split("/")[0]
            t.extractall(root, filter="data")
        (root / top).rename(dest)
    trees[name] = dest

prefix = root / "inst"
def run(cmd, cwd, check=True):
    r = subprocess.run(cmd, cwd=cwd, timeout=3600, capture_output=True, text=True)
    if check and r.returncode != 0:
        sys.exit(f"{' '.join(cmd)} failed:\n{r.stdout[-4000:]}\n{r.stderr[-4000:]}")

if not (prefix / "bin" / "nbdinfo").exists():
    run(["./configure", f"--prefix={prefix}", "--disable-ocaml", "--disable-golang", "--disable-rust",
         "--disable-python", "--disable-fuse"], trees["libnbd"])
    run(["make", f"-j{os.cpu_count() or 4}"], trees["libnbd"])
    run(["make", "install"], trees["libnbd"])
nbdkit = trees["nbdkit"]
server = nbdkit / "server" / "nbdkit"
plugin = nbdkit / "plugins" / "data" / ".libs" / "nbdkit-data-plugin.so"
filt = nbdkit / "filters" / "error" / ".libs" / "nbdkit-error-filter.so"
if not (server.exists() and plugin.exists() and filt.exists()):
    run(["./configure", f"--prefix={prefix}", "--disable-ocaml", "--disable-golang", "--disable-rust",
         "--disable-python", "--disable-perl", "--disable-lua", "--disable-tcl", "--disable-ruby",
         "--without-libvirt", "--without-curl", "--without-ssh", "--without-iso", "--without-libguestfs",
         "--disable-vddk", "--without-ext2", "--without-libnbd", "--without-liblzma", "--without-zlib",
         "--without-libzstd", "--without-bzip2"], nbdkit)
    run(["make", "-k", f"-j{os.cpu_count() or 4}"], nbdkit, check=False)
for p in (server, plugin, filt):
    assert p.exists(), f"{p} was not built"
out = subprocess.run([str(prefix / "bin" / "nbdinfo"), "--version"], capture_output=True, text=True, timeout=30).stdout
assert "1.24.3" in out, out
print("export NETGET_NBD_NBDINFO=" + str(prefix / "bin" / "nbdinfo"))
print("export NETGET_NBD_NBDCOPY=" + str(prefix / "bin" / "nbdcopy"))
print("export NETGET_NBD_NBDKIT=" + str(server))
print("export NETGET_NBD_DATA_PLUGIN=" + str(plugin))
print("export NETGET_NBD_ERROR_FILTER=" + str(filt))
