"""Install the ActivityPub tests' peers into an owned ROOT: Fedify 2.4.2 from npm (the CLI,
and the library `peer/peer.mjs` is built on), with the peer copied next to its modules.
Needs node 20+.

Usage: python3 install_peers.py /absolute/owned/root
Prints the NETGET_FEDIFY and NETGET_FEDIFY_PEER exports the tests read.
"""
import pathlib, shutil, subprocess, sys

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
subprocess.run(["npm", "install", "--silent", "--no-audit", "--no-fund", "--prefix", str(root),
                "@fedify/cli@2.4.2", "@fedify/fedify@2.4.2", "@fedify/vocab@2.4.2"],
               check=True, timeout=900)
here = pathlib.Path(__file__).resolve().parent
shutil.copy(here / "peer" / "peer.mjs", root / "peer.mjs")
fedify = root / "node_modules" / ".bin" / "fedify"
subprocess.run([str(fedify), "--version"], check=True, timeout=120, stdout=subprocess.DEVNULL)
print("export NETGET_FEDIFY=" + str(fedify))
print("export NETGET_FEDIFY_PEER=" + str(root / "peer.mjs"))
