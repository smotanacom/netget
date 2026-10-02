#!/usr/bin/env python3
"""Install pinned independent Graphite peers under one supplied task-owned root.

Run with Python 3.11: Carbon 1.1.10 uses imp/ConfigParser APIs removed in 3.12.
No service, storage writer, global install or NetGet runtime dependency is added.
"""
import pathlib
import shlex
import subprocess
import sys

if sys.version_info[:2] != (3, 11):
    raise SystemExit('Use Python 3.11 for the pinned Carbon 1.1.10 reference peer')
root = pathlib.Path(sys.argv[1]).resolve() / 'graphite-python'
root.mkdir(parents=True, exist_ok=True)
subprocess.run([
    sys.executable, '-m', 'pip', 'install', '--no-cache-dir', '--target', str(root),
    'graphyte==1.7.1', 'carbon==1.1.10', 'Twisted==25.5.0',
    'cachetools==6.2.0', 'six==1.17.0',
], check=True)
# Carbon's distribution preserves its opt/graphite layout under --target.
print('export PYTHONPATH=' + shlex.quote(str(root) + ':' + str(root / 'opt/graphite/lib')))
print('export NETGET_GRAPHITE_PYTHON=' + shlex.quote(sys.executable))
