"""Install the RTSP client test's server into an owned ROOT: mediamtx 1.9.3. The release
binary is downloaded from GitHub (SHA-256 checked); where GitHub is unreachable, the same
release is built from the Go module proxy (`go mod download`, its own `go generate`, `go
build`). ffmpeg (the publisher) comes from the system.

Usage: python3 install_peers.py /absolute/owned/root
Prints the NETGET_MEDIAMTX export the test reads.
"""
import hashlib, json, os, pathlib, shutil, subprocess, sys, tarfile, urllib.request

VERSION = "v1.9.3"
URL = f"https://github.com/bluenviron/mediamtx/releases/download/{VERSION}/mediamtx_{VERSION}_linux_amd64.tar.gz"

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
binary = root / "mediamtx"
if not binary.exists():
    try:
        with urllib.request.urlopen(URL, timeout=300) as r:
            data = r.read()
        sums = urllib.request.urlopen(URL.replace(".tar.gz", ".tar.gz.sha256sum"), timeout=60).read().decode()
        if hashlib.sha256(data).hexdigest() != sums.split()[0]:
            sys.exit(f"{URL}: checksum mismatch")
        archive = root / "mediamtx.tar.gz"
        archive.write_bytes(data)
        with tarfile.open(archive) as t:
            t.extract("mediamtx", root, filter="data")
        archive.unlink()
    except Exception as e:  # no GitHub: build the same release from the module proxy
        print(f"release download failed ({e}); building {VERSION} from source", file=sys.stderr)
        env = dict(os.environ, GOPATH=str(root / "gopath"), GOMODCACHE=str(root / "gopath" / "pkg" / "mod"))
        info = json.loads(subprocess.run(["go", "mod", "download", "-json", f"github.com/bluenviron/mediamtx@{VERSION}"],
                                         check=True, capture_output=True, env=env, timeout=900).stdout)
        src = root / "src"
        shutil.rmtree(src, ignore_errors=True)
        shutil.copytree(info["Dir"], src)
        subprocess.run(["chmod", "-R", "u+w", str(src)], check=True)
        subprocess.run(["go", "generate", "./internal/core", "./internal/servers/hls"], cwd=src, check=True, env=env, timeout=600)
        subprocess.run(["go", "build", "-o", str(binary), "."], cwd=src, check=True, env=env, timeout=1800)
print("export NETGET_MEDIAMTX=" + str(binary))
