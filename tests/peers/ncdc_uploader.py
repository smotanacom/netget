"""An unmodified ncdc uploader joined to a local rendezvous hub fixture."""
import asyncio,json,os,pathlib,pexpect,socket,sys,tempfile,time
async def main():
 adc=os.environ.get('NETGET_UPLOAD_MODE')=='adc';ready=asyncio.Event();cid='A'*39;token='NetGetToken';sock=socket.socket();sock.bind(('127.0.0.1',0));listen=sock.getsockname()[1];sock.close()
 async def hub(r,w):
  try:
   if adc:
    w.write(b'ISUP ADBASE ADTIGR\nISID BBBB\nIINF NIIndependent\n');await w.drain()
    while True:
     b=await r.readline()
     if not b:break
     if b.startswith(b'BINF BBBB'):
      w.write(b+f'BINF CCCC ID{cid} NINetGet SS0 SF0 SL1\nDRCM CCCC BBBB ADC/1.0 {token}\n'.encode());await w.drain()
     if b.startswith(b'DCTM '):ready.set()
   else:
    w.write(b'$Lock EXTENDEDPROTOCOLABCABCABCABCABCABC Pk=Fixture|');await w.drain()
    while True:
     b=await r.readuntil(b'|')
     if b.startswith(b'$ValidateNick '):w.write(b'$Hello Independent|$HubName Fixture|');await w.drain()
     if b.startswith(b'$MyINFO') or b.startswith(b'$GetNickList'):
      w.write(b'$NickList Independent$$NetGetClient$$|$MyINFO $ALL NetGetClient NetGet<V:1,M:P,H:1/0/0,S:1>$ $DSL\x01$$0$|$RevConnectToMe NetGetClient Independent|');await w.drain()
     if b.startswith(b'$ConnectToMe '):ready.set()
  except (asyncio.IncompleteReadError,ConnectionError):pass
  finally:w.close()
 with tempfile.TemporaryDirectory(prefix='netget-ncdc-up-') as directory:
  d=pathlib.Path(directory);(d/'data').mkdir();(d/'data'/'hello.txt').write_text('Hello')
  srv=await asyncio.start_server(hub,'127.0.0.1',0);port=srv.sockets[0].getsockname()[1]
  p=pexpect.spawn(os.environ['NETGET_NCDC'],['--session-dir='+str(d/'session'),'--no-autoconnect'],env=dict(os.environ,TERM='xterm'),encoding='utf-8',timeout=1)
  async def pump():
   while p.isalive():
    try:p.read_nonblocking(65536,timeout=.01)
    except pexpect.TIMEOUT:pass
    except pexpect.EOF:break
    await asyncio.sleep(.02)
  drain=asyncio.create_task(pump())
  try:
   for cmd in ['/set log_debug true','/set nick Independent','/set active true',f'/set active_port {listen}','/set active_ip 127.0.0.1','/set local_address 127.0.0.1','/set tls_policy disabled',f'/share share {d}/data',f'/open test {"adc" if adc else "dchub"}://127.0.0.1:{port}/']:p.sendline(cmd);await asyncio.sleep(.08)
   await asyncio.wait_for(ready.wait(),8);await asyncio.sleep(2);p.sendline('/refresh');await asyncio.sleep(5)
   print(json.dumps({'port':listen,'cid':cid,'token':token}),flush=True)
   await asyncio.to_thread(sys.stdin.readline);p.sendline('/quit');await asyncio.sleep(.1)
  finally:p.close(force=True);drain.cancel();srv.close();await srv.wait_closed();print((d/'session'/'stderr.log').read_text(errors='replace'),file=sys.stderr);print('LOGS',[(f.name,f.read_text()) for f in (d/'session'/'logs').glob('*.log')],file=sys.stderr)
asyncio.run(main())
