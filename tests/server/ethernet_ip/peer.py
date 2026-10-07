"""Independent cpppo 5.2.5 scanner/adapter, with cpppo owning every wire codec."""
import sys,json,socket,importlib.metadata,time,threading
assert importlib.metadata.version('cpppo')=='5.2.5'
from cpppo.server.enip import client,main
from cpppo.server.enip.get_attribute import attribute_operations
role,host,port=sys.argv[1],sys.argv[2],int(sys.argv[3])
if role=='client':
 with client.connector(host=host,port=port,timeout=5) as conn:
  ops=list(attribute_operations(['@1/1/1','@1/1/1=(INT)42','@1/1/9'],route_path=[],send_path=''))
  rows=list(conn.synchronous(operations=ops,timeout=5))
  assert len(rows)==3,rows
  assert rows[0][4]==0 and rows[0][5]==[42,0],rows
  assert rows[1][4]==0,rows
  assert rows[2][4]==0x14,rows
  print(json.dumps({'read':True,'write':True,'error':True}))
else:
 if not port:
  s=socket.socket();s.bind((host,0));port=s.getsockname()[1];s.close()
 t=threading.Thread(target=lambda:main.main(argv=['--address',f'{host}:{port}','--simple']),daemon=True);t.start()
 for _ in range(200):
  try:
   s=socket.create_connection((host,port),timeout=.2);s.close();break
  except OSError:time.sleep(.02)
 else:raise RuntimeError('cpppo did not start')
 print(json.dumps({'port':port}),flush=True);sys.stdin.read()
