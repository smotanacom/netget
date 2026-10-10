//! NetGet's portmapper against **rpcinfo** (libtirpc), the stock RPC client every Linux NFS
//! setup is debugged with. rpcinfo only ever asks port 111, so it runs in a network namespace
//! whose port 111 is DNATed to NetGet's port on the host end of a veth; it then exercises
//! PMAP v2 DUMP (`-p`), RPCBIND v3 DUMP (`-s`), v4 GETADDRLIST (`-l`), and the v4
//! GETADDR-then-NULL path of `-T` and `-u` over TCP and UDP.
//! Needs root or passwordless sudo, iproute2 and iptables, and fails rather than skips
//! without them. No LLM calls: a python policy is the model. Then registrations, the RPC
//! error replies and the bounds, over hand-assembled XDR, and a failed handler.
#[cfg(target_os = "linux")]
use crate::helpers::netns::Netns;
use netget::cli::management::ServerForm;
use netget::state::app_state::AppState;
use netget::state::{AccessLogOwner, ServerId};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

const POLICY: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
if t=='sunrpc_register':
  a=[{'type':'sunrpc_accept' if e['program']>=0x20000000 else 'sunrpc_reject'}]
else:
  m=[{'program':100000,'version':v,'protocol':p,'port':111} for v in (2,3,4) for p in ('tcp','udp')]
  m+=[{'program':100003,'version':3,'protocol':'tcp','port':2049},{'program':100003,'version':4,'protocol':'tcp','port':2049},
      {'program':100005,'version':3,'protocol':'udp','port':20048},{'program':100005,'version':3,'protocol':'tcp','port':20048}]
  a=[{'type':'sunrpc_mappings','mappings':m}]
print(json.dumps({'actions':a}))"#;

async fn start(host: &str, handlers: Option<Vec<Value>>) -> (AppState, ServerId, u16) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "sunrpc".into(),
        port: Some(0),
        host: Some(host.into()),
        instruction: Some("Be a portmapper".into()),
        event_handlers: handlers,
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .unwrap();
    let port = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(a) = state.get_server(id).await.and_then(|s| s.local_addr) {
                break a.port();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (state, id, port)
}

fn policy() -> Option<Vec<Value>> {
    Some(vec![
        json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":POLICY}}),
    ])
}

async fn events(state: &AppState, id: ServerId, event_type: &str) -> Vec<Value> {
    state
        .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
        .await
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .filter(|e| e["event_type"] == event_type)
        .map(|e| e["request"].clone())
        .collect()
}

#[cfg(target_os = "linux")]
async fn rpcinfo(ns: &Netns, args: &[&str]) -> (bool, String) {
    let mut all = vec!["rpcinfo"];
    all.extend_from_slice(args);
    let out = ns.output(&all).await;
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

#[cfg(target_os = "linux")]
/// Rows of rpcinfo's table output, whitespace-split, header dropped.
fn rows(text: &str) -> Vec<Vec<String>> {
    text.lines()
        .skip(1)
        .map(|l| l.split_whitespace().map(str::to_string).collect())
        .filter(|r: &Vec<String>| !r.is_empty())
        .collect()
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn rpcinfo_reads_netget() {
    let ns = Netns::create("rp", 71);
    let host = ns.host_ip.to_string();
    let (state, id, port) = start(&host, policy()).await;
    for proto in ["tcp", "udp"] {
        ns.exec(&[
            "iptables",
            "-t",
            "nat",
            "-A",
            "OUTPUT",
            "-p",
            proto,
            "-d",
            &host,
            "--dport",
            "111",
            "-j",
            "DNAT",
            "--to-destination",
            &format!("{host}:{port}"),
        ]);
    }

    // PMAP v2 DUMP over TCP.
    let (ok, text) = rpcinfo(&ns, &["-p", &host]).await;
    assert!(ok, "{text}");
    let r = rows(&text);
    assert!(
        r.contains(
            &["100003", "3", "tcp", "2049", "nfs"]
                .map(String::from)
                .to_vec()
        ),
        "{text}"
    );
    assert!(
        r.contains(
            &["100005", "3", "udp", "20048", "mountd"]
                .map(String::from)
                .to_vec()
        ),
        "{text}"
    );
    assert_eq!(r.len(), 10, "{text}");

    // RPCBIND v4 DUMP, summarised by rpcinfo.
    let (ok, text) = rpcinfo(&ns, &["-s", &host]).await;
    assert!(ok, "{text}");
    let nfs = rows(&text)
        .into_iter()
        .find(|r| r[0] == "100003")
        .unwrap_or_else(|| panic!("{text}"));
    assert_eq!(nfs[1..4], ["4,3", "tcp", "nfs"].map(String::from), "{text}");
    assert_eq!(nfs[4], "netget", "{text}");

    // RPCBIND v4 GETADDRLIST: the universal address NetGet built.
    let (ok, text) = rpcinfo(&ns, &["-l", &host, "100003", "3"]).await;
    assert!(ok, "{text}");
    assert!(
        text.contains(&format!("{host}.8.1")) && text.contains("tcp"),
        "2049 is 8.1 in a universal address: {text}"
    );

    // GETADDR for the portmapper itself, then NULL on the address it gave (DNATed back).
    let (ok, text) = rpcinfo(&ns, &["-T", "tcp", &host, "100000", "4"]).await;
    assert!(
        ok && text.contains("program 100000 version 4 ready and waiting"),
        "{text}"
    );
    // PMAP v2 GETPORT over UDP, then NULL over UDP.
    let (ok, text) = rpcinfo(&ns, &["-u", &host, "100000", "2"]).await;
    assert!(
        ok && text.contains("program 100000 version 2 ready and waiting"),
        "{text}"
    );
    // A program the model has not registered.
    let (ok, text) = rpcinfo(&ns, &["-T", "tcp", &host, "100024", "1"]).await;
    assert!(!ok && text.contains("not registered"), "{text}");

    // What the model was shown.
    let queries = events(&state, id, "sunrpc_query").await;
    let procedures: std::collections::BTreeSet<&str> = queries
        .iter()
        .map(|q| q["procedure"].as_str().unwrap())
        .collect();
    assert_eq!(
        procedures.into_iter().collect::<Vec<_>>(),
        ["dump", "getaddr", "getaddrlist"],
        "libtirpc resolves through RPCBIND v4 GETADDR even for -u: {queries:?}"
    );
    let dumps: std::collections::BTreeSet<u64> = queries
        .iter()
        .filter(|q| q["procedure"] == "dump")
        .map(|q| q["rpc_version"].as_u64().unwrap())
        .collect();
    assert_eq!(
        dumps.into_iter().collect::<Vec<_>>(),
        [2, 3],
        "-p is PMAP v2, -s RPCBIND v3"
    );
    let status = queries
        .iter()
        .find(|q| q["program"] == 100024)
        .unwrap_or_else(|| panic!("{queries:?}"));
    assert_eq!(status["program_name"], "status");
    assert_eq!(status["protocol"], "tcp");
}

/// Registrations with XDR assembled by python's struct module, not NetGet's encoder.
const REGISTER: &str = r#"import socket,struct,sys,json
host,port=sys.argv[1],int(sys.argv[2])
def xs(t):
  b=t.encode(); return struct.pack('>I',len(b))+b+b'\0'*((4-len(b)%4)%4)
def call(vers,proc,args,cred=(0,b'')):
  body=struct.pack('>IIIIII',9,0,2,100000,vers,proc)+struct.pack('>I',cred[0])+xs_raw(cred[1])+struct.pack('>II',0,0)+args
  s=socket.create_connection((host,port)); s.sendall(struct.pack('>I',0x80000000|len(body))+body)
  n=struct.unpack('>I',s.recv(4))[0]&0x7fffffff; d=b''
  while len(d)<n: d+=s.recv(n-len(d))
  return struct.unpack('>I',d[-4:])[0]
def xs_raw(b): return struct.pack('>I',len(b))+b+b'\0'*((4-len(b)%4)%4)
sys_cred=(1,struct.pack('>I',0)+xs('client')+struct.pack('>III',1000,1000,0))
print(json.dumps({
 'v2_set_user_range':call(2,1,struct.pack('>IIII',0x20000001,1,6,4242),sys_cred),
 'v2_set_nfs':call(2,1,struct.pack('>IIII',100003,3,6,4242)),
 'v4_set':call(4,1,struct.pack('>II',0x20000002,1)+xs('udp')+xs('127.0.0.1.16.147')+xs('alice')),
 'v4_unset':call(4,2,struct.pack('>II',0x20000002,1)+xs('')+xs('')+xs('alice')),
 'v2_getport_nfs':call(2,3,struct.pack('>IIII',100003,3,6,0)),
 'v2_getport_nfs_udp':call(2,3,struct.pack('>IIII',100003,3,17,0)),
 'v2_getport_mountd_v1':call(2,3,struct.pack('>IIII',100005,1,17,0)),
}))"#;

#[tokio::test]
async fn registrations_errors_and_bounds() {
    let (state, id, port) = start("127.0.0.1", policy()).await;
    let out = tokio::process::Command::new("python3")
        .args(["-c", REGISTER, "127.0.0.1", &port.to_string()])
        .output()
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let got: Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(
        got,
        json!({"v2_set_user_range": 1, "v2_set_nfs": 0, "v4_set": 1, "v4_unset": 1,
               "v2_getport_nfs": 2049, "v2_getport_nfs_udp": 0, "v2_getport_mountd_v1": 20048}),
        "the policy accepts only the user range (0x20000000+); GETPORT finds NFS on tcp only, and \
         falls back to another version of a registered program as rpcbind does"
    );
    let regs = events(&state, id, "sunrpc_register").await;
    let first = regs.iter().find(|r| r["program"] == 0x2000_0001).unwrap();
    assert_eq!(first["port"], 4242);
    assert_eq!(first["protocol"], "tcp");
    assert_eq!(
        first["credentials"],
        json!({"flavor": "sys", "stamp": 0, "machine": "client", "uid": 1000, "gid": 1000, "gids": []})
    );
    let v4 = regs
        .iter()
        .find(|r| r["program"] == 0x2000_0002 && r["operation"] == "set")
        .unwrap();
    assert_eq!(
        (v4["port"].as_u64(), v4["owner"].as_str()),
        (Some(4243), Some("alice"))
    );
    let unset = regs.iter().find(|r| r["operation"] == "unset").unwrap();
    assert_eq!(
        unset["protocol"], "",
        "an empty netid removes every transport"
    );

    // RPC-level errors, answered in Rust.
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let mut ask = async |rpcvers: u32, prog: u32, vers: u32, proc_: u32, args: &[u8]| -> Vec<u32> {
        let mut body: Vec<u8> = [7u32, 0, rpcvers, prog, vers, proc_, 0, 0, 0, 0]
            .iter()
            .flat_map(|v| v.to_be_bytes())
            .collect();
        body.extend_from_slice(args);
        let mut msg = (0x8000_0000u32 | body.len() as u32).to_be_bytes().to_vec();
        msg.extend(body);
        s.write_all(&msg).await.unwrap();
        let n = (s.read_u32().await.unwrap() & 0x7fff_ffff) as usize;
        let mut b = vec![0u8; n];
        s.read_exact(&mut b).await.unwrap();
        b.chunks(4)
            .map(|c| u32::from_be_bytes(c.try_into().unwrap()))
            .collect()
    };
    // [xid, REPLY, MSG_DENIED, RPC_MISMATCH, low, high]
    assert_eq!(ask(3, 100_000, 2, 0, &[]).await, [7, 1, 1, 0, 2, 2]);
    // [xid, REPLY, MSG_ACCEPTED, verf flavor, verf len, accept_stat, …]
    assert_eq!(
        ask(2, 100_003, 3, 0, &[]).await,
        [7, 1, 0, 0, 0, 1],
        "PROG_UNAVAIL"
    );
    assert_eq!(
        ask(2, 100_000, 5, 0, &[]).await,
        [7, 1, 0, 0, 0, 2, 2, 4],
        "PROG_MISMATCH 2-4"
    );
    assert_eq!(
        ask(2, 100_000, 2, 5, &[]).await,
        [7, 1, 0, 0, 0, 3],
        "CALLIT is PROC_UNAVAIL"
    );
    assert_eq!(
        ask(2, 100_000, 2, 3, &[0, 0, 0, 1]).await,
        [7, 1, 0, 0, 0, 4],
        "GARBAGE_ARGS"
    );
    assert_eq!(ask(2, 100_000, 2, 0, &[]).await, [7, 1, 0, 0, 0, 0], "NULL");
    let time = ask(2, 100_000, 4, 6, &[]).await;
    assert_eq!(time[..6], [7, 1, 0, 0, 0, 0]);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as u32;
    assert!(time[6].abs_diff(now) < 5, "GETTIME is the server's clock");

    // A record announcing more than the bound closes the connection unread.
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let too_big = 0x8000_0000u32 | (netget::server::sunrpc::wire::MAX_RECORD as u32 + 1);
    s.write_all(&too_big.to_be_bytes()).await.unwrap();
    let mut rest = Vec::new();
    let n = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut rest))
        .await
        .unwrap()
        .unwrap_or(0);
    assert_eq!(n, 0, "{rest:?}");
}

#[tokio::test]
async fn a_failed_handler_answers_system_err() {
    let (_state, _id, port) = start("127.0.0.1", None).await;
    let s = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    // PMAP v2 DUMP over UDP.
    let msg: Vec<u8> = [11u32, 0, 2, 100_000, 2, 4, 0, 0, 0, 0]
        .iter()
        .flat_map(|v| v.to_be_bytes())
        .collect();
    s.send_to(&msg, ("127.0.0.1", port)).await.unwrap();
    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(20), s.recv(&mut buf))
        .await
        .expect("an answer, not silence")
        .unwrap();
    let words: Vec<u32> = buf[..n]
        .chunks(4)
        .map(|c| u32::from_be_bytes(c.try_into().unwrap()))
        .collect();
    assert_eq!(words, [11, 1, 0, 0, 0, 5], "SYSTEM_ERR");
}

fn call(xid: u32, vers: u32, proc_: u32, args: &[u32]) -> Vec<u8> {
    [xid, 0, 2, 100_000, vers, proc_, 0, 0, 0, 0]
        .iter()
        .chain(args)
        .flat_map(|v| v.to_be_bytes())
        .collect()
}

#[tokio::test]
async fn replies_through_the_pcap_oracle() {
    let (_state, _id, port) = start("127.0.0.1", policy()).await;
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    // PMAP v2 DUMP, PMAP v2 GETPORT for NFS v3 tcp, and RPCBIND v4 DUMP and GETADDRLIST,
    // each in one record-marked TCP record.
    let mut getaddrlist = call(24, 4, 11, &[100_003, 3, 3]);
    getaddrlist.extend(b"tcp\0");
    getaddrlist.extend([0u8; 8]); // empty addr and owner strings
    let calls = [
        call(21, 2, 4, &[]),
        call(22, 2, 3, &[100_003, 3, 6, 0]),
        call(23, 4, 4, &[]),
        getaddrlist,
    ];
    let mut oracle = crate::helpers::pcap_oracle::PcapOracle::tcp("rpc")
        .port(netget::server::sunrpc::wire::PORT)
        .also_named("portmap");
    for c in &calls {
        let mut rec = (0x8000_0000u32 | c.len() as u32).to_be_bytes().to_vec();
        rec.extend_from_slice(c);
        s.write_all(&rec).await.unwrap();
        let h = s.read_u32().await.unwrap();
        let mut body = vec![0u8; (h & 0x7fff_ffff) as usize];
        s.read_exact(&mut body).await.unwrap();
        let mut reply = h.to_be_bytes().to_vec();
        reply.extend(body);
        oracle = oracle.to_server(&rec).from_server(&reply);
    }
    oracle.assert_clean();
}
