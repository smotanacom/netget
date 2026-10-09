"""Unmodified pinned test-only IPFIX peers; never link LGPL Python into NetGet."""
import hashlib, io, json, pathlib, platform, ssl, subprocess, sys, tarfile, urllib.request
root=pathlib.Path(sys.argv[1]).resolve();root.mkdir(parents=True,exist_ok=True)
ctx=ssl.create_default_context(cafile='/etc/ssl/cert.pem' if sys.platform=='darwin' else None)
def fetch(url,limit):
 req=urllib.request.Request(url,headers={'User-Agent':'netget-owned-independent-peer'})
 with urllib.request.urlopen(req,context=ctx,timeout=60) as r:
  b=r.read(limit+1)
 assert len(b)<=limit,'peer download byte bound'
 return b
targets={('darwin','arm64'):('darwin-arm64','6bc188842983edbf2df26788180bce3ee801788fec3aebedcff1f5d96eb9fd57'),('linux','x86_64'):('linux-amd64','63d3bb6c4e458f56ae3268eac74979da1fee0cea5342362295e7d587784af14b')}
key=(sys.platform,platform.machine());assert key in targets,'bootstrap explicitly supports Linuxamd64/macOSarm64 only'
target,digest=targets[key];binary=root/'goflow2-v2.2.7'
data=binary.read_bytes() if binary.exists() else fetch(f'https://github.com/netsampler/goflow2/releases/download/v2.2.7/goflow2-2.2.7-{target}',26*1024*1024)
assert hashlib.sha256(data).hexdigest()==digest,'official GoFlow2 binary SHA256 mismatch';binary.write_bytes(data);binary.chmod(0o755)
license_data=fetch('https://raw.githubusercontent.com/netsampler/goflow2/v2.2.7/LICENSE',16384);assert b'BSD 3-Clause License' in license_data;(root/'goflow2-LICENSE').write_bytes(license_data)
version=subprocess.check_output([str(binary),'-v'],stderr=subprocess.STDOUT,text=True);assert 'GoFlow2 v2.2.7 ' in version;(root/'goflow2-version.txt').write_text(version)
sha='31b16fc288819878c2c1845aa1832c8105de47dfbde6af5a5c0500d09b0e940b'
meta=json.loads(fetch('https://pypi.org/pypi/ipfix/0.9.7/json',128*1024));assets=[a for a in meta['urls'] if a['filename']=='ipfix-0.9.7.tar.gz'];assert len(assets)==1 and assets[0]['digests']['sha256']==sha
archive=root/'ipfix-0.9.7.tar.gz';b=archive.read_bytes() if archive.exists() else fetch(assets[0]['url'],128*1024);assert hashlib.sha256(b).hexdigest()==sha;archive.write_bytes(b)
destination=root/'python'/'ipfix';destination.mkdir(parents=True,exist_ok=True);count=0
with tarfile.open(fileobj=io.BytesIO(b),mode='r:gz') as a:
 for m in a.getmembers():
  p=pathlib.PurePosixPath(m.name)
  if not m.isfile() or not p.parts or p.parts[0]!='ipfix-0.9.7':continue
  if len(p.parts)==3 and p.parts[1]=='ipfix' and p.suffix in ('.py','.iespec'):
   content=a.extractfile(m).read();(destination/p.name).write_bytes(content);count+=1
  elif len(p.parts)==2 and p.name=='LICENSE.txt':
   license_data=a.extractfile(m).read();assert b'GNU LESSER GENERAL PUBLIC LICENSE' in license_data.upper();(root/'ipfix-LICENSE.txt').write_bytes(license_data)
  elif len(p.parts)==2 and p.name=='PKG-INFO':assert b'Version: 0.9.7' in a.extractfile(m).read()
assert count>=10 and (root/'ipfix-LICENSE.txt').exists(),'complete unmodified peer package required'
subprocess.run([sys.executable,'-c','import sys;sys.path.insert(0,sys.argv[1]);from ipfix import ie,message;ie.use_iana_default();assert ie.for_spec("sourceIPv4Address").num==8;print("ipfix0.9.7 public API imported")',str(root/'python')],check=True)
(root/'versions.json').write_text(json.dumps({'goflow2':'2.2.7','ipfix':'0.9.7','ipfix_source_sha256':sha},indent=2)+'\n')
print('export PYTHONPATH='+str(root/'python'));print('export NETGET_IPFIX_PYTHON='+sys.executable);print('export NETGET_IPFIX_COLLECTOR='+str(binary))
