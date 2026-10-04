"""Pinned, unchanged RTMP peer in an owned ROOT: MediaMTX v1.21.1 (MIT, Go, its own RTMP stack),
the official release binary for this platform, hash-pinned against the release's
checksums.sha256. FFmpeg (its native RTMP) comes from the system: `brew install ffmpeg` or
`sudo apt-get install ffmpeg`.

Usage: python3 install_peers.py /absolute/owned/root   (Python >= 3.10)
"""
import hashlib, pathlib, platform, ssl, sys, tarfile, urllib.request

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
assert sys.version_info >= (3, 10), "Python >= 3.10 required"
RELEASES = {
    ("Darwin", "arm64"): ("mediamtx_v1.21.1_darwin_arm64.tar.gz", "25e20ed41611f1f3103b8359585210b29b11b69fa0d9e11bd11b92f7bbcb42ef"),
    ("Linux", "x86_64"): ("mediamtx_v1.21.1_linux_amd64.tar.gz", "653abc672a3e693f8d3b2717752492fdcfb8072291ec108d03d3dd857411b0ee"),
}
key = (platform.system(), platform.machine())
assert key in RELEASES, f"no pinned MediaMTX release for {key}"
name, digest = RELEASES[key]
tarball = root / name
if not tarball.exists():
    ctx = ssl.create_default_context(cafile="/etc/ssl/cert.pem" if sys.platform == "darwin" else None)
    url = "https://github.com/bluenviron/mediamtx/releases/download/v1.21.1/" + name
    with urllib.request.urlopen(urllib.request.Request(url, headers={"User-Agent": "netget-rtmp-peer"}), context=ctx, timeout=120) as r:
        data = r.read(100_000_001)
    assert len(data) <= 100_000_000, "bounded download"
    tarball.write_bytes(data)
assert hashlib.sha256(tarball.read_bytes()).hexdigest() == digest, "pinned SHA-256 mismatch: " + name
with tarfile.open(tarball) as t:
    member = t.getmember("mediamtx")
    assert member.isfile()
    (root / "mediamtx").write_bytes(t.extractfile(member).read())
(root / "mediamtx").chmod(0o755)
print("export NETGET_RTMP_MEDIAMTX=" + str(root / "mediamtx"))
