"""gtk-gnutella 1.3.1 wire code; automatic host discovery disabled."""
import asyncio,json,os,pathlib,socket,subprocess,sys,tempfile,time
async def main():
 with tempfile.TemporaryDirectory(prefix='netget-gnutella-') as d:
  d=pathlib.Path(d);s=socket.socket();s.bind(('127.0.0.1',0));port=s.getsockname()[1];s.close()
  conf=f'''listen_port = {port}
stop_host_get = TRUE
configured_peermode = 2
allow_firewalled_ultra = TRUE
max_g2_hubs = 0
enable_dht = FALSE
enable_udp = FALSE
enable_upnp = FALSE
enable_natpmp = FALSE
enable_shell = TRUE
allow_private_network_connection = TRUE
force_local_ip = TRUE
forced_local_ip = "127.0.0.1"
bind_to_forced_local_ip = TRUE
gnet_deflate_enabled = FALSE
prefer_compressed_gnet = FALSE
save_file_path = "{d}/downloads"
move_downloading_files_to = "{d}/complete"
move_corrupted_files_to = "{d}/corrupt"
node_debug = 2
'''
  (d/'geo-ip.txt').write_text('');(d/'geo-ipv6.txt').write_text('');(d/'config_gnet').write_text(conf);env=dict(os.environ,GTK_GNUTELLA_DIR=str(d));exe=os.environ['NETGET_GNUTELLA']
  with (d/'stderr').open('w') as err:
   p=subprocess.Popen([exe,'--topless','--no-supervise','--no-restart'],env=env,stdout=subprocess.DEVNULL,stderr=err)
   try:
    for _ in range(550):
     if p.poll() is not None:raise AssertionError((d/'stderr').read_text())
     if (d/'ipc/socket').exists():
      try:
       r,w=await asyncio.open_connection('127.0.0.1',port);w.close();await w.wait_closed();break
      except OSError:pass
     await asyncio.sleep(.1)
    else:raise AssertionError('gtk shell readiness timeout '+(d/'stderr').read_text()[-2000:])
    if sys.argv[1]=='server':
     print(json.dumps({'port':port}),flush=True);await asyncio.to_thread(sys.stdin.readline)
    else:
     c=await asyncio.create_subprocess_exec(exe,'--shell',env=env,stdin=asyncio.subprocess.PIPE,stdout=asyncio.subprocess.PIPE,stderr=asyncio.subprocess.PIPE)
     out,e=await c.communicate(f'node add {sys.argv[2]}:{sys.argv[3]}\n'.encode());assert c.returncode==0,(out,e)
     await asyncio.sleep(2)
     print(json.dumps({'ok':True,'shell':out.decode(),'log':(d/'stderr').read_text()[-3000:]}))
   finally:
    p.terminate()
    try:await asyncio.to_thread(p.wait,3)
    except subprocess.TimeoutExpired:
     # Bound external test-process cleanup even if upstream shutdown threads stall.
     p.kill();await asyncio.to_thread(p.wait)
    print("\n".join(l for l in (d/"stderr").read_text().splitlines() if any(k in l.lower() for k in ["fatal","assert","gnutella/","node","connection","error","reject"])),file=sys.stderr)
asyncio.run(main())
