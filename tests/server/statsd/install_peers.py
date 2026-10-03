#!/usr/bin/env python3
"""Install pinned test peers outside the repository; never runs npm install scripts.

Usage: python3 tests/server/statsd/install_peers.py /tmp/netget-statsd-peers
Needs network access for PyPI and registry.npmjs.org. No runtime dependency of NetGet.
"""
import base64
import hashlib
import io
import json
import os
import pathlib
import ssl
import subprocess
import sys
import tarfile
import urllib.request

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
subprocess.run([
    sys.executable, '-m', 'pip', 'install', '--no-cache-dir', '--target',
    str(root / 'statsd-python'), 'datadog==0.52.0', 'statsd==4.0.1',
], check=True)

# pip's system trust store handles managed networks whose trusted roots are in
# the OS keychain. Fall back to normal Python TLS verification where unavailable.
try:
    from pip._vendor import truststore
    context = truststore.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    if os.environ.get('SSL_CERT_FILE'):
        context.load_verify_locations(cafile=os.environ['SSL_CERT_FILE'])
except ImportError:
    context = ssl.create_default_context()

with urllib.request.urlopen('https://registry.npmjs.org/statsd/0.9.0', context=context, timeout=30) as reply:
    metadata = json.load(reply)
assert metadata['version'] == '0.9.0'
with urllib.request.urlopen(metadata['dist']['tarball'], context=context, timeout=30) as reply:
    archive_bytes = reply.read()
algorithm, expected = metadata['dist']['integrity'].split('-', 1)
actual = base64.b64encode(hashlib.sha512(archive_bytes).digest()).decode()
assert algorithm == 'sha512' and actual == expected, 'npm integrity verification failed'
output = root / 'statsd-node' / 'node_modules' / 'statsd'
output.mkdir(parents=True, exist_ok=True)
with tarfile.open(fileobj=io.BytesIO(archive_bytes), mode='r:gz') as archive:
    for member in archive:
        # Exclude links/devices; reject unsafe paths before extracting any file.
        if not member.isfile():
            continue
        path = pathlib.PurePosixPath(member.name)
        assert path.parts[0] == 'package' and '..' not in path.parts
        target = output.joinpath(*path.parts[1:])
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(archive.extractfile(member).read())
# Optional Node dependencies (syslog/native, Windows service, proxy) are unused
# by the reference collector's UDP/stdout/custom-backend path in our test.
print('statsd@0.9.0 npm integrity:', metadata['dist']['integrity'])
print('PYTHONPATH=' + str(root / 'statsd-python'))
print('NODE_PATH=' + str(root / 'statsd-node' / 'node_modules'))
