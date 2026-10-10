"""Install the TR-069 tests' peers into an owned ROOT:

- genieacs-sim 0.9.0 (npm, the GenieACS project's CPE simulator) with libxmljs 1.0.11, run
  through `peer/run-sim.cjs` (copied next to its modules): the device the ACS tests manage.
- GenieACS 1.2.16 (npm): the ACS the CPE tests inform (genieacs-cwmp and genieacs-nbi).
- MongoDB 8.0.4 (the official Ubuntu 24.04 build, SHA-256 checked): GenieACS's database.

Needs node 20+ and a C++ toolchain (libxmljs builds a native module).

Usage: python3 install_peers.py /absolute/owned/root
Prints NETGET_GENIEACS_SIM, NETGET_GENIEACS and NETGET_MONGOD.
"""
import hashlib, json, pathlib, shutil, subprocess, sys, tarfile, urllib.request

MONGO = "mongodb-linux-x86_64-ubuntu2404-8.0.4"
MONGO_URL = f"https://fastdl.mongodb.org/linux/{MONGO}.tgz"
MONGO_SHA256 = "ef141c0827e8c39b8635f388e7ed1895b2adef6bb58cc3126ebe7898d18920bb"

root = pathlib.Path(sys.argv[1]).resolve()
here = pathlib.Path(__file__).resolve().parent


def npm(prefix, package_json):
    prefix.mkdir(parents=True, exist_ok=True)
    (prefix / "package.json").write_text(json.dumps(package_json))
    subprocess.run(["npm", "install", "--silent", "--no-audit", "--no-fund", "--prefix", str(prefix)],
                   check=True, timeout=1800)


sim = root / "sim"
npm(sim, {"name": "netget-cwmp-sim", "private": True, "dependencies": {"genieacs-sim": "0.9.0"},
          "overrides": {"libxmljs": "1.0.11"}})
shutil.copy(here / "peer" / "run-sim.cjs", sim / "run-sim.cjs")
acs = root / "acs"
npm(acs, {"name": "netget-cwmp-acs", "private": True, "dependencies": {"genieacs": "1.2.16"}})

mongod = root / MONGO / "bin" / "mongod"
if not mongod.exists():
    for attempt in range(4):
        try:
            with urllib.request.urlopen(MONGO_URL, timeout=900) as r:
                data = r.read()
            break
        except Exception as e:  # a cut-off transfer: try again
            if attempt == 3:
                raise
            print(f"retrying MongoDB download: {e}", file=sys.stderr)
    got = hashlib.sha256(data).hexdigest()
    if got != MONGO_SHA256:
        sys.exit(f"{MONGO_URL}: sha256 {got}, expected {MONGO_SHA256}")
    archive = root / "mongo.tgz"
    archive.write_bytes(data)
    with tarfile.open(archive) as t:
        t.extractall(root, filter="data")
    archive.unlink()
subprocess.run([str(mongod), "--version"], check=True, timeout=60, stdout=subprocess.DEVNULL)
print("export NETGET_GENIEACS_SIM=" + str(sim / "run-sim.cjs"))
print("export NETGET_GENIEACS=" + str(acs / "node_modules" / "genieacs"))
print("export NETGET_MONGOD=" + str(mongod))
