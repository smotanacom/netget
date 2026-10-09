"""Independent stdlib reader and nntpserver 0.0.3 server, with verified TLS."""
import nntplib,sys,json,io
if sys.argv[1]=='server':
 import tempfile,pathlib,subprocess,threading
 from nntpserver.nntpserver import NNTPServer,NNTPConnectionHandler,NNTPPostSetting,NNTPAuthSetting,NNTPAuthenticationError
 class Server(NNTPServer):
  daemon_threads=True
  def refresh(self):pass
  @property
  def groups(self):return {}
  @property
  def articles(self):return {}
  def article(self,key):raise KeyError(key)
  def auth_user(self,user,password):
   if user=='reader' and password=='secret':return b'authorized'
   raise NNTPAuthenticationError('Invalid credentials')
  def post(self,auth_token,lines):
   assert auth_token==b'authorized';assert '.first' in lines
 with tempfile.TemporaryDirectory(prefix='netget-nntps-') as d:
  d=pathlib.Path(d);cert=d/'cert.pem';key=d/'key.pem'
  subprocess.run(['openssl','req','-x509','-newkey','rsa:2048','-nodes','-days','1','-subj','/CN=localhost','-addext','subjectAltName=DNS:localhost','-addext','basicConstraints=critical,CA:FALSE','-keyout',str(key),'-out',str(cert)],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
  server=Server(('127.0.0.1',0),NNTPConnectionHandler,auth=NNTPAuthSetting.SECUREONLY,can_post=NNTPPostSetting.POST|NNTPPostSetting.AUTHREQUIRED,use_ssl=True,certfile=str(cert),keyfile=str(key))
  thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start();print(json.dumps({'port':server.server_address[1],'ca':str(cert)}),flush=True);sys.stdout=sys.stderr
  sys.stdin.readline();server.shutdown();server.server_close();thread.join()
else:
 with nntplib.NNTP(sys.argv[2],int(sys.argv[3]),readermode=True,timeout=10) as n:
  assert 'POST' in n.getcapabilities();assert 'STREAMING' in n.getcapabilities()
  article=b'From: test@localhost\r\nNewsgroups: misc.test\r\nSubject: Test\r\nMessage-ID: <test@localhost>\r\n\r\n.first\r\n\r\n'
  assert n.post(io.BytesIO(article)).startswith('240')
  assert n.ihave('<test@localhost>',io.BytesIO(article)).startswith('235')
  assert n._shortcmd('MODE STREAM').startswith('203')
  assert n._shortcmd('CHECK <test@localhost>').startswith('238')
  n._putcmd('TAKETHIS <test@localhost>');n.file.write(article.replace(b'\r\n.',b'\r\n..')+b'.\r\n');n.file.flush();assert n._getresp().startswith('239')
 print(json.dumps({'ok':True}))
