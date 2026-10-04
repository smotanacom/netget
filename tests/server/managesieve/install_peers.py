"""Pinned, unchanged ManageSieve peers in an owned ROOT:

- Dovecot 2.4.5 with Pigeonhole 2.4.5 (LGPL-2.1/MIT, C) built from hash-pinned release
  tarballs: the ManageSieve server for NetGet's client, which compiles every uploaded script.
  Needs a C toolchain, pkg-config and OpenSSL headers (Homebrew openssl@3, or libssl-dev).
  The same version on every platform, so one configuration serves macOS and Linux.
- sievelib 1.5.0 (MIT, Python), a hash-pinned wheel in a venv: the client of NetGet's server.

Usage: python3 install_peers.py /absolute/owned/root   (Python >= 3.10)
"""
import hashlib, os, pathlib, ssl, subprocess, sys, tarfile, urllib.request, venv

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
SOURCES = {
    "dovecot": ("https://dovecot.org/releases/2.4/dovecot-2.4.5.tar.gz",
                "868c2686a61b5f8e00a3e4721789b1ab46e6528fd773a5fbed07a6ecba7731e6"),
    "pigeonhole": ("https://pigeonhole.dovecot.org/releases/2.4/dovecot-pigeonhole-2.4.5.tar.gz",
                   "ad7c478cb3aaa76c5f81f86727a3e6843645b0a1253f5684fb8a0beec0d22925"),
}
WHEEL = ("sievelib-1.5.0-py3-none-any.whl",
         "https://files.pythonhosted.org/packages/d4/dc/cfd57eb53662202f68889d93c3455d2af6ffef4efc3c2f3544277f90cd89/",
         "4979c5de564eace76e132d4071caf4fe25cbb7e3c50187057e42b6eff53a255b")
ctx = ssl.create_default_context(cafile="/etc/ssl/cert.pem" if sys.platform == "darwin" else None)

def fetch(url, path, digest):
    if not path.exists():
        with urllib.request.urlopen(urllib.request.Request(url, headers={"User-Agent": "netget-managesieve-peer"}), context=ctx, timeout=120) as r:
            path.write_bytes(r.read(50_000_000))
    assert hashlib.sha256(path.read_bytes()).hexdigest() == digest, "pinned SHA-256 mismatch: " + path.name

def run(cmd, cwd, env=None):
    r = subprocess.run(cmd, cwd=cwd, env=env, timeout=3600, capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit(f"{' '.join(cmd)} failed:\n{r.stdout[-4000:]}\n{r.stderr[-4000:]}")

trees = {}
for name, (url, digest) in SOURCES.items():
    archive = root / f"{name}.tar.gz"
    fetch(url, archive, digest)
    dest = root / name
    if not dest.exists():
        with tarfile.open(archive) as t:
            top = t.getnames()[0].split("/")[0]
            t.extractall(root, filter="data")
        (root / top).rename(dest)
    trees[name] = dest

prefix = root / "inst"
dovecot = prefix / "sbin" / "dovecot"
if not (prefix / "libexec" / "dovecot" / "managesieve").exists():
    env = dict(os.environ)
    if sys.platform == "darwin":
        openssl = subprocess.run(["brew", "--prefix", "openssl@3"], check=True, capture_output=True, text=True).stdout.strip()
        env.update(CPPFLAGS=f"-I{openssl}/include", LDFLAGS=f"-L{openssl}/lib", LIBS="-liconv")
    # No systemd units: on Linux the default installs them under /usr, outside the owned root.
    env["systemdsystemunitdir"] = str(prefix / "lib" / "systemd")
    run(["./configure", f"--prefix={prefix}", "--without-ldap", "--without-lua", "--without-sqlite",
         "--without-pgsql", "--without-mysql", "--without-icu", "--without-stemmer", "--without-libcap",
         "--without-systemd", "--with-ssl=openssl"], trees["dovecot"], env)
    run(["make", f"-j{os.cpu_count() or 4}"], trees["dovecot"], env)
    run(["make", "install"], trees["dovecot"], env)
    run(["./configure", f"--prefix={prefix}", f"--with-dovecot={prefix}/lib/dovecot"], trees["pigeonhole"], env)
    run(["make", f"-j{os.cpu_count() or 4}"], trees["pigeonhole"], env)
    run(["make", "install"], trees["pigeonhole"], env)
out = subprocess.run([str(dovecot), "--version"], capture_output=True, text=True, timeout=30).stdout
assert out.startswith("2.4.5"), out

wheel = root / WHEEL[0]
fetch(WHEEL[1] + WHEEL[0], wheel, WHEEL[2])
venv_dir = root / "venv"
if not (venv_dir / "bin" / "python").exists():
    venv.EnvBuilder(with_pip=True).create(venv_dir)
python = str(venv_dir / "bin" / "python")
subprocess.run([python, "-m", "pip", "install", "--quiet", "--disable-pip-version-check", "--no-deps", str(wheel)], check=True, timeout=600)
subprocess.run([python, "-c", "import importlib.metadata as m, sievelib.managesieve; assert m.version('sievelib') == '1.5.0'"], check=True, timeout=30)
print("export NETGET_MANAGESIEVE_DOVECOT=" + str(dovecot))
print("export NETGET_MANAGESIEVE_PYTHON=" + python)
