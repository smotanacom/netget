"""Install isolated official Python emitter and official-decoder HTTP adapter, no global state."""
import hashlib, io, os, pathlib, shlex, ssl, subprocess, sys, urllib.request, zipfile
root = pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
here = pathlib.Path(__file__).resolve().parent
ctx = ssl.create_default_context(cafile=os.environ.get('NETGET_PEER_CA', '/etc/ssl/cert.pem') if sys.platform == 'darwin' else None)
url = 'https://proxy.golang.org/github.com/influxdata/line-protocol/v2/@v/v2.2.1.zip'
data = urllib.request.urlopen(url, context=ctx, timeout=30).read()
assert hashlib.sha256(data).hexdigest() == 'a3a0dc732f3c7e1dce136bfb04f39dc18e222a1b9817aa6f7307f45cf27e435f'
z = zipfile.ZipFile(io.BytesIO(data))
prefix = 'github.com/influxdata/line-protocol/v2@v2.2.1/'
source = root / 'go'
(source / 'lineprotocol').mkdir(parents=True, exist_ok=True)
license_text = z.read(prefix + 'LICENSE')
assert b'MIT License' in license_text and b'InfluxData' in license_text
(source / 'LICENSE').write_bytes(license_text)
for item in z.namelist():
    relative = item.removeprefix(prefix)
    if relative.startswith('lineprotocol/') and relative.endswith('.go') and not relative.endswith('_test.go'):
        (source / relative).write_bytes(z.read(item))
(source / 'go.mod').write_text('module netget-influx-peer\n\ngo 1.21\n')
(source / 'main.go').write_bytes((here / 'peer_receiver.go.txt').read_bytes())
env = dict(os.environ, GOCACHE=str(root/'go-cache'), GOMODCACHE=str(root/'go-modcache'), GOPROXY='off', GOTOOLCHAIN='local', GOFLAGS='-p=4')
subprocess.run(['go', 'build', '-o', str(root/'influx-decoder-receiver'), '.'], cwd=source, env=env, check=True)
env = dict(os.environ)
if sys.platform == 'darwin': env['PIP_CERT'] = os.environ.get('NETGET_PEER_CA', '/etc/ssl/cert.pem')
pins = ['influxdb-client==1.50.0', 'reactivex==5.1.0', 'certifi==2026.7.22', 'python-dateutil==2.9.0.post0', 'urllib3==2.8.0', 'six==1.17.0', 'typing-extensions==4.16.0']
subprocess.run([sys.executable, '-m', 'pip', 'install', '--no-cache-dir', '--target', str(root/'python'), *pins], env=env, check=True)
licenses = list((root/'python'/'influxdb_client-1.50.0.dist-info').rglob('LICENSE*'))
assert any(b'MIT License' in f.read_bytes() for f in licenses), 'official emitter license missing'
subprocess.run([sys.executable,'-c',"import importlib.metadata as m;assert m.version('influxdb-client')=='1.50.0';print(m.metadata('influxdb-client')['License'])"], env=dict(env,PYTHONPATH=str(root/'python')), check=True)
(root/'versions.txt').write_text(subprocess.check_output([sys.executable,'-m','pip','list','--format=freeze','--path',str(root/'python')],env=env,text=True))
print('export PYTHONPATH='+shlex.quote(str(root/'python')))
print('export NETGET_INFLUX_PYTHON='+shlex.quote(sys.executable))
print('export NETGET_INFLUX_RECEIVER='+shlex.quote(str(root/'influx-decoder-receiver')))
