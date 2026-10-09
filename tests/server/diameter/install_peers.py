"""Pinned unchanged independent Diameter SDK peers in owned ROOT.

Existing Python>=3.11 and Go>=1.26 required. No source patch, global installation,
container, external service or native NetGet runtime dependency. Adapters only
call public SDK APIs. NETGET_DIAMETER_GO_CACHE may borrow an owned build cache.
"""
import hashlib
import io
import json
import os
import pathlib
import re
import shutil
import ssl
import subprocess
import sys
import urllib.request
import zipfile

root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
assert sys.version_info >= (3, 11), 'existing Python>=3.11 required'
ctx = ssl.create_default_context(cafile='/etc/ssl/cert.pem' if sys.platform == 'darwin' else None)

def fetch(name, url, digest, limit):
    path = root / name
    if path.exists():
        data = path.read_bytes()
    else:
        with urllib.request.urlopen(urllib.request.Request(url, headers={'User-Agent': 'netget-independent-diameter-peer'}), context=ctx, timeout=60) as response:
            data = response.read(limit + 1)
    assert len(data) <= limit, 'bounded peer download'
    assert hashlib.sha256(data).hexdigest() == digest, 'pinned SHA256 mismatch: ' + name
    path.write_bytes(data)
    return data

def extract(data, destination, prefix, total_limit, file_limit):
    destination.mkdir(exist_ok=True)
    total = 0
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        for entry in archive.infolist():
            assert entry.filename.startswith(prefix)
            path = pathlib.PurePosixPath(entry.filename[len(prefix):])
            assert not path.is_absolute() and '..' not in path.parts
            if not path.parts or entry.is_dir():
                continue
            total += entry.file_size
            assert entry.file_size <= file_limit and total <= total_limit
            target = destination.joinpath(*path.parts)
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(archive.read(entry))

go_sha = 'e5947ecb7a80c9ba0c972c11f2c24516c4b67797fbef912307222ccc711e84b3'
go_data = fetch('go-diameter-v4.5.0.zip', 'https://proxy.golang.org/github.com/fiorix/go-diameter/v4/@v/v4.5.0.zip', go_sha, 1024*1024)
extract(go_data, root/'go-source', 'github.com/fiorix/go-diameter/v4@v4.5.0/', 4*1024*1024, 2*1024*1024)
assert b'Redistribution and use' in (root/'go-source'/'LICENSE').read_bytes()
wrapper = root/'go-wrapper'
wrapper.mkdir(exist_ok=True)
(wrapper/'main.go').write_bytes(pathlib.Path(__file__).with_name('peer.go').read_bytes())
(wrapper/'go.mod').write_text('module netget-diameter-peer\n\ngo 1.26.0\n\nrequire github.com/fiorix/go-diameter/v4 v4.5.0\nrequire github.com/ishidawataru/sctp v0.0.0-20251114114122-19ddcbc6aae2 // indirect\nreplace github.com/fiorix/go-diameter/v4 => ../go-source\n')
(wrapper/'go.sum').write_text('github.com/ishidawataru/sctp v0.0.0-20251114114122-19ddcbc6aae2 h1:36qep4gxKs+JgeHGWeQ040RyZdt9kQlLglL1rFVn/oQ=\ngithub.com/ishidawataru/sctp v0.0.0-20251114114122-19ddcbc6aae2/go.mod h1:co9pwDoBCm1kGxawmb4sPq0cSIOOWNPT4KnHotMP1Zg=\n')
go = shutil.which('go')
assert go, 'existing Go>=1.26 required'
go_version = subprocess.check_output([go, 'version'], text=True).strip()
version = re.search(r'go(\d+)\.(\d+)', go_version)
assert version and tuple(map(int, version.groups())) >= (1,26), go_version
build_env = os.environ.copy()
build_env.update(GOCACHE=build_env.get('NETGET_DIAMETER_GO_CACHE', str(root/'go-cache')), GOPATH=str(root/'go-path'), GOMODCACHE=str(root/'go-modules'), GOTOOLCHAIN='local')
if sys.platform == 'darwin':
    build_env['SSL_CERT_FILE'] = '/etc/ssl/cert.pem'
# Core TCP SDK compiles only the pinned Apache2 SCTP portability module; it
# does not build SDK examples or download the declared gRPC dependency graph.
peer = root/'go-diameter-peer'
build = subprocess.run([go, 'build', '-mod=readonly', '-o', str(peer), '.'], cwd=wrapper, env=build_env, capture_output=True, text=True, timeout=180)
(root/'go-build.log').write_text(build.stdout + build.stderr)
assert build.returncode == 0, build.stdout + build.stderr
python_sha = 'b5ef067db631181a06578d8b1f33e8536425b318d8612aa48479ee3f4abf934d'
python_data = fetch('python_diameter-0.9.0-py3-none-any.whl', 'https://files.pythonhosted.org/packages/8f/5c/72909b543afd7cbeb77c200afe929a9136fe43f45b794fa5063e569d64d6/python_diameter-0.9.0-py3-none-any.whl', python_sha, 512*1024)
extract(python_data, root/'python', '', 4*1024*1024, 512*1024)
assert b'MIT License' in (root/'python'/'python_diameter-0.9.0.dist-info'/'licenses'/'LICENSE').read_bytes()
py_env = os.environ.copy()
py_env['PYTHONPATH'] = str(root/'python')
subprocess.run([sys.executable, '-c', 'import importlib.metadata; from diameter.node import Node; assert importlib.metadata.version("python-diameter")=="0.9.0"'], env=py_env, check=True, timeout=10)
(root/'versions.json').write_text(json.dumps({'go_diameter':'4.5.0','go_archive_sha256':go_sha,'python_diameter':'0.9.0','python_wheel_sha256':python_sha,'source_modified':False,'licenses':{'go-diameter':'BSD-3-Clause','python-diameter':'MIT','sctp':'Apache-2.0'},'go':go_version,'python':sys.version},indent=2)+'\n')
print('export NETGET_DIAMETER_PEER='+str(peer))
print('export NETGET_DIAMETER_PYTHON='+sys.executable)
print('export PYTHONPATH='+str(root/'python'))
