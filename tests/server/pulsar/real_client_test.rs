//! NetGet's Pulsar broker against two independent clients: the official **Python client**
//! (`pulsar-client` 3.13, a binding to the C++ library) and the **Java CLI** shipped with
//! Apache Pulsar 4.0.6 (`bin/pulsar-client`). Both look the topic up, create producers and
//! consumers, publish (the Python one also batched), and read back what NetGet delivered,
//! including a message the model refused and messages the model published itself. Then raw
//! frames for a bad checksum, the frame bound and a failed handler. `install_peers.py` prints
//! `NETGET_PULSAR_PYTHON` and `NETGET_PULSAR_HOME`; the tests fail rather than skip without
//! them. No LLM calls: a python policy is the model.
use netget::cli::management::ServerForm;
use netget::server::pulsar::wire::{self, command_type as t, BaseCommand};
use netget::state::app_state::AppState;
use netget::state::{AccessLogOwner, ServerId};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

const POLICY: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
if t=='pulsar_message' and 'forbidden' in e['payload']:
  a=[{'type':'pulsar_reject','message':'no forbidden orders'}]
elif t=='pulsar_message' and e['topic'].endswith('/orders'):
  a=[{'type':'pulsar_accept'},{'type':'pulsar_publish','topic':'replies','payload':'ack '+e['payload'],'properties':{'for':e['producer_name']}}]
else:
  a=[{'type':'pulsar_accept'}]
print(json.dumps({'actions':a}))"#;

fn env(var: &str) -> String {
    let v = std::env::var(var).unwrap_or_default();
    assert!(
        !v.is_empty(),
        "{var} is required: python3 tests/server/pulsar/install_peers.py <dir> and export what it prints"
    );
    v
}

async fn start(handlers: Option<Vec<Value>>) -> (AppState, ServerId, u16) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "pulsar".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Be a careful broker".into()),
        event_handlers: handlers,
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .unwrap();
    let port = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(a) = state.get_server(id).await.and_then(|s| s.local_addr) {
                break a.port();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
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

async fn events(state: &AppState, id: ServerId, kind: &str) -> Vec<Value> {
    state
        .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
        .await
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .filter(|e| e["event_type"] == kind)
        .map(|e| e["request"].clone())
        .collect()
}

#[tokio::test]
async fn the_python_client_produces_and_consumes_through_netget() {
    let (state, id, port) = start(policy()).await;
    let out = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new(env("NETGET_PULSAR_PYTHON"))
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/server/pulsar/py_session.py"
            ))
            .arg(format!("pulsar://127.0.0.1:{port}"))
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("the python client did not finish")
    .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(out.status.success(), "{stdout}\n{stderr}");
    let steps: Vec<Value> = stdout
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let refused = steps.iter().find(|s| s["step"] == "refused").unwrap();
    assert_eq!(
        refused["ok"], true,
        "the refused message was not refused: {stdout}"
    );
    // The C++ library fails a ChecksumError send (the one refusal it does not answer by
    // reconnecting) and logs the broker's text: the model's reason.
    assert!(
        format!("{stdout}{stderr}").contains("no forbidden orders"),
        "{stdout}{stderr}"
    );
    let done = steps
        .iter()
        .find(|s| s["step"] == "done")
        .unwrap_or_else(|| panic!("{stdout}\n{stderr}"));

    let received: Vec<&str> = done["received"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["data"].as_str().unwrap())
        .collect();
    assert_eq!(
        received,
        [
            "order 1",
            "order 2 \u{2713}",
            "order 3",
            "batch 0",
            "batch 1",
            "batch 2"
        ],
        "{done}"
    );
    let first = &done["received"][0];
    assert_eq!(first["properties"], json!({"customer": "ada"}), "{first}");
    assert_eq!(first["key"], "ada", "{first}");
    assert_eq!(done["received"][4]["properties"], json!({"i": "1"}));
    let replies: Vec<&str> = done["replies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["data"].as_str().unwrap())
        .collect();
    assert_eq!(
        replies,
        [
            "ack order 1",
            "ack order 2 \u{2713}",
            "ack order 3",
            "ack batch 0",
            "ack batch 1",
            "ack batch 2"
        ]
    );
    assert_eq!(
        done["replies"][0]["properties"],
        json!({"for": "py-producer"})
    );

    let messages = events(&state, id, "pulsar_message").await;
    assert_eq!(
        messages.len(),
        7,
        "six accepted and one refused: {messages:?}"
    );
    let forbidden = messages
        .iter()
        .find(|m| m["payload"] == "forbidden order")
        .unwrap();
    assert_eq!(forbidden["topic"], "persistent://public/default/orders");
    assert_eq!(forbidden["encoding"], "utf8");
    let producers = events(&state, id, "pulsar_producer").await;
    assert_eq!(
        producers.len(),
        2,
        "no reconnect after the refusal: {producers:?}"
    );
    for name in ["py-producer", "py-batch"] {
        assert!(
            producers.iter().any(|p| p["producer_name"] == name),
            "{name}: {producers:?}"
        );
    }
    assert_eq!(events(&state, id, "pulsar_subscribe").await.len(), 2);
}

#[tokio::test]
async fn the_java_cli_produces_and_consumes_through_netget() {
    let (state, id, port) = start(policy()).await;
    let cli = format!("{}/bin/pulsar-client", env("NETGET_PULSAR_HOME"));
    let url = format!("pulsar://127.0.0.1:{port}");
    let consumer = tokio::process::Command::new(&cli)
        .args([
            "--url",
            &url,
            "consume",
            "-s",
            "java-sub",
            "-n",
            "2",
            "persistent://public/default/replies",
        ])
        .kill_on_drop(true)
        .output();
    let consumer =
        tokio::spawn(async move { tokio::time::timeout(Duration::from_secs(120), consumer).await });
    // The consumer subscribes at the latest message: wait until it has.
    tokio::time::timeout(Duration::from_secs(90), async {
        while events(&state, id, "pulsar_subscribe").await.is_empty() {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("the Java consumer never subscribed");
    let produced = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new(&cli)
            .args([
                "--url",
                &url,
                "produce",
                "-m",
                "from java",
                "-n",
                "2",
                "-p",
                "origin=java",
                "orders",
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("the Java producer did not finish")
    .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&produced.stdout),
        String::from_utf8_lossy(&produced.stderr)
    );
    assert!(produced.status.success(), "{text}");
    assert!(text.contains("2 messages successfully produced"), "{text}");
    let consumed = consumer
        .await
        .unwrap()
        .expect("the Java consumer did not finish")
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&consumed.stdout),
        String::from_utf8_lossy(&consumed.stderr)
    );
    assert!(consumed.status.success(), "{text}");
    assert_eq!(text.matches("content:ack from java").count(), 2, "{text}");
    assert!(text.contains("2 messages successfully consumed"), "{text}");
    // The Java client fails a message refused with NotAllowedError, and stops there.
    let refused = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new(&cli)
            .args([
                "--url",
                &url,
                "produce",
                "-m",
                "forbidden from java",
                "-n",
                "1",
                "orders",
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("the Java producer did not finish")
    .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&refused.stdout),
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(text.contains("no forbidden orders"), "{text}");
    assert!(!text.contains("1 messages successfully produced"), "{text}");
    let messages = events(&state, id, "pulsar_message").await;
    assert_eq!(messages.len(), 3, "{messages:?}");
    assert!(messages
        .iter()
        .filter(|m| m["payload"] == "from java")
        .all(|m| m["properties"] == json!({"origin": "java"})));
}

/// A raw connection for the frames no client library would send.
async fn raw(port: u16) -> TcpStream {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut c = BaseCommand::of(t::CONNECT);
    c.connect = Some(wire::CommandConnect {
        client_version: "raw".into(),
        protocol_version: Some(19),
        ..Default::default()
    });
    s.write_all(&wire::simple(&c)).await.unwrap();
    let (answer, _) = wire::read_frame(&mut s, Duration::from_secs(10))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answer.r#type, t::CONNECTED);
    s
}

async fn producer(s: &mut TcpStream) -> BaseCommand {
    let mut c = BaseCommand::of(t::PRODUCER);
    c.producer = Some(wire::CommandProducer {
        topic: "raw".into(),
        producer_id: 0,
        request_id: 0,
        ..Default::default()
    });
    s.write_all(&wire::simple(&c)).await.unwrap();
    wire::read_frame(s, Duration::from_secs(30))
        .await
        .unwrap()
        .unwrap()
        .0
}

#[tokio::test]
async fn checksums_bounds_and_a_failed_handler() {
    let (state, id, port) = start(policy()).await;
    let mut s = raw(port).await;
    // Zero-valued required fields are still on the wire, so producer 0 / request 0 work.
    let ok = producer(&mut s).await;
    assert_eq!(ok.r#type, t::PRODUCER_SUCCESS, "{ok:?}");
    // A SEND whose checksum is wrong is refused with ChecksumError and reaches nobody.
    let meta = wire::MessageMetadata {
        producer_name: "raw".into(),
        sequence_id: 0,
        publish_time: 1,
        ..Default::default()
    };
    let mut send = BaseCommand::of(t::SEND);
    send.send = Some(wire::CommandSend {
        producer_id: 0,
        sequence_id: 0,
        ..Default::default()
    });
    let mut frame = wire::with_payload(&send, &meta, b"hello");
    let last = frame.len() - 1;
    frame[last] ^= 0xff;
    s.write_all(&frame).await.unwrap();
    let (r, _) = wire::read_frame(&mut s, Duration::from_secs(10))
        .await
        .unwrap()
        .unwrap();
    let e = r.send_error.expect("a SendError");
    assert_eq!(e.error, wire::server_error::CHECKSUM_ERROR, "{e:?}");
    // The intact frame is accepted, with a receipt naming entry 0.
    frame[last] ^= 0xff;
    s.write_all(&frame).await.unwrap();
    let (r, _) = wire::read_frame(&mut s, Duration::from_secs(30))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        r.send_receipt
            .expect("a receipt")
            .message_id
            .unwrap()
            .entry_id,
        0
    );
    assert_eq!(events(&state, id, "pulsar_message").await.len(), 1);

    // A frame announcing more than MAX_FRAME ends the connection before it is read.
    let mut big = raw(port).await;
    big.write_all(&((wire::MAX_FRAME + 1) as u32).to_be_bytes())
        .await
        .unwrap();
    let mut buf = [0u8; 1];
    let end = tokio::time::timeout(Duration::from_secs(10), big.read(&mut buf))
        .await
        .unwrap();
    assert!(matches!(end, Ok(0) | Err(_)), "{end:?}");

    // With no model, a producer is refused with a category and nothing else.
    let (_state, _id, port) = start(None).await;
    let mut s = raw(port).await;
    let refused = producer(&mut s).await;
    let e = refused.error.expect("an error");
    assert_eq!(e.error, wire::server_error::SERVICE_NOT_READY, "{e:?}");
    assert!(
        !e.message.contains("127.0.0.1") && !e.message.contains("LLM"),
        "{e:?}"
    );
}
