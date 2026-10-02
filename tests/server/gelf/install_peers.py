#!/usr/bin/env python3
"""Pinned independent GELF peers, isolated source/cache/install; no service or global install.
Graylog files remain byte-identical. A same-package export-only wrapper exposes its private
TCP constructor/address/read; upstream framing, decompression and Message decoder run unchanged.
"""
import hashlib, os, pathlib, shlex, subprocess, sys
CURL = '/usr/bin/curl' if sys.platform == 'darwin' else 'curl'
SHA = '25db8704bcf3f484c958312cd0cc49e5c768dcf1'
FILES = {'message.go': '2c9147bf85302a328ceb4624bf5d007da8dee6684e6043ab09a40066cc61a397', 'reader.go': '8bd7f48d01d7794d8522a1e63420ed5070bba18135f8f62d28cdf5fdc9ffe49f', 'tcpreader.go': '966e816eace6443bb515c7cfa62c493380c7b9ee42b39c76dc314c932102787d', 'tcpwriter.go': 'e07a1130c8dc5e9fea71f5c91c4d2f3a5a452362d9a17bb80b191492a8026211', 'udpwriter.go': '844d4dc489387ce0e3fb6ae58a9218c1a09f5ab46c95df1ddcf0e105955fbfef', 'utils.go': '6ec347774def3ca3cf96b5a4a47b295b0f6a1929ca34842b990f96f126cda826', 'writer.go': '12f7990b7de10ceea4dc1970f6d8efcfd9603144e8e33709e256a8b4fd64ef5d'}
root=pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True,exist_ok=True)
subprocess.run([sys.executable,'-m','pip','install','--no-cache-dir','--target',str(root/'python'),'pygelf==0.4.3'],check=True)
source=root/'go';(source/'gelf').mkdir(parents=True,exist_ok=True)
for name,digest in FILES.items():
 data=subprocess.check_output([CURL,'--fail','--silent','--show-error','--location','--max-time','30','https://raw.githubusercontent.com/Graylog2/go-gelf/'+SHA+'/gelf/'+name])
 if hashlib.sha256(data).hexdigest()!=digest:raise RuntimeError('upstream source hash mismatch: '+name)
 (source/'gelf'/name).write_bytes(data)
# License belongs to these reference-only source files.
(source/'LICENSE').write_bytes(subprocess.check_output([CURL,'--fail','--silent','--show-error','--location','--max-time','30','https://raw.githubusercontent.com/Graylog2/go-gelf/'+SHA+'/LICENSE']))
here=pathlib.Path(__file__).resolve().parent
(source/'gelf/netget_export.go').write_bytes((here/'peer_export.go.txt').read_bytes())
(source/'main.go').write_bytes((here/'peer_main.go.txt').read_bytes())
(source/'go.mod').write_text('module netget-peer\ngo 1.21\n')
env=os.environ.copy();env.update(GOCACHE=str(root/'gocache'),GOMODCACHE=str(root/'gomodcache'),GOTOOLCHAIN='local',GOPROXY='off')
subprocess.run(['go','build','-o',str(root/'gelf-reader'),'.'],cwd=source,env=env,check=True)
print('export PYTHONPATH='+shlex.quote(str(root/'python')))
print('export NETGET_GELF_PYTHON='+shlex.quote(sys.executable))
print('export NETGET_GELF_READER='+shlex.quote(str(root/'gelf-reader')))
