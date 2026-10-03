use super::common::*;
use crate::helpers::real_server::{InstallHint, RealServer};
use serde_json::{json, Value};
use std::time::Duration;
pub(super) const PASSWORD: &str = "netget-owned-oci-password62";
pub(super) const TOKEN: &str = "netget-owned-oci-token62";
const PEER: &str = r#"import base64,http.client,http.server,json,pathlib,sys,threading,urllib.parse
front,token,backend=sys.argv[1:]
password='netget-owned-oci-password62';bearer='netget-owned-oci-token62'
seen={'token_requests':0,'backend_requests':0,'basic_verified':False,'last_scope':[],'authorization_present':False}
class Handler(http.server.BaseHTTPRequestHandler):
 def log_message(self,*a):pass
 def reply(self,status,body,headers={}):
  self.send_response(status)
  for k,v in headers.items():self.send_header(k,v)
  self.send_header('Content-Length',str(len(body)));self.end_headers()
  if self.command!='HEAD':self.wfile.write(body)
 def do_GET(self):
  if self.server.server_port==int(token) or (backend=='pair' and self.path.startswith('/token?')):
   seen['token_requests']+=1;q=urllib.parse.parse_qs(urllib.parse.urlsplit(self.path).query)
   seen['last_scope']=q.get('scope',[])
   auth=self.headers.get('Authorization','')
   expected='Basic '+base64.b64encode(('reader:'+password).encode()).decode()
   seen['basic_verified']=auth==expected
   accepted=auth=='' if backend=='anonymous' else auth==expected
   if not accepted or q.get('service')!=['registry.test']:
    self.reply(401,json.dumps({'note':password}).encode(),{'Content-Type':'application/json'});return
   self.reply(200,json.dumps({'token':bearer,'access_token':bearer,'expires_in':1}).encode(),{'Content-Type':'application/json'});return
  if self.path=='/observations':self.reply(200,json.dumps(seen).encode(),{'Content-Type':'application/json'});return
  if backend=='pair':
   seen['backend_requests']+=1;seen['authorization_present']='Authorization' in self.headers
   host,port=pathlib.Path(__file__).with_name('backend.txt').read_text().strip().split(':')
   c=http.client.HTTPConnection(host,int(port),timeout=5)
   headers={k:v for k,v in self.headers.items() if k.lower() not in ['host','connection']}
   c.request(self.command,self.path,headers=headers);r=c.getresponse();body=r.read(4194305)
   headers={k:v for k,v in r.getheaders() if k.lower() not in ['content-length','connection','transfer-encoding']}
   self.reply(r.status,body,headers);c.close();return
  authorized=self.headers.get('Authorization')=='Bearer '+bearer;seen['authorization_present']='Authorization' in self.headers
  if not authorized:
   scope='' if self.path=='/v2/' else ',scope="repository:library/demo:pull"'
   challenge='Bearer realm="http://127.0.0.1:'+token+'/token",service="registry.test"'+scope
   self.reply(401,b'{"errors":[{"code":"UNAUTHORIZED","message":"token required"}]}',{'Content-Type':'application/json','WWW-Authenticate':challenge});return
  if backend in ['0','anonymous']:
   tags=['latest'] if backend=='anonymous' else [password,bearer]
   self.reply(200,json.dumps({'name':'library/demo','tags':tags}).encode(),{'Content-Type':'application/json'});return
  seen['backend_requests']+=1;host,port=backend.split(':');c=http.client.HTTPConnection(host,int(port),timeout=5)
  c.request(self.command,self.path,headers={'Accept':self.headers.get('Accept','*/*')});r=c.getresponse();body=r.read(4194305)
  headers={k:v for k,v in r.getheaders() if k.lower() not in ['content-length','connection','transfer-encoding']}
  self.reply(r.status,body,headers);c.close()
 do_HEAD=do_GET
threading.Thread(target=http.server.ThreadingHTTPServer(('127.0.0.1',int(token)),Handler).serve_forever,daemon=True).start()
server=http.server.ThreadingHTTPServer(('127.0.0.1',int(front)),Handler);print('OCI_AUTH_READY',flush=True);server.serve_forever()
"#;
pub(super) async fn peer() -> crate::helpers::E2EResult<RealServer> {
    peer_mode("0").await
}
pub(super) async fn pair_peer() -> crate::helpers::E2EResult<RealServer> {
    peer_mode("pair").await
}
async fn peer_mode(mode: &str) -> crate::helpers::E2EResult<RealServer> {
    RealServer::builder(
        "python3",
        InstallHint {
            brew: "python3",
            apt: "python3",
        },
    )
    .config_file("auth_peer.py", PEER)
    .extra_ports(1)
    .args(["-u", "{dir}/auth_peer.py", "{port}", "{port1}", mode])
    .ready_when_log_matches("OCI_AUTH_READY")
    .start()
    .await
}
#[tokio::test]
async fn native_anonymous_token_exchange_sends_no_basic_and_requires_a_later_pull(
) -> crate::helpers::E2EResult<()> {
    let peer = peer_mode("anonymous").await?;
    let state = state();
    let id = client(
        &state,
        format!("http://{}", peer.addr()),
        json!({"trusted_token_origin":format!("http://127.0.0.1:{}",peer.extra_ports[0])}),
        vec![static_handler("*", json!([]))],
    )
    .await;
    event(&state, id, "oci_connected", 0).await;
    let before = latest(&state, id).await;
    send(
        &state,
        id,
        json!({"type":"oci_request","operation":"tags","repository":"library/demo"}),
    )
    .await;
    event(&state, id, "oci_auth_challenge", before).await;
    let before = latest(&state, id).await;
    send(&state, id, json!({"type":"oci_authenticate"})).await;
    let auth = event(&state, id, "oci_authentication", before).await.1;
    assert_eq!(auth["data"]["token_received"], true);
    assert_eq!(auth["data"]["registry_authorization_verified"], false);
    let seen = observations(&peer).await;
    assert_eq!(seen["token_requests"], 1);
    assert_eq!(seen["basic_verified"], false);
    assert_eq!(seen["authorization_present"], false);
    assert_eq!(seen["last_scope"], json!(["repository:library/demo:pull"]));
    let before = latest(&state, id).await;
    send(
        &state,
        id,
        json!({"type":"oci_request","operation":"tags","repository":"library/demo"}),
    )
    .await;
    assert_eq!(
        event(&state, id, "oci_result", before).await.1["data"]["tags"],
        json!(["latest"])
    );
    assert_eq!(observations(&peer).await["authorization_present"], true);
    state.remove_client(id).await;
    Ok(())
}
pub(super) async fn observations(peer: &RealServer) -> Value {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap()
        .get(format!("http://{}/observations", peer.addr()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}
#[tokio::test]
async fn explicit_native_challenge_exchange_honors_origin_credentials_expiry_and_no_replay(
) -> crate::helpers::E2EResult<()> {
    let peer = peer().await?;
    let origin = format!("http://{}", peer.addr());
    let state = state();
    let id = client(
        &state,
        origin.clone(),
        json!({}),
        vec![static_handler("*", json!([]))],
    )
    .await;
    let connected = event(&state, id, "oci_connected", 0).await.1;
    assert_eq!(connected["data"]["status"], 401);
    assert_eq!(connected["authentication_verified"], false);
    rejected(
        &state,
        id,
        json!({"type":"oci_authenticate","username":"reader","password":PASSWORD}),
        "trust",
    )
    .await;
    assert_eq!(observations(&peer).await["token_requests"], 0);
    state.remove_client(id).await;
    let id = client(
        &state,
        origin,
        json!({"trusted_token_origin":format!("http://127.0.0.1:{}",peer.extra_ports[0])}),
        vec![static_handler("*", json!([]))],
    )
    .await;
    event(&state, id, "oci_connected", 0).await;
    // A read first establishes the exact repository pull scope from a native401.
    let before = latest(&state, id).await;
    send(
        &state,
        id,
        json!({"type":"oci_request","operation":"tags","repository":"library/demo"}),
    )
    .await;
    event(&state, id, "oci_auth_challenge", before).await;
    let before = latest(&state, id).await;
    assert!(state
        .send_to_client(
            id,
            json!({"type":"oci_authenticate","username":"reader","password":"wrong-private"}),
            Duration::from_secs(2)
        )
        .await
        .is_err());
    let refusal = event(&state, id, "oci_request_error", before).await.1;
    assert_eq!(refusal["category"], "transport_or_schema");
    assert_eq!(observations(&peer).await["basic_verified"], false);
    let before = latest(&state, id).await;
    send(
        &state,
        id,
        json!({"type":"oci_authenticate","username":"reader","password":PASSWORD}),
    )
    .await;
    let auth = event(&state, id, "oci_authentication", before).await.1;
    assert_eq!(auth["data"]["token_received"], true);
    assert_eq!(auth["data"]["registry_authorization_verified"], false);
    let seen = observations(&peer).await;
    assert_eq!(seen["token_requests"], 2);
    assert_eq!(seen["basic_verified"], true);
    assert_eq!(seen["last_scope"], json!(["repository:library/demo:pull"]));
    assert_eq!(
        seen["authorization_present"], false,
        "auth must not replay the pull"
    );
    let before = latest(&state, id).await;
    send(
        &state,
        id,
        json!({"type":"oci_request","operation":"tags","repository":"library/demo"}),
    )
    .await;
    let result = event(&state, id, "oci_result", before).await.1;
    assert_eq!(result["data"]["tags"], json!(["<redacted>", "<redacted>"]));
    assert_eq!(observations(&peer).await["authorization_present"], true);
    // Observe expiry through the peer's missing Authorization and a new native401.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let before = latest(&state, id).await;
            send(
                &state,
                id,
                json!({"type":"oci_request","operation":"tags","repository":"library/demo"}),
            )
            .await;
            if observations(&peer).await["authorization_present"] == false {
                event(&state, id, "oci_auth_challenge", before).await;
                break;
            }
            event(&state, id, "oci_result", before).await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(observations(&peer).await["authorization_present"], false);
    assert_eq!(
        observations(&peer).await["token_requests"],
        2,
        "expiry does not auto-refresh"
    );
    for e in state
        .list_access_logs_for(
            Some(netget::state::AccessLogOwner::Client(id.as_u32())),
            None,
        )
        .await
    {
        let text = format!("{:?}", e);
        assert!(!text.contains(PASSWORD));
        assert!(!text.contains(TOKEN));
    }
    state.remove_client(id).await;
    Ok(())
}
