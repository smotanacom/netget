#!/usr/bin/env python3
"""Install pinned independent stacks in an explicitly owned directory (never skip)."""
import hashlib, os, pathlib, shlex, subprocess, sys, tarfile, urllib.request, venv
root=pathlib.Path(sys.argv[1]).resolve();root.mkdir(parents=True,exist_ok=True)
def run(*args):subprocess.run([str(a) for a in args],check=True)
python=root/'python'/'bin'/'python'
venv.create(root/'python',with_pip=True)
run(python,'-m','pip','install','python-snap7==3.2.1','cpppo==5.2.5','asyncua==1.1.8','bacpypes3==0.0.102')
def source(name,url,sha):
 archive=root/(name+'.tar.gz')
 if not archive.exists():
  with urllib.request.urlopen(url,timeout=60) as response:archive.write_bytes(response.read())
 if hashlib.sha256(archive.read_bytes()).hexdigest()!=sha:raise RuntimeError('source digest mismatch: '+name)
 with tarfile.open(archive) as t:
  for m in t.getmembers():
   if not (root/m.name).resolve().is_relative_to(root) or m.issym() or m.islnk():raise RuntimeError('unsafe source archive')
  t.extractall(root)
 return root/name
dnp=source('opendnp3-3.1.2','https://codeload.github.com/dnp3/opendnp3/tar.gz/refs/tags/3.1.2','183cc29222c3cb58099a9753c43defed5cb8677fda985c3f88e38b5c19a36ff2')
iec=source('lib60870-2.3.4','https://codeload.github.com/mz-automation/lib60870/tar.gz/refs/tags/v2.3.4','09a2acd3241168c23e0d6653a0cde689d2b6f4b22d7c0e3b18b38c7a728ad77d')/'lib60870-C'
run('cmake','-S',dnp,'-B',root/'dnp-build','-DDNP3_STATIC_LIBS=ON','-DCMAKE_BUILD_TYPE=Release','-DCMAKE_POLICY_VERSION_MINIMUM=3.5')
run('cmake','--build',root/'dnp-build','-j2')
run('cmake','-S',iec,'-B',root/'iec-build','-DBUILD_EXAMPLES=OFF','-DBUILD_TESTS=OFF','-DCMAKE_BUILD_TYPE=Release','-DCMAKE_POLICY_VERSION_MINIMUM=3.5')
run('cmake','--build',root/'iec-build','-j2')
repo=pathlib.Path(__file__).resolve().parents[2]
run(os.environ.get('CXX','c++'),'-std=c++14','-O2','-I'+str(dnp/'cpp/lib/include'),repo/'tests/server/dnp3/peer.cpp',root/'dnp-build/cpp/lib/libopendnp3.a','-lpthread','-o',root/'dnp-peer')
run(os.environ.get('CC','cc'),'-std=c11','-O2','-I'+str(iec/'src/inc/api'),'-I'+str(iec/'src/hal/inc'),repo/'tests/server/iec104/peer.c',root/'iec-build/src/liblib60870.a','-lpthread','-o',root/'iec-peer')
for key,path in [('NETGET_ICS_PYTHON',python),('NETGET_DNP3_PEER',root/'dnp-peer'),('NETGET_IEC104_PEER',root/'iec-peer')]:print('export '+key+'='+shlex.quote(str(path)))
