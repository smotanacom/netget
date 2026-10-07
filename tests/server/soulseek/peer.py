"""Independent aioslsk 1.6.4 central message serialization, both directions."""
import asyncio,json,sys,hashlib,struct
from aioslsk.protocol.messages import Login,RoomList,RoomChatMessage,JoinRoom,GetUserStatus,GetPeerAddress
async def read(r,cls):
 h=await r.readexactly(4); n=int.from_bytes(h,'little'); assert n<=65536
 return cls.deserialize(0,h+await r.readexactly(n))
async def serve(r,w):
 try:
  while True:
   h=await r.readexactly(4);n=int.from_bytes(h,'little');b=h+await r.readexactly(n);code=int.from_bytes(b[4:8],'little')
   if code==1:
    v=Login.Request.deserialize(0,b);assert v.md5hash==hashlib.md5((v.username+v.password).encode()).hexdigest()
    out=Login.Response(True,'NetGet','127.0.0.1',hashlib.md5(v.password.encode()).hexdigest(),False)
   elif code==64:RoomList.Request.deserialize(0,b);out=RoomList.Response(['NetGet'],[0],[],[],[],[],[])
   elif code==13:
    v=RoomChatMessage.Request.deserialize(0,b);out=RoomChatMessage.Response(v.room,'netget',v.message)
   else:raise AssertionError(code)
   w.write(out.serialize());await w.drain()
 except asyncio.IncompleteReadError:pass
 finally:w.close();await w.wait_closed()
async def main():
 if sys.argv[1]=='server':
  s=await asyncio.start_server(serve,'127.0.0.1',0);print(json.dumps({'port':s.sockets[0].getsockname()[1]}),flush=True)
  await asyncio.to_thread(sys.stdin.readline);s.close();await s.wait_closed()
 else:
  r,w=await asyncio.open_connection(sys.argv[2],int(sys.argv[3]));w.write(Login.Request('netget','test',175,hashlib.md5(b'netgettest').hexdigest(),1).serialize());await w.drain();assert (await read(r,Login.Response)).success
  w.write(RoomList.Request().serialize());await w.drain();assert (await read(r,RoomList.Response)).rooms==['NetGet']
  w.write(JoinRoom.Request('NetGet').serialize());await w.drain();assert (await read(r,JoinRoom.Response)).room=='NetGet'
  w.write(RoomChatMessage.Request('NetGet','Hi').serialize());await w.drain();assert (await read(r,RoomChatMessage.Response)).message=='Hi'
  w.write(GetUserStatus.Request('netget').serialize());await w.drain();assert (await read(r,GetUserStatus.Response)).status==0
  w.write(GetPeerAddress.Request('netget').serialize());await w.drain();assert (await read(r,GetPeerAddress.Response)).ip=='0.0.0.0'
  w.close();await w.wait_closed();print(json.dumps({'ok':True}))
asyncio.run(main())
