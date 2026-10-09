//! The LPD client against a scripted fixture that asserts every byte: the reachability probe,
//! a job in each file order with the exact control file, a refused final acknowledgement
//! reported as such, long and short listings, removal, print-waiting, and an injected action
//! refused before the wire.
use netget::{
    cli::management::ClientForm,
    state::{app_state::AppState, client_handles::ClientSendOutcome, AccessLogOwner, ClientId},
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::mpsc,
};

pub async fn start(remote: String, params: Value, handlers: Vec<Value>) -> (AppState, ClientId) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let llm = netget::llm::OllamaClient::new("http://127.0.0.1:1");
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "lpd".into(),
        remote_addr: Some(remote),
        instruction: Some("Print test pages".into()),
        startup_params: Some(params),
        event_handlers: Some(handlers),
        ..Default::default()
    }
    .create(&state, llm, tx)
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !state.has_client_handle(id).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (state, id)
}

pub async fn wait_log(state: &AppState, id: ClientId, needle: &str) -> String {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(entry) = state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await
                .iter()
                .map(|e| serde_json::to_string(e).unwrap())
                .find(|e| e.contains(needle))
            {
                break entry;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no client event containing {needle:?}"))
}

async fn line(r: &mut BufReader<TcpStream>) -> String {
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), r.read_until(b'\n', &mut buf))
        .await
        .expect("fixture read deadline")
        .unwrap();
    String::from_utf8(buf).unwrap()
}

/// Read one announced file: returns (code, count, name, body).
async fn file(
    r: &mut BufReader<TcpStream>,
    ack_header: u8,
    ack_body: u8,
) -> (char, String, Vec<u8>) {
    let header = line(r).await;
    let code = header.chars().next().unwrap();
    let (count, name) = header[1..].trim_end().split_once(' ').unwrap();
    let name = name.to_string();
    let count: usize = count.parse().unwrap();
    r.get_mut().write_all(&[ack_header]).await.unwrap();
    let mut body = vec![0u8; count + 1];
    r.read_exact(&mut body).await.unwrap();
    assert_eq!(body.pop(), Some(0), "file terminated by NUL");
    r.get_mut().write_all(&[ack_body]).await.unwrap();
    (code, name, body)
}

async fn accept(listener: &TcpListener) -> BufReader<TcpStream> {
    let (s, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
        .await
        .expect("client dials")
        .unwrap();
    BufReader::new(s)
}

/// Asks for a listing only after a refused job, so the accepted injected job adds no traffic.
const LIST_AFTER_REFUSAL: &str = "import json,sys\ne=json.load(sys.stdin)['event']\na=[] if e['accepted'] else [{'type':'lpd_queue','queue':'raw','long':True,'list':['alice']}]\nprint(json.dumps({'actions':a}))";

#[tokio::test]
async fn jobs_listings_removal_and_injection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        // The reachability probe opens and closes without a command.
        let mut probe = accept(&listener).await;
        assert!(line(&mut probe).await.is_empty());
        // Control file first; the server refuses the job at its final acknowledgement.
        let mut r = accept(&listener).await;
        assert_eq!(line(&mut r).await, "\x02raw\n");
        r.get_mut().write_all(&[0]).await.unwrap();
        let (code, control_name, control) = file(&mut r, 0, 0).await;
        assert_eq!(code, '\x02');
        let number = &control_name[3..6];
        assert_eq!(control_name, format!("cfA{number}ws9"));
        assert_eq!(
            String::from_utf8(control).unwrap(),
            format!("Hws9\nPalice\nJreport\nLalice\nNreport\nfdfA{number}ws9\nUdfA{number}ws9\n")
        );
        let (code, data_name, data) = file(&mut r, 0, 1).await;
        assert_eq!(
            (code, data_name.as_str()),
            ('\x03', format!("dfA{number}ws9").as_str())
        );
        assert_eq!(data, b"hello\n.\n");
        // The long listing the result handler asked for.
        let mut r = accept(&listener).await;
        assert_eq!(line(&mut r).await, "\x04raw alice\n");
        r.get_mut()
            .write_all(b"raw is ready\nalice: active [job 042]\n")
            .await
            .unwrap();
        drop(r);
        // Injected: a data-first job, accepted.
        let mut r = accept(&listener).await;
        assert_eq!(line(&mut r).await, "\x02raw\n");
        r.get_mut().write_all(&[0]).await.unwrap();
        let (code, _, data) = file(&mut r, 0, 0).await;
        assert_eq!((code, data.as_slice()), ('\x03', b"second".as_slice()));
        let (code, _, control) = file(&mut r, 0, 0).await;
        assert_eq!(code, '\x02');
        assert!(
            String::from_utf8(control).unwrap().contains("\nldfA"),
            "literal format"
        );
        // Injected: removal and print-waiting.
        let mut r = accept(&listener).await;
        assert_eq!(line(&mut r).await, "\x05raw alice 42\n");
        r.get_mut().write_all(b"job 042 dequeued\n").await.unwrap();
        drop(r);
        let mut r = accept(&listener).await;
        assert_eq!(line(&mut r).await, "\x01raw\n");
        let mut rest = Vec::new();
        r.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty());
    });
    let (state, id) = start(
        address.to_string(),
        json!({"host":"ws9","user":"alice"}),
        vec![
            json!({"event_pattern":"lpd_ready","handler":{"type":"static","actions":[{"type":"lpd_print","queue":"raw","text":"hello\n.\n","job_name":"report"}]}}),
            json!({"event_pattern":"lpd_print_result","handler":{"type":"script","language":"python","code":LIST_AFTER_REFUSAL}}),
            json!({"event_pattern":"lpd_reply","handler":{"type":"static","actions":[]}}),
        ],
    )
    .await;
    let result = wait_log(&state, id, r#""refused_at":"final""#).await;
    assert!(result.contains(r#""accepted":false"#), "{result}");
    wait_log(&state, id, "alice: active [job 042]").await;
    let rejected = state
        .send_to_client(
            id,
            json!({"type":"lpd_queue","queue":"has space"}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(
        matches!(rejected, ClientSendOutcome::Rejected { .. }),
        "{rejected:?}"
    );
    let sent = state
        .send_to_client(
            id,
            json!({"type":"lpd_print","queue":"raw","text":"second","format":"l","order":"data_first"}),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(matches!(sent, ClientSendOutcome::Sent { .. }), "{sent:?}");
    wait_log(&state, id, r#""accepted":true"#).await;
    for action in [
        json!({"type":"lpd_remove","queue":"raw","agent":"alice","jobs":[42]}),
        json!({"type":"lpd_start_queue","queue":"raw"}),
    ] {
        let sent = state
            .send_to_client(id, action, Duration::from_secs(10))
            .await
            .unwrap();
        assert!(matches!(sent, ClientSendOutcome::Sent { .. }), "{sent:?}");
    }
    wait_log(&state, id, "job 042 dequeued").await;
    wait_log(&state, id, r#""command":"start_queue""#).await;
    fixture.await.unwrap();
    state.remove_client(id).await;
}
