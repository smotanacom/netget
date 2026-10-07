"""Unmodified bacpypes3 0.0.102 independent peer."""
import asyncio,sys,socket,json,argparse
from bacpypes3.app import Application
from bacpypes3.local.analog import AnalogValueObject
from bacpypes3.pdu import Address
async def main():
 role,host,port=sys.argv[1],sys.argv[2],int(sys.argv[3])
 s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);s.bind((host,0));local=s.getsockname()[1];s.close()
 if role=='server':port=port or local
 args=argparse.Namespace(vendoridentifier=999,instance=5678,name='Independent peer',address=f'{host}:{port if role=="server" else local}',network=0,foreign=None,bbmd=None)
 app=Application.from_args(args)
 try:
  await asyncio.sleep(.1)
  if role=='client':
   addr=Address(f'{host}:{port}')
   devices=await app.who_is(address=addr);assert any(int(d.iAmDeviceIdentifier[1])==1234 for d in devices)
   value=await app.read_property(addr,'analog-value,1','present-value');assert float(value)==12.5
   await app.write_property(addr,'analog-value,1','present-value',23.5)
   failed=False
   try:await app.read_property(addr,'analog-value,999','present-value')
   except BaseException as e:failed='unknown-object' in str(e)
   assert failed
   print(json.dumps(dict(discovery=True,read=True,write=True,error=True)))
  else:
   app.add_object(AnalogValueObject(objectIdentifier=('analog-value',1),objectName='Value',presentValue=12.5,statusFlags=[0,0,0,0],outOfService=False))
   print(json.dumps(dict(port=port)),flush=True)
   await asyncio.to_thread(sys.stdin.read)
 finally:app.close()
asyncio.run(main())
