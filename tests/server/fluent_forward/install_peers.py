#!/usr/bin/env python3
"""Isolated pinned independent Forward peers; no global gems/packages or collector storage.
Requires Python 3.8+, Ruby 3.2+, gem, C compiler/make for native Ruby gems.
"""
import os, pathlib, shlex, subprocess, sys
root=pathlib.Path(sys.argv[1]).resolve()
root.mkdir(parents=True,exist_ok=True)
subprocess.run([sys.executable,'-m','pip','install','--no-cache-dir','--upgrade','--target',str(root/'python'),'fluent-logger==0.11.1','msgpack==1.1.2'],check=True)
env=os.environ.copy()
env.update(GEM_HOME=str(root/'ruby'),GEM_PATH=str(root/'ruby'),GEM_SPEC_CACHE=str(root/'gemspec-cache'),MAKEFLAGS='-j4')
# Gem uses public roots; an inherited proxy-only SSL_CERT_FILE cannot validate
# direct rubygems.org after a proxy disappears. Keep certificate verification on.
if sys.platform=='darwin' and pathlib.Path('/etc/ssl/cert.pem').is_file(): env['SSL_CERT_FILE']='/etc/ssl/cert.pem'
subprocess.run(['gem','install','--install-dir',str(root/'ruby'),'--no-document','--version','1.19.4','fluentd'],env=env,check=True)
(root/'ruby-dependencies.txt').write_bytes(subprocess.check_output(['gem','list','--local'],env=env))
print('export PYTHONPATH='+shlex.quote(str(root/'python')))
print('export NETGET_FORWARD_PYTHON='+shlex.quote(sys.executable))
print('export GEM_HOME='+shlex.quote(str(root/'ruby')))
print('export GEM_PATH='+shlex.quote(str(root/'ruby')))
print('export NETGET_FORWARD_RUBY='+shlex.quote(subprocess.check_output(['ruby','-e','print RbConfig.ruby'],text=True).strip()))
