"""Unmodified ncdc client / uhub 0.8.0 hub, never NetGet on the peer side."""
import asyncio,json,os,socket,subprocess,sys,tempfile,time
from pathlib import Path
async def main():
 if sys.argv[1]=='server':
  with tempfile.TemporaryDirectory(prefix='netget-uhub-') as d:
   d=Path(d);s=socket.socket();s.bind(('127.0.0.1',0));port=s.getsockname()[1];s.close()
   (d/'acl').write_text('');(d/'plugins').write_text('')
   (d/'hub.conf').write_text(f'server_bind_addr=127.0.0.1\nserver_port={port}\nfile_acl={d}/acl\nfile_plugins={d}/plugins\nshow_banner=0\nhub_name=Independent\n')
   exe=os.environ['NETGET_UHUB'];p=subprocess.Popen([exe,'-c',str(d/'hub.conf')],stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
   try:
    for _ in range(100):
     try:r,w=await asyncio.open_connection('127.0.0.1',port);w.close();await w.wait_closed();break
     except OSError:
      if p.poll() is not None:raise AssertionError(p.stderr.read().decode())
      await asyncio.sleep(.05)
    else:raise AssertionError('uhub readiness timeout')
    print(json.dumps({'port':port}),flush=True);await asyncio.to_thread(sys.stdin.readline)
   finally:p.terminate();p.wait(timeout=5)
 else:
  import pexpect
  with tempfile.TemporaryDirectory(prefix='netget-ncdc-') as d:
   p=pexpect.spawn(os.environ.get('NETGET_NCDC','ncdc'),['--session-dir='+d,'--no-autoconnect'],env=dict(os.environ,TERM='xterm'),encoding='utf-8',timeout=1)
   try:
    p.sendline('/set nick Independent');p.sendline(f'/open test adc://{sys.argv[2]}:{sys.argv[3]}/')
    output='';deadline=time.monotonic()+10
    while time.monotonic()<deadline:
     try:output+=p.read_nonblocking(65536,timeout=.2)
     except pexpect.TIMEOUT:pass
     if 'Logged in' in output or 'Connected.' in output or 'NetGet' in output and 'Users' in output:break
    # Login state is also verified by the NetGet access log in the Rust test.
    p.sendline('HelloIndependent');await asyncio.sleep(.2)
    for _ in range(8):
     try:output+=p.read_nonblocking(65536,timeout=.1)
     except pexpect.TIMEOUT:pass
    p.sendline('/quit');p.expect(pexpect.EOF,timeout=5)
    logs='\n'.join(f.read_text() for f in Path(d,'logs').glob('*.log'))
    assert 'HelloIndependent' in logs,(logs,output[-4000:])
    assert 'Disconnected: ' not in logs,(logs,output[-4000:])
    print(json.dumps({'ok':True,'logs':logs}))
   finally:p.close(force=True)
asyncio.run(main())
