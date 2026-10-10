//! MessagePack-RPC server over raw messages: requests answered with results and errors,
//! pipelined requests answered in order, notifications (and a notification sent back), the
//! JSON mapping of binary and extensions, and the bounds and fail-closed paths.
use netget::cli::management::ServerForm;
use netget::server::msgpack_rpc::wire;
use netget::state::{app_state::AppState, AccessLogOwner, ServerId};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
};

/// add sums its params, echo returns them, notify_me answers and notifies, anything else is an
/// error; notifications are ignored.
pub const RPC_SCRIPT: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
if t=='msgpack_notification':
  a=[{'type':'msgpack_ignore'}]
elif e['method']=='add':
  a=[{'type':'msgpack_result','result':sum(e['params'])}]
elif e['method']=='echo':
  a=[{'type':'msgpack_result','result':e['params']}]
elif e['method']=='notify_me':
  a=[{'type':'msgpack_result','result':'ok'},{'type':'msgpack_notify','method':'progress','params':[100]}]
else:
  a=[{'type':'msgpack_error','error':'no such method: '+e['method']}]
print(json.dumps({'actions':a}))"#;

pub fn handlers() -> Vec<Value> {
    vec![
        json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":RPC_SCRIPT}}),
    ]
}

pub async fn start(handlers: Vec<Value>, params: Value) -> (AppState, ServerId, SocketAddr) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "msgpack-rpc".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Serve RPC".into()),
        startup_params: Some(params),
        event_handlers: Some(handlers),
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .unwrap();
    let addr = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(a) = state.get_server(id).await.and_then(|s| s.local_addr) {
                break a;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (state, id, SocketAddr::from(([127, 0, 0, 1], addr.port())))
}

pub async fn handler_saw(state: &AppState, id: ServerId, needle: &str) -> bool {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if state
                .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
                .await
                .iter()
                .any(|e| serde_json::to_string(e).unwrap().contains(needle))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

async fn next(stream: &mut wire::Stream<tokio::io::ReadHalf<TcpStream>>) -> wire::Rpc {
    wire::parse(
        stream
            .next(Duration::from_secs(20))
            .await
            .unwrap()
            .expect("a message"),
    )
    .unwrap()
}

#[tokio::test]
async fn requests_notifications_and_mapping() -> anyhow::Result<()> {
    let (state, id, addr) = start(handlers(), json!({})).await;
    let (r, mut w) = tokio::io::split(TcpStream::connect(addr).await.unwrap());
    let mut s = wire::Stream::new(r);
    // Three requests pipelined in one write are answered in order.
    let mut out = wire::request(1, "add", &json!([2, 3, -10]))?;
    out.extend(wire::request(
        2,
        "echo",
        &json!([{"$bin": "00ff"}, {"$ext": 5, "hex": "abcd"}, 1.5, null, {"k": [true]}]),
    )?);
    out.extend(wire::request(3, "missing", &json!([]))?);
    w.write_all(&out).await.unwrap();
    assert_eq!(
        next(&mut s).await,
        wire::Rpc::Response {
            msgid: 1,
            error: Value::Null,
            result: json!(-5)
        }
    );
    assert_eq!(
        next(&mut s).await,
        wire::Rpc::Response {
            msgid: 2,
            error: Value::Null,
            result: json!([{"$bin": "00ff"}, {"$ext": 5, "hex": "abcd"}, 1.5, null, {"k": [true]}])
        },
        "binary and extensions survive the JSON round trip"
    );
    assert_eq!(
        next(&mut s).await,
        wire::Rpc::Response {
            msgid: 3,
            error: json!("no such method: missing"),
            result: Value::Null
        }
    );
    // A notification gets no reply; a request may carry a notification back.
    w.write_all(&wire::notification("log", &json!(["hello"]))?)
        .await
        .unwrap();
    assert!(handler_saw(&state, id, r#""params":["hello"]"#).await);
    w.write_all(&wire::request(4, "notify_me", &json!([]))?)
        .await
        .unwrap();
    assert_eq!(
        next(&mut s).await,
        wire::Rpc::Response {
            msgid: 4,
            error: Value::Null,
            result: json!("ok")
        }
    );
    assert_eq!(
        next(&mut s).await,
        wire::Rpc::Notification {
            method: "progress".into(),
            params: json!([100])
        }
    );
    // A message split across writes is reassembled.
    let bytes = wire::request(5, "add", &json!([40, 2]))?;
    w.write_all(&bytes[..3]).await.unwrap();
    w.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    w.write_all(&bytes[3..]).await.unwrap();
    assert_eq!(
        next(&mut s).await,
        wire::Rpc::Response {
            msgid: 5,
            error: Value::Null,
            result: json!(42)
        }
    );
    state.remove_server(id).await;
    Ok(())
}

async fn closed(s: &mut TcpStream) {
    let mut buf = [0u8; 64];
    loop {
        match tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => return,
            Ok(Ok(_)) => continue,
            Err(_) => panic!("the connection stayed open"),
        }
    }
}

/// Closed without a single byte of reply.
async fn closed_silently(s: &mut TcpStream) {
    let mut buf = [0u8; 64];
    match tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf)).await {
        Ok(Ok(0)) | Ok(Err(_)) => {}
        Ok(Ok(n)) => panic!("expected no reply, read {n} bytes"),
        Err(_) => panic!("the connection stayed open"),
    }
}

#[tokio::test]
async fn bounds_and_fail_closed() {
    let (state, id, addr) = start(handlers(), json!({"idle_timeout_secs": 1})).await;
    // A str32 announcing 4 GiB waits for bytes it never allocates; past 1 MiB buffered, closed.
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut out = vec![0x94, 0x00, 0x01, 0xa3];
    out.extend(b"add");
    out.push(0xdb);
    out.extend(u32::MAX.to_be_bytes());
    out.extend(vec![b'x'; wire::MAX_MESSAGE]);
    let _ = s.write_all(&out).await;
    closed(&mut s).await;
    // A valid echo request whose params nest past 32 levels: refused, never echoed.
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut out = vec![0x94, 0x00, 0x01, 0xa4];
    out.extend(b"echo");
    out.extend(std::iter::repeat_n(0x91u8, 64));
    out.push(0xc0);
    s.write_all(&out).await.unwrap();
    closed_silently(&mut s).await;
    // Not an RPC envelope, and the never-used byte 0xc1.
    for bad in [vec![0x93, 0x07, 0x01, 0x02], vec![0xc1]] {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(&bad).await.unwrap();
        closed(&mut s).await;
    }
    // A silent connection is closed after idle_timeout_secs.
    let mut s = TcpStream::connect(addr).await.unwrap();
    closed(&mut s).await;
    state.remove_server(id).await;
    // No handler: an error naming a generic failure, never a result.
    let (state, id, addr) = start(vec![], json!({})).await;
    let (r, mut w) = tokio::io::split(TcpStream::connect(addr).await.unwrap());
    let mut s = wire::Stream::new(r);
    w.write_all(&wire::request(9, "add", &json!([1])).unwrap())
        .await
        .unwrap();
    assert_eq!(
        next(&mut s).await,
        wire::Rpc::Response {
            msgid: 9,
            error: json!("netget: request could not be processed"),
            result: Value::Null
        }
    );
    state.remove_server(id).await;
}
