"""Pin official Apache-2.0 Prometheus service in owned storage, no global installs.

Usage: install_peers.py ROOT [EXISTING_PROMETHEUS]
An explicitly supplied existing binary is version checked and reused read-only;
without it the official release archive is bounded and SHA256 verified.
"""
import hashlib
import io
import json
import pathlib
import platform
import ssl
import subprocess
import sys
import tarfile
import urllib.request
import zipfile

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
version = "3.15.0"
ctx = ssl.create_default_context(cafile="/etc/ssl/cert.pem" if sys.platform == "darwin" else None)


def fetch(url, limit):
    request = urllib.request.Request(url, headers={"User-Agent": "netget-owned-independent-peer"})
    with urllib.request.urlopen(request, context=ctx, timeout=60) as response:
        data = response.read(limit + 1)
    assert len(data) <= limit, "peer download byte bound"
    return data


targets = {
    ("darwin", "arm64"): ("darwin-arm64", "920df4d17e78b3b0175af144eb318b0c74d1cf7b1d1251b326966f0e81977260"),
    ("linux", "x86_64"): ("linux-amd64", "2a542df32eac02ee17b9d844fb2aa1de00dafa5476579ba8a3ba862e9d572ea0"),
}
assert (sys.platform, platform.machine()) in targets, "supported bootstrap: Linux amd64/macOS arm64"
target, digest = targets[(sys.platform, platform.machine())]
if len(sys.argv) == 3:
    binary = pathlib.Path(sys.argv[2]).resolve()
    assert binary.is_file(), "explicit existing binary missing"
else:
    archive = root / f"prometheus-{version}.{target}.tar.gz"
    data = archive.read_bytes() if archive.exists() else fetch(
        f"https://github.com/prometheus/prometheus/releases/download/v{version}/{archive.name}", 128 * 1024 * 1024
    )
    assert hashlib.sha256(data).hexdigest() == digest, "official archive SHA mismatch"
    archive.write_bytes(data)
    binary = root / f"prometheus-{version}"
    prefix = f"prometheus-{version}.{target}"
    found = set()
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as source:
        for member in source.getmembers():
            path = pathlib.PurePosixPath(member.name)
            if not member.isfile() or len(path.parts) != 2 or path.parts[0] != prefix or path.name not in ("prometheus", "LICENSE", "NOTICE"):
                continue
            assert member.size <= 256 * 1024 * 1024, "peer extracted member byte bound"
            content = source.extractfile(member).read()
            destination = binary if path.name == "prometheus" else root / path.name
            destination.write_bytes(content)
            found.add(path.name)
    assert found == {"prometheus", "LICENSE", "NOTICE"}
    binary.chmod(0o755)
license_data = fetch(f"https://raw.githubusercontent.com/prometheus/prometheus/v{version}/LICENSE", 32768)
assert b"Apache License" in license_data and b"Version 2.0" in license_data
(root / "LICENSE").write_bytes(license_data)
output = subprocess.check_output([str(binary), "--version"], text=True, stderr=subprocess.STDOUT)
assert f"prometheus, version {version} " in output, "pinned service version required"
(root / "version.txt").write_text(output)
(root / "versions.json").write_text(json.dumps({"prometheus": version, "official_archive_sha256": digest,
    "binary": str(binary), "existing_binary_reused": len(sys.argv) == 3, "scope": "published remote write1.0 float samples"}, indent=2) + "\n")
print("export NETGET_PRW_PROMETHEUS=" + str(binary))

# The official exporter supplies only scrape values; the actual wire sender is
# the independent Prometheus daemon. The pure Python wheel has no dependencies.
wheel_name = "prometheus_client-0.22.1-py3-none-any.whl"
wheel = root / wheel_name
wheel_data = wheel.read_bytes() if wheel.exists() else fetch("https://files.pythonhosted.org/packages/32/ae/ec06af4fe3ee72d16973474f122541746196aaa16cea6f66d18b963c6177/" + wheel_name, 128 * 1024)
assert hashlib.sha256(wheel_data).hexdigest() == "cca895342e308174341b2cbf99a56bef291fbc0ef7b9e5412a0f26d653ba7094", "official exporter wheel SHA mismatch"
wheel.write_bytes(wheel_data)
package = root / "python"
package.mkdir(exist_ok=True)
with zipfile.ZipFile(io.BytesIO(wheel_data)) as source:
    total = 0
    for member in source.infolist():
        path = pathlib.PurePosixPath(member.filename)
        assert not path.is_absolute() and ".." not in path.parts and not member.is_dir(), "wheel member path"
        assert path.parts[0] in ("prometheus_client", "prometheus_client-0.22.1.dist-info"), "wheel namespace"
        total += member.file_size
        assert total <= 512 * 1024, "wheel extracted byte bound"
        destination = package / path
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(source.read(member))
assert any(b"Apache License" in p.read_bytes() for p in (package / "prometheus_client-0.22.1.dist-info").rglob("LICENSE*")), "official exporter license"
print("export PYTHONPATH=" + str(package))
print("export NETGET_PRW_PYTHON=" + sys.executable)
