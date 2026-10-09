"""ncdc file-list and content downloader, with a minimal hub rendezvous fixture."""
import asyncio,json,os,pathlib,sys,tempfile,time
if sys.argv[1]=='server':
 os.environ['NETGET_UPLOAD_MODE']='adc'
 import runpy
 runpy.run_path(str(pathlib.Path(__file__).parents[2]/'peers'/'ncdc_uploader.py'))
 raise SystemExit
async def main():
 import pexpect
 targetport=int(sys.argv[3]);adc=True
 r,w=await asyncio.open_connection(sys.argv[2],targetport);w.write(b'CSUP ADBASE ADTIGR\n');await w.drain();assert (await r.readline()).startswith(b'CSUP');cid=(await r.readline()).decode().strip().split(' ID')[1];w.close();await w.wait_closed()
 async def hub(r,w):
  try:
   if adc:
    w.write(b'ISUP ADBASE ADTIGR\nISID BBBB\nIINF NIIndependent\n');await w.drain()
    while True:
     b=await r.readline();print("HUB",repr(b),file=sys.stderr)
     if not b:break
     if b.startswith(b'BINF BBBB'):
      w.write(b+f'BINF CCCC ID{cid} NINetGet I4127.0.0.1 SUTCP4 SS5 SF1 SL1\n'.encode());await w.drain()
     if b.startswith(b'DRCM '):
      token=b.decode().split()[-1];w.write(f'DCTM CCCC BBBB ADC/1.0 {targetport} {token}\n'.encode());await w.drain()
   else:
    w.write(b'$Lock EXTENDEDPROTOCOLABCABCABCABCABCABC Pk=Fixture|');await w.drain()
    while True:
     b=await r.readuntil(b'|')
     if b.startswith(b'$ValidateNick '):w.write(b'$Hello Independent|$HubName Fixture|');await w.drain()
     if b.startswith(b'$GetNickList') or b.startswith(b'$MyINFO'):
      w.write(b'$NickList Independent$$NetGet$$|$MyINFO $ALL NetGet NetGet<V:1,M:A,H:1/0/0,S:1>$ $DSL\x01$$5$|');await w.drain()
     if b.startswith(b'$RevConnectToMe '):w.write(f'$ConnectToMe Independent 127.0.0.1:{targetport}|'.encode());await w.drain()
  except (asyncio.IncompleteReadError,ConnectionError):pass
  finally:w.close()
 with tempfile.TemporaryDirectory(prefix='netget-ncdc-file-') as d:
  srv=await asyncio.start_server(hub,'127.0.0.1',0);port=srv.sockets[0].getsockname()[1]
  p=pexpect.spawn(os.environ['NETGET_NCDC'],['--session-dir='+d,'--no-autoconnect'],env=dict(os.environ,TERM='xterm'),encoding='utf-8',timeout=1)
  try:
   p.sendline('/set log_debug true');p.sendline('/set nick Independent');p.sendline(f'/open test {"adc" if adc else "dchub"}://127.0.0.1:{port}/')
   async def pump(seconds):
    deadline=time.monotonic()+seconds
    while time.monotonic()<deadline:
     try:p.read_nonblocking(65536,timeout=.01)
     except pexpect.TIMEOUT:pass
     except pexpect.EOF:return
     await asyncio.sleep(.02)
   await pump(1);p.sendline('/browse -f NetGet');await pump(4)
   lists=list(pathlib.Path(d,'fl').glob('*'));assert lists,[(f.name,f.read_text()) for f in pathlib.Path(d,'logs').glob('*.log')]+[('stderr',(pathlib.Path(d)/'stderr.log').read_text())]
   assert any(f.stat().st_size>0 for f in lists)
   p.send('\x1b3');await pump(.2);p.send('d')
   content=pathlib.Path(d,'dl','share','hello.txt')
   deadline=time.monotonic()+8
   while time.monotonic()<deadline and not content.exists():await pump(.1)
   assert content.exists(),[(f.name,f.read_text(errors='replace')) for f in pathlib.Path(d,'logs').glob('*.log')]+[('stderr',(pathlib.Path(d)/'stderr.log').read_text(errors='replace'))]
   assert content.read_bytes()==b'Hello',content.read_bytes()
   p.sendline('/quit');await pump(.3);p.close(force=True);print(json.dumps({'ok':True,'filelists':[f.name for f in lists]}))
  finally:p.close(force=True);srv.close();await srv.wait_closed()
asyncio.run(main())
