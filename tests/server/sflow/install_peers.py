"""Install bounded pinned unmodified sFlow peers in an owned directory only."""
import hashlib
import io
import json
import os
import pathlib
import platform
import ssl
import subprocess
import sys
import tarfile
import urllib.request

ROOT = pathlib.Path(sys.argv[1]).resolve()
ROOT.mkdir(parents=True, exist_ok=True)
REVISION = "ed105e3cf9fb208505ed3a9939c9449321cbacf1"
SOURCE_SHA = "79dd1073b1df3fa5fb3c7c13f8ec9e23c6f700a51a3355a5f316b1a473ab6e8e"
CTX = ssl.create_default_context(cafile="/etc/ssl/cert.pem" if sys.platform == "darwin" else None)


def fetch(url, limit):
    request = urllib.request.Request(url, headers={"User-Agent": "netget-owned-independent-peer"})
    with urllib.request.urlopen(request, context=CTX, timeout=60) as response:
        data = response.read(limit + 1)
    assert len(data) <= limit, "peer download byte bound"
    return data


targets = {
    ("darwin", "arm64"): ("darwin-arm64", "6bc188842983edbf2df26788180bce3ee801788fec3aebedcff1f5d96eb9fd57"),
    ("linux", "x86_64"): ("linux-amd64", "63d3bb6c4e458f56ae3268eac74979da1fee0cea5342362295e7d587784af14b"),
}
assert (sys.platform, platform.machine()) in targets, "supported bootstrap: Linux amd64/macOS arm64"
target, digest = targets[(sys.platform, platform.machine())]
collector = ROOT / "goflow2-v2.2.7"
data = collector.read_bytes() if collector.exists() else fetch(
    f"https://github.com/netsampler/goflow2/releases/download/v2.2.7/goflow2-2.2.7-{target}", 26 * 1024 * 1024
)
assert hashlib.sha256(data).hexdigest() == digest, "official collector SHA mismatch"
collector.write_bytes(data)
collector.chmod(0o755)
license_data = fetch("https://raw.githubusercontent.com/netsampler/goflow2/v2.2.7/LICENSE", 16384)
assert b"BSD 3-Clause License" in license_data
(ROOT / "goflow2-LICENSE").write_bytes(license_data)
version = subprocess.check_output([str(collector), "-v"], stderr=subprocess.STDOUT, text=True)
assert "GoFlow2 v2.2.7 " in version
(ROOT / "goflow2-version.txt").write_text(version)

archive = ROOT / "cistern-sflow.tar.gz"
data = archive.read_bytes() if archive.exists() else fetch(
    f"https://codeload.github.com/Cistern/sflow/tar.gz/{REVISION}", 128 * 1024
)
assert hashlib.sha256(data).hexdigest() == SOURCE_SHA, "unmodified source SHA mismatch"
archive.write_bytes(data)
package = ROOT / "cistern"
package.mkdir(exist_ok=True)
count = 0
with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as source:
    for member in source.getmembers():
        path = pathlib.PurePosixPath(member.name)
        if not member.isfile() or len(path.parts) != 2 or path.parts[0] != "sflow-" + REVISION:
            continue
        if (path.suffix == ".go" and not path.name.endswith("_test.go")) or path.name in ("go.mod", "LICENSE"):
            content = source.extractfile(member).read()
            (package / path.name).write_bytes(content)
            count += 1
assert count >= 10 and b"Redistribution and use" in (package / "LICENSE").read_bytes()
build = ROOT / "peer-command"
build.mkdir(exist_ok=True)
(build / "main.go").write_bytes(pathlib.Path(__file__).with_name("peer.go").read_bytes())
(build / "go.mod").write_text(
    "module netget-sflow-independent-peer\n\ngo 1.21\n\nrequire github.com/Cistern/sflow v0.0.0\n"
    f"\nreplace github.com/Cistern/sflow => {package}\n"
)
env = os.environ.copy()
env.update(GOCACHE=str(ROOT / "go-cache"), GOPATH=str(ROOT / "go-path"), GOMODCACHE=str(ROOT / "go-mod-cache"),
           GOWORK="off", GOENV="off", GOTOOLCHAIN="local", GOPROXY="off", GOSUMDB="off", GOMAXPROCS="2")
peer = ROOT / "cistern-sflow-peer"
subprocess.run(["go", "build", "-p", "2", "-trimpath", "-o", str(peer), "."], cwd=build, env=env, check=True)
assert subprocess.check_output([str(peer), "version"], text=True).strip() == "Cistern/sflow " + REVISION
(ROOT / "versions.json").write_text(json.dumps({"goflow2": "2.2.7", "cistern_sflow_revision": REVISION,
    "cistern_source_sha256": SOURCE_SHA, "source_id_public_argument_workaround": True,
    "cistern_vlan_encoder_not_used": True}, indent=2) + "\n")
print("export NETGET_SFLOW_PEER=" + str(peer))
print("export NETGET_SFLOW_COLLECTOR=" + str(collector))
