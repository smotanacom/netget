"""Install the Stratum tests' peers into an owned ROOT, each pinned by SHA-256:

- cpuminer 2.5.1 (pooler, C): the miner the server tests point at NetGet's pool.
- ckpool at 3cedff5e1977 (C): the pool the client tests mine against, in solo mode.
- Bitcoin Core 28.1: the regtest node ckpool builds its work from.

Builds need a C toolchain, autoconf/automake/libtool, yasm and libcurl's headers
(Ubuntu: build-essential autoconf automake libtool yasm pkg-config libcurl4-openssl-dev).

Usage: python3 install_peers.py /absolute/owned/root
Prints the NETGET_CPUMINER, NETGET_CKPOOL, NETGET_BITCOIND and NETGET_BITCOIN_CLI exports
the tests read.
"""
import hashlib, os, pathlib, subprocess, sys, tarfile, urllib.request

PEERS = {
    "cpuminer": ("https://downloads.sourceforge.net/project/cpuminer/pooler-cpuminer-2.5.1.tar.gz",
                 "337f04fdb32f34b85819d09d59f6d3cf62991ac2e656735c43661dd3d4c57631"),
    "ckpool": ("https://bitbucket.org/ckolivas/ckpool/get/3cedff5e1977.tar.gz",
               "3f21d64cdbe5a703c0442cb243fa91139609cc44683f05408f7400fdb6ef912e"),
    "bitcoin": ("https://bitcoincore.org/bin/bitcoin-core-28.1/bitcoin-28.1-x86_64-linux-gnu.tar.gz",
                "07f77afd326639145b9ba9562912b2ad2ccec47b8a305bd075b4f4cb127b7ed7"),
}

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)


def fetch(name):
    url, digest = PEERS[name]
    dest = root / name
    if dest.exists():
        return dest
    archive = root / f"{name}.tar.gz"
    with urllib.request.urlopen(url, timeout=300) as r:
        data = r.read()
    got = hashlib.sha256(data).hexdigest()
    if got != digest:
        sys.exit(f"{url}: sha256 {got}, expected {digest}")
    archive.write_bytes(data)
    tmp = root / f"{name}.tmp"
    tmp.mkdir()
    with tarfile.open(archive) as t:
        t.extractall(tmp, filter="data")
    (only,) = list(tmp.iterdir())
    only.rename(dest)
    tmp.rmdir()
    return dest


def run(cmd, cwd):
    subprocess.run(cmd, cwd=cwd, check=True, timeout=1800,
                   stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)


jobs = str(os.cpu_count() or 2)
cpuminer = fetch("cpuminer")
if not (cpuminer / "minerd").exists():
    run(["./configure", "CFLAGS=-O2"], cpuminer)
    run(["make", "-j", jobs], cpuminer)
ckpool = fetch("ckpool")
if not (ckpool / "src" / "ckpool").exists():
    run(["./autogen.sh"], ckpool)
    run(["./configure", "--without-ckdb"], ckpool)
    run(["make", "-j", jobs], ckpool)
bitcoin = fetch("bitcoin")
for exe in [cpuminer / "minerd", ckpool / "src" / "ckpool", bitcoin / "bin" / "bitcoind"]:
    subprocess.run([str(exe), "--help"], check=False, timeout=60,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if not exe.exists():
        sys.exit(f"{exe} was not built")
print("export NETGET_CPUMINER=" + str(cpuminer / "minerd"))
print("export NETGET_CKPOOL=" + str(ckpool / "src" / "ckpool"))
print("export NETGET_BITCOIND=" + str(bitcoin / "bin" / "bitcoind"))
print("export NETGET_BITCOIN_CLI=" + str(bitcoin / "bin" / "bitcoin-cli"))
