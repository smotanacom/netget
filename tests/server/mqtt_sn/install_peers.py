"""Pinned, unchanged MQTT-SN peers built into an owned ROOT from hash-pinned source tarballs.

- mqtt-sn-tools at a1dd870 (MIT, C): mqtt-sn-pub and mqtt-sn-sub, clients of NetGet's gateway.
- Eclipse Paho MQTT-SN Gateway at e1e1b73 (EPL-2.0, C++), UDP build: the gateway in front of
  Mosquitto that NetGet's client talks to. Needs cmake, a C++ compiler and OpenSSL headers
  (Homebrew openssl@3 on macOS, libssl-dev on Linux). Mosquitto itself comes from the system
  package (mosquitto, mosquitto-clients).

Usage: python3 install_peers.py /absolute/owned/root
"""
import hashlib, os, pathlib, ssl, subprocess, sys, tarfile, urllib.request

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
SOURCES = {
    "mqtt-sn-tools": ("https://github.com/njh/mqtt-sn-tools/archive/a1dd870684f6c9647eb26057705059dc383a6c27.tar.gz",
                      "96ac4037b0ba69a76da3fcd5985eb046083af68623c3def73fed2f0debf40a11"),
    "paho-mqtt-sn": ("https://github.com/eclipse-paho/paho.mqtt-sn.embedded-c/archive/e1e1b733e0fecef6bed92798d7f6d6440d9026f2.tar.gz",
                     "948df5e211017f02f56ceea934c00b7b11465f30b1fe9d46257ed9cc5411c472"),
}
ctx = ssl.create_default_context(cafile="/etc/ssl/cert.pem" if sys.platform == "darwin" else None)
trees = {}
for name, (url, digest) in SOURCES.items():
    archive = root / f"{name}.tar.gz"
    if not archive.exists():
        with urllib.request.urlopen(urllib.request.Request(url, headers={"User-Agent": "netget-mqtt-sn-peer"}), context=ctx, timeout=120) as r:
            archive.write_bytes(r.read(20_000_000))
    assert hashlib.sha256(archive.read_bytes()).hexdigest() == digest, "pinned SHA-256 mismatch: " + name
    dest = root / name
    if not dest.exists():
        with tarfile.open(archive) as t:
            top = t.getnames()[0].split("/")[0]
            t.extractall(root, filter="data")
        (root / top).rename(dest)
    trees[name] = dest

subprocess.run(["make"], cwd=trees["mqtt-sn-tools"], check=True, timeout=600, capture_output=True)

build = trees["paho-mqtt-sn"] / "build.gateway"
build.mkdir(exist_ok=True)
cmake = ["cmake", "..", "-DSENSORNET=udp", "-DCMAKE_POLICY_VERSION_MINIMUM=3.5"]
if sys.platform == "darwin":
    openssl = subprocess.run(["brew", "--prefix", "openssl@3"], check=True, capture_output=True, text=True).stdout.strip()
    cmake += [f"-DCMAKE_EXE_LINKER_FLAGS=-L{openssl}/lib", f"-DCMAKE_CXX_FLAGS=-I{openssl}/include"]
subprocess.run(cmake, cwd=build, check=True, timeout=600, capture_output=True)
subprocess.run(["make", "MQTTSNPacket", "MQTT-SNGateway"], cwd=build, check=True, timeout=1800, capture_output=True)
gateway = trees["paho-mqtt-sn"] / "MQTTSNGateway" / "bin" / "MQTT-SNGateway"
assert gateway.exists(), gateway
print("export NETGET_MQTTSN_PUB=" + str(trees["mqtt-sn-tools"] / "mqtt-sn-pub"))
print("export NETGET_MQTTSN_SUB=" + str(trees["mqtt-sn-tools"] / "mqtt-sn-sub"))
print("export NETGET_MQTTSN_GATEWAY=" + str(gateway))
