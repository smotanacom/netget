"""Pinned, unchanged Eclipse Leshan 2.0.0-M15 demos (EPL-2.0, Java, on Californium) from Maven
Central, hash-pinned, into an owned ROOT: the client demo is a device for NetGet's server and
the server demo (with its REST API) a server for NetGet's client. Needs Java 17 or later on PATH.

Usage: python3 install_peers.py /absolute/owned/root
"""
import hashlib, pathlib, ssl, subprocess, sys, urllib.request

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
BASE = "https://repo1.maven.org/maven2/org/eclipse/leshan/"
JARS = {
    "server": ("leshan-server-demo/2.0.0-M15/leshan-server-demo-2.0.0-M15-jar-with-dependencies.jar",
               "0db3208dbede54f97b4c4b7967db8037f2d70424108d83db3e20c88962fac82d"),
    "client": ("leshan-client-demo/2.0.0-M15/leshan-client-demo-2.0.0-M15-jar-with-dependencies.jar",
               "372f2152b6321790fff7deb1ce6932a499d9a777f05bbecf73ef028baaa422a2"),
}
ctx = ssl.create_default_context(cafile="/etc/ssl/cert.pem" if sys.platform == "darwin" else None)
paths = {}
for role, (rel, digest) in JARS.items():
    jar = root / rel.rsplit("/", 1)[1]
    if not jar.exists():
        with urllib.request.urlopen(urllib.request.Request(BASE + rel, headers={"User-Agent": "netget-lwm2m-peer"}), context=ctx, timeout=300) as r:
            jar.write_bytes(r.read(100_000_000))
    assert hashlib.sha256(jar.read_bytes()).hexdigest() == digest, "pinned SHA-256 mismatch: " + jar.name
    paths[role] = jar
version = subprocess.run(["java", "-version"], capture_output=True, text=True, timeout=60).stderr
assert "version" in version, version
print("export NETGET_LWM2M_LESHAN_SERVER=" + str(paths["server"]))
print("export NETGET_LWM2M_LESHAN_CLIENT=" + str(paths["client"]))
