"""Pinned, unchanged Zipkin servers in an owned ROOT:
- the official Zipkin server, zipkin-server 3.5.1's exec jar from Maven Central (sha1
  38091e09… as Maven Central publishes it; pinned here by sha256), run with java >= 17 and
  its default in-memory storage;
- Jaeger 1.62.0's all-in-one from its GitHub release (sha256 as jaeger-1.62.0.sha256sum.txt
  publishes it), whose Zipkin collector stores into Jaeger's own model and answers through
  Jaeger's query API.
Linux amd64 only (Jaeger's binary).

Usage: python3 install_peers.py /absolute/owned/root
Prints the NETGET_ZIPKIN_JAR, NETGET_ZIPKIN_JAVA and NETGET_JAEGER_BIN exports the tests read.
"""
import hashlib, pathlib, shutil, subprocess, sys, tarfile, urllib.request

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
# Maven Central first, then Google's mirror of it (Central answers 429 under heavy use).
JAR = (["https://repo1.maven.org/maven2/io/zipkin/zipkin-server/3.5.1/zipkin-server-3.5.1-exec.jar",
        "https://maven-central.storage-download.googleapis.com/maven2/io/zipkin/zipkin-server/3.5.1/zipkin-server-3.5.1-exec.jar"],
       "7ac633a6832647e71e50012a5e3e91fbc7054414955261724526030faf271d8d")
JAEGER = (["https://github.com/jaegertracing/jaeger/releases/download/v1.62.0/jaeger-1.62.0-linux-amd64.tar.gz"],
          "ccd00b24a3e740eb079579b1c74389f6e6a6742e4fd34697d8789dae05ae2079")

def fetch(urls, sha256, dest):
    if dest.exists() and hashlib.sha256(dest.read_bytes()).hexdigest() == sha256:
        return dest
    tmp = dest.with_suffix(".part")
    errors = []
    for url in urls:
        try:
            with urllib.request.urlopen(url, timeout=600) as r, open(tmp, "wb") as f:
                shutil.copyfileobj(r, f)
        except OSError as e:
            errors.append(f"{url}: {e}")
            continue
        got = hashlib.sha256(tmp.read_bytes()).hexdigest()
        if got == sha256:
            tmp.rename(dest)
            return dest
        errors.append(f"{url}: sha256 {got}, expected {sha256}")
    sys.exit("download failed: " + "; ".join(errors))

jar = fetch(*JAR, root / "zipkin-server-3.5.1-exec.jar")
java = shutil.which("java")
assert java, "java >= 17 is required: apt-get install openjdk-21-jre-headless (or brew install openjdk)"
subprocess.run([java, "-version"], check=True, timeout=60, stderr=subprocess.DEVNULL)
archive = fetch(*JAEGER, root / "jaeger-1.62.0-linux-amd64.tar.gz")
jaeger = root / "jaeger-all-in-one"
if not jaeger.exists():
    with tarfile.open(archive) as t:
        member = t.getmember("jaeger-1.62.0-linux-amd64/jaeger-all-in-one")
        with t.extractfile(member) as src, open(jaeger, "wb") as dst:
            shutil.copyfileobj(src, dst)
    jaeger.chmod(0o755)
subprocess.run([str(jaeger), "version"], check=True, timeout=60, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
print("export NETGET_ZIPKIN_JAR=" + str(jar))
print("export NETGET_ZIPKIN_JAVA=" + java)
print("export NETGET_JAEGER_BIN=" + str(jaeger))
