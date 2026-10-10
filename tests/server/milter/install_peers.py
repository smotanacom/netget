"""Unchanged Milter peers, used in both roles:
- OpenDKIM's miltertest (C; an MTA side driven by tests/server/milter/miltertest.lua) and
  pymilter (Sendmail's libmilter through its Python binding; runs
  tests/client/milter/pymilter_filter.py) come from the distribution:
  apt-get install miltertest python3-milter (Ubuntu 24.04 ships miltertest
  2.11.0~beta2 and python3-milter 1.0.5 against libmilter 8.18);
- emersion/go-milter v0.4.1, in client and server roles, built into ROOT from
  tests/client/milter/peer (go.sum-pinned). Needs Go 1.24 or newer.

Usage: python3 install_peers.py /absolute/owned/root
Prints the NETGET_MILTERTEST, NETGET_MILTER_PYTHON and NETGET_MILTER_GO_PEER exports the tests
read. The python is whichever system interpreter can import pymilter's `milter` module (the
Debian package builds it for the distribution's default python3 only).
"""
import glob, os, pathlib, shutil, subprocess, sys

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
here = pathlib.Path(__file__).resolve().parent
miltertest = shutil.which("miltertest")
assert miltertest, "miltertest is required: apt-get install miltertest"
candidates = ["/usr/bin/python3"] + sorted(glob.glob("/usr/bin/python3.[0-9]*"), reverse=True)
python = next(
    (
        p
        for p in candidates
        if os.access(p, os.X_OK)
        and not p.endswith("-config")
        and subprocess.run([p, "-c", "import milter, Milter"], capture_output=True, timeout=60).returncode == 0
    ),
    None,
)
assert python, "pymilter is required: apt-get install python3-milter (no system python imports milter)"
peer = root / "milter-peer"
build_env = dict(os.environ, GOFLAGS="-mod=readonly", CGO_ENABLED="0")
go_dir = here.parent.parent / "client" / "milter" / "peer"
subprocess.run(["go", "build", "-trimpath", "-o", str(peer), "."], cwd=go_dir, env=build_env, check=True, timeout=900)
subprocess.run(["go", "mod", "verify"], cwd=go_dir, env=build_env, check=True, timeout=300)
print("export NETGET_MILTERTEST=" + miltertest)
print("export NETGET_MILTER_PYTHON=" + python)
print("export NETGET_MILTER_GO_PEER=" + str(peer))
