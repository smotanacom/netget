"""Independent aioslsk peer codec with real compressed file-list messages."""
import asyncio,json,sys
from aioslsk.protocol.messages import PeerInit,PeerSharesRequest,PeerSharesReply,PeerUserInfoRequest,PeerUserInfoReply
from aioslsk.protocol.primitives import DirectoryData,FileData
async def frame(r):
 h=await r.readexactly(4);n=int.from_bytes(h,'little');assert n<=1048576
 return h+await r.readexactly(n)
def listing():return PeerSharesReply.Request([DirectoryData('share',[FileData(1,'hello.txt',5,'txt',[])])],0,[])
async def serve(r,w):
 try:
  init=PeerInit.Request.deserialize(0,await frame(r));assert init.typ=='P'
  while True:
   b=await frame(r);code=int.from_bytes(b[4:8],'little')
   if code==4:PeerSharesRequest.Request.deserialize(0,b);out=listing()
   elif code==15:PeerUserInfoRequest.Request.deserialize(0,b);out=PeerUserInfoReply.Request('NetGet',False,upload_slots=1,queue_size=0,has_slots_free=True,upload_permissions=0)
   else:raise AssertionError(code)
   w.write(out.serialize());await w.drain()
 except asyncio.IncompleteReadError:pass
 finally:w.close();await w.wait_closed()
async def main():
 if sys.argv[1]=='server':
  s=await asyncio.start_server(serve,'127.0.0.1',0);print(json.dumps({'port':s.sockets[0].getsockname()[1]}),flush=True);await asyncio.to_thread(sys.stdin.readline);s.close();await s.wait_closed()
 else:
  r,w=await asyncio.open_connection(sys.argv[2],int(sys.argv[3]));w.write(PeerInit.Request('independent','P',0).serialize()+PeerSharesRequest.Request().serialize());await w.drain()
  v=PeerSharesReply.Request.deserialize(0,await frame(r));assert v.directories[0].files[0].filename=='hello.txt'
  w.write(PeerUserInfoRequest.Request().serialize());await w.drain();assert PeerUserInfoReply.Request.deserialize(0,await frame(r)).description=='NetGet'
  w.close();await w.wait_closed();print(json.dumps({'ok':True}))
asyncio.run(main())
