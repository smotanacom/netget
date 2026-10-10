//! ClickHouse native server over raw packets: hello and login, ping, a typed result set with
//! and without compression, DDL, the INSERT exchange, exceptions, and the bounds and
//! fail-closed paths.
use netget::cli::management::ServerForm;
use netget::server::clickhouse::wire::{self, client_packet, server_packet, Block, Source};
use netget::state::{app_state::AppState, AccessLogOwner, ServerId};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    sync::mpsc,
};

/// Answers SELECTs on `events` with every supported type, accepts INSERT INTO events and
/// DDL, and raises UNKNOWN_TABLE for anything else.
pub const SQL_SCRIPT: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
if t=='clickhouse_insert_data':
  a={'type':'clickhouse_ok'} if len(e['rows'])<=10 else {'type':'clickhouse_exception','code':241,'message':'too many rows'}
else:
  q=' '.join(e['query'].split()).rstrip(';'); ql=q.lower()
  if ql.startswith('select') and 'from events' in ql:
    a={'type':'clickhouse_result','columns':[{'name':'id','type':'UInt32'},{'name':'name','type':'String'},{'name':'score','type':'Nullable(Float64)'},{'name':'day','type':'Date'},{'name':'at','type':'DateTime'},{'name':'ok','type':'Bool'},{'name':'delta','type':'Int64'}],'rows':[[1,'alpha',1.5,'2024-01-02','2024-01-02 03:04:05',True,-7],[2,'beta',None,'2024-12-31','2024-12-31 23:59:59',False,9007199254740993]]}
  elif ql.startswith('insert into events'):
    a={'type':'clickhouse_insert','columns':[{'name':'id','type':'UInt32'},{'name':'name','type':'String'}]}
  elif ql.startswith('create') or ql.startswith('drop'):
    a={'type':'clickhouse_ok'}
  else:
    a={'type':'clickhouse_exception','code':60,'message':'Table default.missing does not exist'}
print(json.dumps({'actions':[a]}))"#;

pub fn handlers() -> Vec<Value> {
    vec![
        json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":SQL_SCRIPT}}),
    ]
}

pub async fn start(handlers: Vec<Value>, params: Value) -> (AppState, ServerId, SocketAddr) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "clickhouse".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Answer SQL".into()),
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
    state
        .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
        .await
        .iter()
        .map(|e| serde_json::to_string(e).unwrap())
        .any(|e| e.contains(needle))
}

struct Raw {
    r: BufReader<tokio::io::ReadHalf<TcpStream>>,
    w: tokio::io::WriteHalf<TcpStream>,
}

type Outcome = Result<Vec<Block>, (i32, String)>;

impl Raw {
    async fn hello(
        addr: SocketAddr,
        user: &str,
        password: &str,
    ) -> (Self, Result<(), (i32, String)>) {
        let (r, w) = tokio::io::split(TcpStream::connect(addr).await.unwrap());
        let mut c = Raw {
            r: BufReader::new(r),
            w,
        };
        let mut out = Vec::new();
        wire::put_varuint(&mut out, client_packet::HELLO);
        wire::put_string(&mut out, b"raw test");
        wire::put_varuint(&mut out, 24);
        wire::put_varuint(&mut out, 8);
        wire::put_varuint(&mut out, 54469);
        wire::put_string(&mut out, b"");
        wire::put_string(&mut out, user.as_bytes());
        wire::put_string(&mut out, password.as_bytes());
        c.w.write_all(&out).await.unwrap();
        let kind = c.kind().await;
        let mut s = Source::plain(&mut c.r);
        if kind == server_packet::EXCEPTION {
            let e = wire::read_exception(&mut s).await.unwrap();
            return (c, Err(e));
        }
        assert_eq!(kind, server_packet::HELLO);
        assert_eq!(s.string().await.unwrap(), "ClickHouse");
        s.varuint().await.unwrap();
        s.varuint().await.unwrap();
        assert_eq!(
            s.varuint().await.unwrap(),
            wire::REVISION,
            "the server announces its own revision"
        );
        assert_eq!(s.string().await.unwrap(), "UTC");
        s.string().await.unwrap();
        s.varuint().await.unwrap();
        (c, Ok(()))
    }

    async fn kind(&mut self) -> u64 {
        wire::read_packet_type(&mut self.r, Duration::from_secs(20))
            .await
            .unwrap()
            .expect("a packet")
    }

    async fn send_query(&mut self, query: &str, compressed: bool) {
        let mut out = wire::query_packet(query, compressed);
        out.extend(wire::data_packet(client_packet::DATA, &Block::default(), compressed).unwrap());
        self.w.write_all(&out).await.unwrap();
    }

    /// Read to end of stream (or the header block, for an INSERT).
    async fn read(&mut self, compressed: bool, header_only: bool) -> Outcome {
        let mut blocks = Vec::new();
        loop {
            let kind = self.kind().await;
            match kind {
                server_packet::DATA => {
                    Source::plain(&mut self.r).string().await.unwrap();
                    let mut s = Source::new(&mut self.r, compressed);
                    let block = Block::decode(&mut s).await.unwrap();
                    assert!(s.drained());
                    blocks.push(block);
                    if header_only {
                        return Ok(blocks);
                    }
                }
                server_packet::PROGRESS => {
                    let mut s = Source::plain(&mut self.r);
                    for _ in 0..5 {
                        s.varuint().await.unwrap();
                    }
                }
                server_packet::EXCEPTION => {
                    return Err(wire::read_exception(&mut Source::plain(&mut self.r))
                        .await
                        .unwrap())
                }
                server_packet::END_OF_STREAM => return Ok(blocks),
                other => panic!("unexpected packet {other}"),
            }
        }
    }

    async fn closed(&mut self) {
        let mut buf = [0u8; 256];
        loop {
            match tokio::time::timeout(Duration::from_secs(5), self.r.read(&mut buf)).await {
                Ok(Ok(0)) | Ok(Err(_)) => return,
                Ok(Ok(_)) => continue,
                Err(_) => panic!("the connection stayed open"),
            }
        }
    }
}

#[tokio::test]
async fn hello_select_ddl_insert_and_exceptions() {
    let (state, id, addr) =
        start(handlers(), json!({"user": "analyst", "password": "s3cret"})).await;
    let (mut c, refused) = Raw::hello(addr, "analyst", "wrong").await;
    assert_eq!(refused.unwrap_err().0, 516, "AUTHENTICATION_FAILED");
    c.closed().await;
    let (mut c, ok) = Raw::hello(addr, "analyst", "s3cret").await;
    ok.unwrap();
    let mut ping = Vec::new();
    wire::put_varuint(&mut ping, client_packet::PING);
    c.w.write_all(&ping).await.unwrap();
    assert_eq!(c.kind().await, server_packet::PONG);
    for compressed in [false, true] {
        c.send_query("SELECT * FROM events", compressed).await;
        let blocks = c.read(compressed, false).await.unwrap();
        assert_eq!(
            (blocks.len(), blocks[0].rows.len()),
            (2, 0),
            "a header block, then the rows"
        );
        assert_eq!(
            blocks[1].columns_json(),
            json!([{"name":"id","type":"UInt32"},{"name":"name","type":"String"},{"name":"score","type":"Nullable(Float64)"},{"name":"day","type":"Date"},{"name":"at","type":"DateTime"},{"name":"ok","type":"Bool"},{"name":"delta","type":"Int64"}])
        );
        assert_eq!(
            blocks[1].rows,
            vec![
                vec![
                    json!(1),
                    json!("alpha"),
                    json!(1.5),
                    json!("2024-01-02"),
                    json!("2024-01-02 03:04:05"),
                    json!(true),
                    json!(-7)
                ],
                vec![
                    json!(2),
                    json!("beta"),
                    Value::Null,
                    json!("2024-12-31"),
                    json!("2024-12-31 23:59:59"),
                    json!(false),
                    json!(9007199254740993i64)
                ],
            ],
            "compressed={compressed}"
        );
    }
    c.send_query("CREATE TABLE t (x UInt8) ENGINE = Memory", false)
        .await;
    assert!(c.read(false, false).await.unwrap().is_empty());
    c.send_query("SELECT * FROM missing", false).await;
    assert_eq!(
        c.read(false, false).await.unwrap_err(),
        (
            60,
            "DB::Exception: Table default.missing does not exist".into()
        )
    );
    // INSERT: the header, then the rows (compressed this time), then the handler's verdict.
    c.send_query("INSERT INTO events (id, name) VALUES", true)
        .await;
    let header = c.read(true, true).await.unwrap().remove(0);
    assert_eq!(
        header.columns_json(),
        json!([{"name":"id","type":"UInt32"},{"name":"name","type":"String"}])
    );
    let rows =
        Block::from_json(&header.columns_json(), &json!([[7, "seven"], [8, "eight"]])).unwrap();
    let mut out = wire::data_packet(client_packet::DATA, &rows, true).unwrap();
    out.extend(wire::data_packet(client_packet::DATA, &Block::default(), true).unwrap());
    c.w.write_all(&out).await.unwrap();
    assert!(c.read(true, false).await.unwrap().is_empty());
    assert!(
        handler_saw(&state, id, r#""rows":[[7,"seven"],[8,"eight"]]"#).await,
        "the inserted rows reached the handler"
    );
    // Eleven rows are refused by the handler with its exception.
    c.send_query("INSERT INTO events (id, name) VALUES", false)
        .await;
    c.read(false, true).await.unwrap();
    let many: Vec<Value> = (0..11).map(|i| json!([i, "x"])).collect();
    let rows = Block::from_json(&header.columns_json(), &Value::Array(many)).unwrap();
    let mut out = wire::data_packet(client_packet::DATA, &rows, false).unwrap();
    out.extend(wire::data_packet(client_packet::DATA, &Block::default(), false).unwrap());
    c.w.write_all(&out).await.unwrap();
    assert_eq!(c.read(false, false).await.unwrap_err().0, 241);
    state.remove_server(id).await;
}

#[tokio::test]
async fn bounds_and_fail_closed() {
    let (state, id, addr) = start(handlers(), json!({"idle_timeout_secs": 1})).await;
    // A string announced past 1 MiB in the hello is refused before it is read.
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut out = Vec::new();
    wire::put_varuint(&mut out, client_packet::HELLO);
    wire::put_varuint(&mut out, (wire::MAX_STRING + 1) as u64);
    s.write_all(&out).await.unwrap();
    let mut buf = [0u8; 64];
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf)).await,
        Ok(Ok(0)) | Ok(Err(_))
    ));
    // An INSERT block announcing more than 100000 rows ends the connection.
    let (mut c, ok) = Raw::hello(addr, "default", "").await;
    ok.unwrap();
    c.send_query("INSERT INTO events (id, name) VALUES", false)
        .await;
    c.read(false, true).await.unwrap();
    let mut out = Vec::new();
    wire::put_varuint(&mut out, client_packet::DATA);
    wire::put_string(&mut out, b"");
    wire::put_varuint(&mut out, 0);
    wire::put_varuint(&mut out, 2);
    wire::put_varuint(&mut out, (wire::MAX_ROWS + 1) as u64);
    c.w.write_all(&out).await.unwrap();
    c.closed().await;
    // A compressed frame with a bad checksum ends it too.
    let (mut c, _) = Raw::hello(addr, "default", "").await;
    let mut out = wire::query_packet("SELECT 1", true);
    wire::put_varuint(&mut out, client_packet::DATA);
    wire::put_string(&mut out, b"");
    let mut frame = wire::compress(&Block::default().encode_body().unwrap());
    frame[0] ^= 0xff;
    out.extend(frame);
    c.w.write_all(&out).await.unwrap();
    c.closed().await;
    // A silent connection is closed after idle_timeout_secs.
    let (mut c, _) = Raw::hello(addr, "default", "").await;
    c.closed().await;
    state.remove_server(id).await;
    // No handler: an exception with a generic message, never a result.
    let (state, id, addr) = start(vec![], json!({})).await;
    let (mut c, _) = Raw::hello(addr, "default", "").await;
    c.send_query("SELECT * FROM events", false).await;
    assert_eq!(
        c.read(false, false).await.unwrap_err(),
        (
            1002,
            "DB::Exception: netget: request could not be processed".into()
        )
    );
    state.remove_server(id).await;
}
