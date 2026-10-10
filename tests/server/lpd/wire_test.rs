//! LPD server over raw sockets: receive-job in both file orders with the handler's decision
//! as the final acknowledgement, abort, queue listings, removal, the declared bounds and the
//! fail-closed paths. Handlers are scripts and static rules; no model is consulted.
use netget::cli::management::ServerForm;
use netget::state::{app_state::AppState, ServerId};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
};

/// Accepts a job only when every parsed field is what the test sent.
const JOB_SCRIPT: &str = "import json,sys\ne=json.load(sys.stdin)['event']\nf=e['files']\nok=(e['queue']=='raw' and e['job_id']=='001' and e['job_name']=='report' and e['user']=='alice' and e['host']=='ws1' and e['title']=='Q3' and len(f)==1 and f[0]['format']=='f' and f[0]['source_name']=='report.txt' and f[0]['text']=='hello printer\\n' and f[0]['size']==14) or (e['job_name']=='binary' and f[0]['text'] is None and f[0]['size']==4)\nprint(json.dumps({'actions':[{'type':'lpd_job_reply','accept':ok}]}))";

pub fn handlers(job_script: &str) -> Vec<Value> {
    vec![
        json!({"event_pattern":"lpd_print_job","handler":{"type":"script","language":"python","code":job_script}}),
        json!({"event_pattern":"lpd_queue_query","handler":{"type":"static","actions":[{"type":"lpd_queue_status","status":"raw is ready and printing","jobs":[
            {"owner":"alice","job_id":42,"files":"report.txt","size":1024},
            {"owner":"bob","job_id":43,"files":"notes.txt","size":10}
        ]}]}}),
        json!({"event_pattern":"lpd_remove_request","handler":{"type":"static","actions":[{"type":"lpd_remove_result","removed":[42]}]}}),
    ]
}

pub async fn start(handlers: Vec<Value>, params: Value) -> (AppState, ServerId, SocketAddr) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "lpd".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Run a test print server".into()),
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

async fn ack(s: &mut TcpStream) -> u8 {
    let mut b = [0u8; 1];
    let n = tokio::time::timeout(Duration::from_secs(10), s.read(&mut b))
        .await
        .expect("LPD acknowledgement deadline")
        .unwrap();
    assert_eq!(n, 1, "connection closed instead of acknowledging");
    b[0]
}

async fn send_file(s: &mut TcpStream, code: u8, name: &str, body: &[u8]) -> (u8, u8) {
    s.write_all(format!("{}{} {name}\n", code as char, body.len()).as_bytes())
        .await
        .unwrap();
    let first = ack(s).await;
    if first != 0 {
        return (first, 0xff);
    }
    s.write_all(body).await.unwrap();
    s.write_all(&[0]).await.unwrap();
    (first, ack(s).await)
}

async fn closed(s: &mut TcpStream) -> Vec<u8> {
    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut rest))
        .await
        .expect("server closes")
        .unwrap();
    rest
}

const CONTROL: &str =
    "Hws1\nPalice\nJreport\nTQ3\nCA\nLalice\nNreport.txt\nfdfA001ws1\nUdfA001ws1\n";

#[tokio::test]
async fn jobs_in_both_orders_carry_the_handlers_decision() {
    let (state, id, addr) = start(handlers(JOB_SCRIPT), json!({"queues":["raw"]})).await;
    // Control file first (LPRng's order): the final acknowledgement is the handler's.
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"\x02raw\n").await.unwrap();
    assert_eq!(ack(&mut s).await, 0);
    assert_eq!(
        send_file(&mut s, 2, "cfA001ws1", CONTROL.as_bytes()).await,
        (0, 0)
    );
    assert_eq!(
        send_file(&mut s, 3, "dfA001ws1", b"hello printer\n").await,
        (0, 0)
    );
    // A second job on the same connection that the handler refuses: nonzero final ack.
    let refused = CONTROL.replace("A001", "A002");
    assert_eq!(
        send_file(&mut s, 2, "cfA002ws1", refused.as_bytes()).await,
        (0, 0)
    );
    assert_eq!(
        send_file(&mut s, 3, "dfA002ws1", b"other text\n").await,
        (0, 1)
    );
    drop(s);
    // Data first (BSD's order) with binary content the handler sees as text: null.
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"\x02raw\n").await.unwrap();
    assert_eq!(ack(&mut s).await, 0);
    assert_eq!(
        send_file(&mut s, 3, "dfA007ws1", &[0x1b, 0x00, 0xff, 0x01]).await,
        (0, 0)
    );
    let control = "Hws1\nPalice\nJbinary\nldfA007ws1\n";
    assert_eq!(
        send_file(&mut s, 2, "cfA007ws1", control.as_bytes()).await,
        (0, 0)
    );
    drop(s);
    // Abort discards what was sent; the job after it is decided on its own.
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"\x02raw\n").await.unwrap();
    assert_eq!(ack(&mut s).await, 0);
    assert_eq!(
        send_file(&mut s, 3, "dfA001ws1", b"discard me\n").await,
        (0, 0)
    );
    s.write_all(b"\x01\n").await.unwrap();
    assert_eq!(
        send_file(&mut s, 2, "cfA001ws1", CONTROL.as_bytes()).await,
        (0, 0)
    );
    assert_eq!(
        send_file(&mut s, 3, "dfA001ws1", b"hello printer\n").await,
        (0, 0)
    );
    drop(s);
    // An unknown queue is refused at once.
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"\x02laser\n").await.unwrap();
    assert_eq!(ack(&mut s).await, 1);
    state.remove_server(id).await;
    assert!(
        TcpStream::connect(addr).await.is_err(),
        "stop releases the port"
    );
}

#[tokio::test]
async fn listings_and_removal_render_the_handlers_answer() {
    let (state, id, addr) = start(handlers(JOB_SCRIPT), json!({})).await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"\x03raw\n").await.unwrap();
    assert_eq!(
        String::from_utf8(closed(&mut s).await).unwrap(),
        "raw is ready and printing\n\
         Rank   Owner      Job  Files                                 Total Size\n\
         active alice      42   report.txt                            1024 bytes\n\
         2      bob        43   notes.txt                             10 bytes\n"
    );
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"\x04raw alice\n").await.unwrap();
    let long = String::from_utf8(closed(&mut s).await).unwrap();
    assert!(
        long.contains("\nalice: active                      [job 042]\n        report.txt"),
        "{long:?}"
    );
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"\x05raw alice 42\n").await.unwrap();
    assert_eq!(
        String::from_utf8(closed(&mut s).await).unwrap(),
        "job 042 dequeued\n"
    );
    // Print-waiting has no reply; the server closes.
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"\x01raw\n").await.unwrap();
    assert!(closed(&mut s).await.is_empty());
    state.remove_server(id).await;
}

#[tokio::test]
async fn declared_bounds_refuse_before_reading() {
    let (state, id, addr) = start(handlers(JOB_SCRIPT), json!({"max_job_bytes": 16})).await;
    // A control file past 64 KiB is refused at its announcement, before any byte of it.
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"\x02raw\n\x0265537 cfA001ws1\n")
        .await
        .unwrap();
    assert_eq!(ack(&mut s).await, 0);
    assert_eq!(ack(&mut s).await, 1);
    assert!(closed(&mut s).await.is_empty());
    // Data past max_job_bytes, counted across the job's files.
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"\x02raw\n").await.unwrap();
    assert_eq!(ack(&mut s).await, 0);
    assert_eq!(
        send_file(&mut s, 3, "dfA001ws1", b"0123456789").await,
        (0, 0)
    );
    s.write_all(b"\x037 dfB001ws1\n").await.unwrap();
    assert_eq!(ack(&mut s).await, 1);
    assert!(closed(&mut s).await.is_empty());
    // A file not terminated by NUL ends the connection without a decision.
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"\x02raw\n").await.unwrap();
    assert_eq!(ack(&mut s).await, 0);
    s.write_all(b"\x033 dfA001ws1\n").await.unwrap();
    assert_eq!(ack(&mut s).await, 0);
    s.write_all(b"abcX").await.unwrap();
    assert!(closed(&mut s).await.is_empty());
    // A command line past 1024 bytes is closed without a reply.
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(&[b'q'; 1100]).await.unwrap();
    assert!(closed(&mut s).await.is_empty());
    state.remove_server(id).await;
}

#[tokio::test]
async fn handler_failure_refuses_jobs_and_removes_nothing() {
    // No handlers and an unreachable model.
    let (state, id, addr) = start(vec![], json!({})).await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"\x02raw\n").await.unwrap();
    assert_eq!(ack(&mut s).await, 0);
    assert_eq!(
        send_file(&mut s, 2, "cfA001ws1", CONTROL.as_bytes()).await,
        (0, 0)
    );
    assert_eq!(
        send_file(&mut s, 3, "dfA001ws1", b"hello printer\n").await,
        (0, 1)
    );
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"\x03raw\n").await.unwrap();
    assert_eq!(
        String::from_utf8(closed(&mut s).await).unwrap(),
        "raw: queue status unavailable\n"
    );
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"\x05raw alice 42\n").await.unwrap();
    assert!(
        closed(&mut s).await.is_empty(),
        "nothing is reported removed"
    );
    state.remove_server(id).await;
}
