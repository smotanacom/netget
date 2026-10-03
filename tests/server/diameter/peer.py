import argparse,json,sys,time,socket,importlib.metadata
assert importlib.metadata.version("python-diameter")=="0.9.0"
from diameter.node import Node
from diameter.node.application import SimpleThreadingApplication
from diameter.message.avp import Avp
from diameter.message.commands.aa import AaRequest
from diameter.message.constants import *
p=argparse.ArgumentParser();p.add_argument('--mode',choices=['client','server'],required=True);p.add_argument('--port',type=int,default=0);p.add_argument('--password',default='Correct');p.add_argument('--request-type',type=int,default=3);a=p.parse_args()
if a.mode=='server' and a.port==0:
 with socket.socket() as s:s.bind(('127.0.0.1',0));a.port=s.getsockname()[1]
host='server.example' if a.mode=='server' else 'client.example'
node=Node(host,'example',ip_addresses=['127.0.0.1'],tcp_port=a.port if a.mode=='server' else 0,vendor_ids=[0]);node.wakeup_interval=.05
if a.mode=='server':
 peer=node.add_peer('aaa://client.example','example')
 def handler(app,request):
  assert request.auth_application_id==1 and request.auth_session_state==1
  answer=app.generate_answer(request,result_code=2001 if request.auth_request_type==2 or request.user_password==b'Correct' else 4001)
  answer.append_avp(Avp.new(AVP_AUTH_REQUEST_TYPE,value=request.auth_request_type));answer.append_avp(Avp.new(AVP_AUTH_SESSION_STATE,value=1));answer.append_avp(Avp.new(AVP_SERVICE_TYPE,value=1));return answer
 app=SimpleThreadingApplication(1,is_auth_application=True,max_threads=2,request_handler=handler)
else:
 peer=node.add_peer(f'aaa://server.example:{a.port};transport=tcp','example',ip_addresses=['127.0.0.1'],is_persistent=True)
 app=SimpleThreadingApplication(1,is_auth_application=True,max_threads=2)
node.add_application(app,[peer]);node.start()
try:
 if a.mode=='server':print(json.dumps({'ready':True,'port':node.tcp_sockets[0].getsockname()[1]}),flush=True);sys.stdin.readline()
 else:
  app.wait_for_ready(timeout=5)
  request=AaRequest();request.session_id='client.example;calibration;1';request.origin_host=b'client.example';request.origin_realm=b'example';request.destination_realm=b'example';request.auth_application_id=1;request.auth_request_type=a.request_type;request.auth_session_state=1;request.user_name='alice';request.user_password=a.password.encode() if a.request_type!=2 else None
  answer=app.send_request(request,timeout=5)
  assert [v.value for v in answer.avps if v.code==AVP_AUTH_REQUEST_TYPE]==[a.request_type]
  assert [v.value for v in answer.avps if v.code==AVP_AUTH_SESSION_STATE]==[1]
  assert answer.session_id==request.session_id
  print(json.dumps({'result_code':answer.result_code,'version':'0.9.0','source_modified':False}),flush=True)
finally:node.stop(wait_timeout=3)
