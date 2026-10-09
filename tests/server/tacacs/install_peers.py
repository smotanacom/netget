"""Pinned unmodified SDK peers in an owned ROOT; existing Go/Python required.

ROOT: ordinary bounded source/wheel extraction, local wrapper using public SDK
APIs, no network Go dependencies, global install, source patch or containers.
NETGET_TACACS_GO_CACHE may reuse a programme-owned Go compilation cache.
"""
import hashlib
import io
import json
import os
import pathlib
import shutil
import ssl
import subprocess
import sys
import tarfile
import urllib.request
import zipfile

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
ctx = ssl.create_default_context(cafile='/etc/ssl/cert.pem' if sys.platform == 'darwin' else None)

def fetch(name, url, digest, limit):
    path = root / name
    if path.exists():
        data = path.read_bytes()
    else:
        req = urllib.request.Request(url, headers={'User-Agent': 'netget-owned-independent-peer'})
        with urllib.request.urlopen(req, context=ctx, timeout=60) as response:
            data = response.read(limit + 1)
    assert len(data) <= limit, 'bounded peer download'
    assert hashlib.sha256(data).hexdigest() == digest, 'pinned SHA256 mismatch: ' + name
    path.write_bytes(data)
    return data

source_digest = 'ced63f2d09fbde9b5fdca3b52978a270731c83cbdc0dc3f35720e76ae4696c80'
data = fetch('nwaples-tacplus-v0.0.3.tar.gz',
             'https://codeload.github.com/nwaples/tacplus/tar.gz/refs/tags/v0.0.3', source_digest, 65536)
source = root / 'nwaples-tacplus-v0.0.3'
source.mkdir(exist_ok=True)
size = 0
with tarfile.open(fileobj=io.BytesIO(data), mode='r:gz') as archive:
    for entry in archive:
        path = pathlib.PurePosixPath(entry.name)
        assert not path.is_absolute() and '..' not in path.parts
        if not entry.isfile() or len(path.parts) != 2:
            continue
        if path.name.endswith('.go') or path.name in ('LICENSE', 'go.mod', 'README.md'):
            size += entry.size
            assert entry.size <= 65536 and size <= 262144
            (source / path.name).write_bytes(archive.extractfile(entry).read())
assert b'Redistribution and use' in (source / 'LICENSE').read_bytes()
wrapper = root / 'nwaples-wrapper'
wrapper.mkdir(exist_ok=True)
(wrapper / 'main.go').write_bytes(pathlib.Path(__file__).with_name('peer.go').read_bytes())
(wrapper / 'go.mod').write_text('module netget-tacacs-peer\n\ngo 1.17\n\nrequire github.com/nwaples/tacplus v0.0.3\nreplace github.com/nwaples/tacplus => ../nwaples-tacplus-v0.0.3\n')
go = shutil.which('go')
assert go, 'existing Go toolchain required (upstream SDK needs Go1.17+)'
env = os.environ.copy()
env.update(GOCACHE=env.get('NETGET_TACACS_GO_CACHE', str(root / 'go-cache')),
           GOPATH=str(root / 'go-path'), GOMODCACHE=str(root / 'go-modules'),
           GOPROXY='off', GOSUMDB='off', GOTOOLCHAIN='local')
peer = root / 'nwaples-tacplus-peer'
build = subprocess.run([go, 'build', '-o', str(peer), '.'], cwd=wrapper, env=env,
                       capture_output=True, text=True, timeout=180)
(root / 'build.log').write_text(build.stdout + build.stderr)
assert build.returncode == 0, build.stderr
version = json.loads(subprocess.check_output([str(peer), '-version'], text=True))
assert version['version'] == '0.0.3' and version['source_modified'] is False
python_root = root / 'python'
python_root.mkdir(exist_ok=True)
for name, url, digest in [
    ('tacacs_plus-2.6-py2.py3-none-any.whl', 'https://files.pythonhosted.org/packages/17/95/3827a86360757596715ed8eb0394acb5b5104176f36716e468e45ed5fd68/tacacs_plus-2.6-py2.py3-none-any.whl', '55aa4e733b0c4366cf5ab2d36deb03729466554319b239b9221b509b256128ff'),
    ('six-1.17.0-py2.py3-none-any.whl', 'https://files.pythonhosted.org/packages/b7/ce/149a00dd41f10bc29e5921b496af8b574d8413afcd5e30dfa0ed46c2cc5e/six-1.17.0-py2.py3-none-any.whl', '4721f391ed90541fddacab5acf947aa0d3dc7d27b2e1e8eda2be8970586c3274'),
]:
    data = fetch(name, url, digest, 65536)
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        size = 0
        for entry in archive.infolist():
            path = pathlib.PurePosixPath(entry.filename)
            assert not path.is_absolute() and '..' not in path.parts
            if entry.is_dir():
                continue
            size += entry.file_size
            assert entry.file_size <= 131072 and size <= 262144
            destination = python_root.joinpath(*path.parts)
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(archive.read(entry))
# Published 2.6 wheel/sdist omit the BSD license; retain unchanged pinned upstream copy.
fetch('tacacs-plus-LICENSE', 'https://raw.githubusercontent.com/ansible/tacacs_plus/2.6/LICENSE',
      '6b6fbecdde41901e6305b988b09bc0aba3adbb47200c9cfe9938f30ac451cbca', 16384)
env['PYTHONPATH'] = str(python_root)
subprocess.run([sys.executable, '-c', 'import tacacs_plus,six;assert six.__version__=="1.17.0"'],
               env=env, check=True, timeout=10)
(root / 'versions.json').write_text(json.dumps({**version, 'source_sha256': source_digest,
    'go': subprocess.check_output([go, 'version'], text=True).strip(),
    'python': sys.version, 'tacacs_plus': '2.6', 'six': '1.17.0'}, indent=2) + '\n')
print('export NETGET_TACACS_PEER=' + str(peer))
print('export NETGET_TACACS_PYTHON=' + sys.executable)
print('export PYTHONPATH=' + str(python_root))
