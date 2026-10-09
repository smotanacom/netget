"""Unmodified asyncua 1.1.8, independent of NetGet's Rust stack."""
import asyncio,sys,socket,json
from asyncua import Client,Server,ua,uamethod
@uamethod
def method(parent,value):return value*2
class Changes:
 def __init__(self):self.values=[]
 def datachange_notification(self,node,value,data):self.values.append(value)
async def main():
 role,host,port=sys.argv[1],sys.argv[2],int(sys.argv[3])
 if role=='client':
  async with Client(f'opc.tcp://{host}:{port}/') as c:
   device=c.get_node('ns=2;s=Device');value=c.get_node('ns=2;s=Value')
   assert len(await device.get_children())>=2
   assert await value.read_value()==12.5
   changes=Changes();sub=await c.create_subscription(100,changes);await sub.subscribe_data_change(value)
   for _ in range(200):
    if 12.5 in changes.values:break
    await asyncio.sleep(.01)
   assert 12.5 in changes.values
   await value.write_value(23.5,ua.VariantType.Double)
   for _ in range(200):
    if 23.5 in changes.values:break
    await asyncio.sleep(.01)
   assert 23.5 in changes.values
   assert await value.read_value()==12.5 # writes notify, handlers own future read values
   assert await device.call_method(ua.NodeId('Method',2),ua.Variant(3.,ua.VariantType.Double))==6.
   try:await c.get_node('ns=2;s=missing').read_value();raise AssertionError('unknown node accepted')
   except ua.UaStatusCodeError as e:assert e.code==ua.StatusCodes.BadNodeIdUnknown
   try:await value.write_value('bad',ua.VariantType.String);raise AssertionError('wrong type accepted')
   except ua.UaStatusCodeError as e:assert e.code==ua.StatusCodes.BadTypeMismatch
   await sub.delete()
   print(json.dumps(dict(browse=True,read=True,write=True,method=True,subscription=True)))
 else:
  if not port:
   s=socket.socket();s.bind((host,0));port=s.getsockname()[1];s.close()
  server=Server();await server.init();server.set_endpoint(f'opc.tcp://{host}:{port}/');server.set_security_policy([ua.SecurityPolicyType.NoSecurity]);ns=await server.register_namespace('urn:independent:peer');assert ns==2
  device=await server.nodes.objects.add_object(ua.NodeId('Device',ns),'Device');value=await device.add_variable(ua.NodeId('Value',ns),'Value',12.5);await value.set_writable();await device.add_method(ua.NodeId('Method',ns),'Method',method,[ua.VariantType.Double],[ua.VariantType.Double])
  async with server:
   print(json.dumps(dict(port=port)),flush=True);await asyncio.to_thread(sys.stdin.read)
asyncio.run(main())
