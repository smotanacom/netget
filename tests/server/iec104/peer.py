"""Launch the unchanged lib60870 2.3.4 stack through a small C++ test harness."""
import sys,os,socket,subprocess,json,time
exe=os.environ['NETGET_IEC104_PEER'];role,host,port=sys.argv[1],sys.argv[2],int(sys.argv[3])
if role=='client':
 r=subprocess.run([exe,role,host,str(port)],capture_output=True,text=True,timeout=25)
 if r.returncode:sys.stderr.write(r.stdout+r.stderr)
 assert r.returncode==0,r.returncode
 print(r.stdout.strip().splitlines()[-1])
else:
 if not port:
  s=socket.socket();s.bind((host,0));port=s.getsockname()[1];s.close()
 child=subprocess.Popen([exe,role,host,str(port)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,text=True)
 try:
  ready=json.loads(child.stdout.readline());assert ready['port']==port
  for _ in range(200):
   try:s=socket.create_connection((host,port),timeout=.1);s.close();break
   except OSError:time.sleep(.02)
  else:raise RuntimeError('OpenDNP3 did not bind')
  print(json.dumps(ready),flush=True);sys.stdin.read();child.stdin.close();assert child.wait(timeout=5)==0
 finally:
  if child.poll() is None:child.kill();child.wait()
