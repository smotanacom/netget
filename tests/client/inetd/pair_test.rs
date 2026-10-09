//! The six inetd clients against NetGet's own servers, over TCP and UDP: each query's answer
//! decoded into its response event, Echo's match check, Chargen's pattern check, Time's
//! RFC 868 decoding, and an injected query refused before the wire.
use netget::{
    cli::management::ClientForm,
    state::{app_state::AppState, client_handles::ClientSendOutcome, AccessLogOwner, ClientId},
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;

pub async fn client(
    protocol: &str,
    remote: String,
    transport: &str,
    ready: &str,
    query: Value,
) -> (AppState, ClientId) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: protocol.into(),
        remote_addr: Some(remote),
        instruction: Some("Query the service".into()),
        startup_params: Some(json!({"transport": transport})),
        event_handlers: Some(vec![
            json!({"event_pattern": ready, "handler": {"type":"static","actions":[query]}}),
            json!({"event_pattern": "*", "handler": {"type":"static","actions":[]}}),
        ]),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
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

pub async fn wait_log(state: &AppState, id: ClientId, needles: &[&str]) -> String {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(entry) = state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await
                .iter()
                .map(|e| serde_json::to_string(e).unwrap())
                .find(|e| needles.iter().all(|n| e.contains(n)))
            {
                break entry;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no client event containing all of {needles:?}"))
}

#[tokio::test]
async fn every_client_reads_its_netget_server_over_both_transports() {
    use crate::helpers::inetd::{answer, start};
    let cases: [(&str, &str, Value, &str, Value, &[&str]); 6] = [
        (
            "echo",
            "echo_request",
            json!({"type":"echo_reply"}),
            "echo_ready",
            json!({"type":"echo_send","data":"round trip"}),
            &[r#""data":"round trip""#, r#""matches":true"#],
        ),
        (
            "discard",
            "discard_request",
            json!({"type":"discard_reply"}),
            "discard_ready",
            json!({"type":"discard_send","data":"gone"}),
            &[r#""bytes":4"#],
        ),
        (
            "daytime",
            "daytime_request",
            json!({"type":"daytime_reply","text":"Saturday, January 1, 2000 12:00:00-UTC"}),
            "daytime_ready",
            json!({"type":"daytime_query"}),
            &["Saturday, January 1, 2000 12:00:00-UTC\""],
        ),
        (
            "qotd",
            "qotd_request",
            json!({"type":"qotd_reply","quote":"first\nsecond"}),
            "qotd_ready",
            json!({"type":"qotd_query"}),
            &[r#""quote":"first\nsecond""#],
        ),
        (
            "chargen",
            "chargen_request",
            json!({"type":"chargen_reply"}),
            "chargen_ready",
            json!({"type":"chargen_query","bytes":300}),
            &[r#""conforms":true"#],
        ),
        (
            "time",
            "time_request",
            json!({"type":"time_reply","unix_seconds":946728000}),
            "time_ready",
            json!({"type":"time_query"}),
            &[
                r#""unix_seconds":946728000"#,
                r#""seconds_since_1900":3155716800"#,
                "2000-01-01T12:00:00+00:00",
            ],
        ),
    ];
    for (protocol, event, reply, ready, query, needles) in cases {
        let (sstate, sid, addr) = start(protocol, answer(event, reply), json!({})).await;
        for transport in ["tcp", "udp"] {
            let (state, id) =
                client(protocol, addr.to_string(), transport, ready, query.clone()).await;
            let mut wanted = needles.to_vec();
            let transport_needle = format!(r#""transport":"{transport}""#);
            wanted.push(&transport_needle);
            wait_log(&state, id, &wanted).await;
            state.remove_client(id).await;
        }
        sstate.remove_server(sid).await;
    }
}

#[tokio::test]
async fn echo_reports_a_mismatch_and_injected_queries_are_checked() {
    use crate::helpers::inetd::{answer, start};
    let (sstate, sid, addr) = start(
        "echo",
        answer(
            "echo_request",
            json!({"type":"echo_reply","data":"tampered"}),
        ),
        json!({}),
    )
    .await;
    let (state, id) = client(
        "echo",
        addr.to_string(),
        "tcp",
        "echo_ready",
        json!({"type":"echo_send","data":"original"}),
    )
    .await;
    wait_log(&state, id, &[r#""matches":false"#, r#""data":"tampered""#]).await;
    let rejected = state
        .send_to_client(
            id,
            json!({"type":"echo_send","data":"zz","encoding":"hex"}),
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
            json!({"type":"echo_send","data":"00ff","encoding":"hex"}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(
        matches!(sent, ClientSendOutcome::Sent { bytes_sent: 2 }),
        "{sent:?}"
    );
    state.remove_client(id).await;
    sstate.remove_server(sid).await;
}
