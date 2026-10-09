"""Pinned independent peers in owned storage: Linux amd64 or native macOS arm64; no containers/global installs."""
import hashlib, io, json, pathlib, platform, ssl, subprocess, sys, urllib.request, zipfile
root=pathlib.Path(sys.argv[1]).resolve();root.mkdir(parents=True,exist_ok=True)
ctx=ssl.create_default_context(cafile='/etc/ssl/cert.pem' if sys.platform=='darwin' else None)
if sys.platform=='darwin' and platform.machine()=='arm64':
 target='darwin-arm64';digests=['95d830437482aba989a7d7a65be3c618a67a6e46f6e614544cc4b9ce09f499a1','9709de08e15ef4307ce52dd5c306edf0d115db02119a712c39c00a04e013e88d']
elif sys.platform=='linux' and platform.machine() in ('x86_64','AMD64'):
 target='linux-amd64';digests=['62aea42c9cba52cd1642b3666ab37019a0ce4c24ab50b07e85dccc8d812f7d61','451fe650e8277d22d69cb8db50bba809f581fe78decba7fce4027ef185457be9']
else:
 raise SystemExit('Pinned Loki/Alloy bootstrap supports Linux amd64/macOS arm64 only; unsupported platforms explicitly fail')
for stem,tag,sha,marker in [('loki','v3.7.8',digests[0],b'GNU AFFERO'),('alloy','v1.20.1',digests[1],b'Apache License')]:
 name=stem+'-'+target+'.zip';archive_path=root/name
 if not archive_path.exists():
  req=urllib.request.Request(f'https://github.com/grafana/{stem}/releases/download/{tag}/{name}',headers={'User-Agent':'curl/8.7.1'})
  archive_path.write_bytes(urllib.request.urlopen(req,context=ctx,timeout=60).read())
 data=archive_path.read_bytes();assert hashlib.sha256(data).hexdigest()==sha,'peer SHA256 mismatch'
 with zipfile.ZipFile(io.BytesIO(data)) as archive:
  members=[n for n in archive.namelist() if pathlib.PurePosixPath(n).name==stem+'-'+target];assert len(members)==1
  binary=root/(stem+'-'+tag);binary.write_bytes(archive.read(members[0]));binary.chmod(0o755)
 license_data=urllib.request.urlopen(f'https://raw.githubusercontent.com/grafana/{stem}/{tag}/LICENSE',context=ctx,timeout=30).read();assert marker in license_data
 (root/(stem+'-LICENSE')).write_bytes(license_data)
 version=subprocess.check_output([str(binary),'-version'] if stem=='loki' else [str(binary),'--version'],stderr=subprocess.STDOUT,text=True);assert tag.removeprefix('v') in version
 (root/(stem+'-version.txt')).write_text(version)
 print('export NETGET_'+stem.upper()+'_PEER='+str(binary))
packages=['python-logging-loki==0.3.1','requests==2.34.2','rfc3339==6.2','charset-normalizer==3.5.2','idna==3.20','urllib3==2.8.0','certifi==2026.7.22']
subprocess.run([sys.executable,'-m','pip','install','--disable-pip-version-check','--no-cache-dir','--no-deps','--target',str(root/'python'),*packages],check=True)
subprocess.run([sys.executable,'-c','import importlib.metadata as m,json,sys;sys.path.insert(0,sys.argv[1]);packages=sys.argv[2:];out={p:m.version(p) for p in packages};assert out["python-logging-loki"]=="0.3.1";print(json.dumps(out))',str(root/'python'),*[p.split('==')[0] for p in packages]],check=True)
license_files=list((root/'python'/'python_logging_loki-0.3.1.dist-info').rglob('*LICENSE*'));assert any(b'MIT License' in p.read_bytes() for p in license_files if p.is_file())
(root/'versions.json').write_text(json.dumps({'loki':'3.7.8','alloy':'1.20.1','python':packages},indent=2)+'\n')
print('export PYTHONPATH='+str(root/'python'));print('export NETGET_LOKI_PYTHON='+sys.executable)
