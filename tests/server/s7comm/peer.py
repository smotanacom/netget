"""Unchanged python-snap7 3.2.1 client and server, independent of the Rust S7 codec."""
import sys, json, socket, importlib.metadata
assert importlib.metadata.version("python-snap7") == "3.2.1"
from snap7.client import Client
from snap7.server import Server
from snap7.type import Area, SrvArea
role, host, port = sys.argv[1],sys.argv[2],int(sys.argv[3])
if role == "client":
    c=Client();c.connect(host,0,2,tcp_port=port)
    assert list(c.db_read(1,0,3)) == [42,43,44]
    c.db_write(1,0,bytearray([10,11,12]))
    refused=False
    try: c.db_read(2,0,1)
    except Exception: refused=True
    assert refused, "DB2 must fail with address error"
    for area in [Area.PE,Area.PA,Area.MK]: assert list(c.read_area(area,0,0,3)) == [42,43,44]
    c.disconnect();print(json.dumps({"read":True,"write":True,"address_error":True,"areas":4}))
else:
    if not port:
        s=socket.socket();s.bind((host,0));port=s.getsockname()[1];s.close()
    server=Server();memory=bytearray([42,43,44]+[0]*253);server.register_area(SrvArea.DB,1,memory);server.start(tcp_port=port)
    print(json.dumps({"port":port}),flush=True)
    sys.stdin.read();server.stop();server.destroy()
