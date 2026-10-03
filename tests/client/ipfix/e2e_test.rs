use netget::cli::management::{ClientForm, ServerForm};
use netget::server::ipfix::codec::{Batch, TemplateCache};
use netget::state::{
    app_state::AppState, client_handles::ClientSendOutcome, AccessLogOwner, ClientId, ClientStatus,
};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::{net::UdpSocket, sync::mpsc};
pub(super) async fn start(
    remote: String,
    handlers: Option<Vec<Value>>,
    params: Option<Value>,
) -> (AppState, ClientId) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "ipfix".into(),
        remote_addr: Some(remote),
        instruction: Some(String::new()),
        event_handlers: handlers,
        startup_params: params,
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
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    (state, id)
}
pub(super) fn batch() -> Value {
    netget::client::ipfix::actions::example_batch()
}
pub(super) async fn send(state: &AppState, id: ClientId, batch: Value) -> ClientSendOutcome {
    state
        .send_to_client(
            id,
            json!({"type":"export_ipfix_records","batch":batch}),
            Duration::from_secs(15),
        )
        .await
        .unwrap()
}
pub(super) async fn receive(peer: &UdpSocket) -> (Vec<u8>, SocketAddr) {
    let mut b = vec![0; 8193];
    let (n, addr) = tokio::time::timeout(Duration::from_secs(5), peer.recv_from(&mut b))
        .await
        .unwrap()
        .unwrap();
    b.truncate(n);
    (b, addr)
}
async fn disconnected(state: &AppState, id: ClientId) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.get_client(id).await.unwrap().status != ClientStatus::Disconnected {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn atomic_validation_live_injection_template_refresh_and_disconnect_work_with_parked_handler()
{
    let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let(state,id)=start(peer.local_addr().unwrap().to_string(),Some(vec![json!({"event_pattern":"ipfix_connected","handler":{"type":"manual","timeout_secs":300}})]),Some(json!({"template_refresh_seconds":1}))).await;
    let mut bad = batch();
    bad["data_sets"][0]["records"][0][0] = json!({"kind":"unsigned","value":1});
    assert!(matches!(
        send(&state, id, bad).await,
        ClientSendOutcome::Rejected { .. }
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), peer.recv(&mut [0; 8193]))
            .await
            .is_err()
    );
    assert!(matches!(
        send(&state, id, batch()).await,
        ClientSendOutcome::Executed { .. }
    ));
    let (w, source) = receive(&peer).await;
    let mut cache = TemplateCache::new(Duration::from_secs(10), Duration::from_secs(30));
    let m = cache
        .ingest(source, &w, tokio::time::Instant::now())
        .unwrap();
    assert_eq!(m.sequence_number, 0);
    assert_eq!(m.record_count, 1);
    let (w, source) = receive(&peer).await;
    let m = cache
        .ingest(source, &w, tokio::time::Instant::now())
        .unwrap();
    assert_eq!(m.sequence_number, 1);
    assert_eq!(m.record_count, 0);
    assert!(m.template_changes.is_empty());
    assert!(!state.list_intercepts().await.is_empty());
    let outcome = state
        .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(5))
        .await
        .unwrap();
    assert!(matches!(outcome, ClientSendOutcome::Disconnected));
    disconnected(&state, id).await;
    assert!(state.list_intercepts().await.is_empty());
    assert!(
        tokio::time::timeout(Duration::from_millis(1100), peer.recv(&mut [0; 8193]))
            .await
            .is_err()
    );
    state.remove_client(id).await;
}
#[tokio::test]
async fn domain_sequences_count_records_options_not_templates_and_reject_schema_reuse() {
    let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (state, id) = start(peer.local_addr().unwrap().to_string(), None, None).await;
    let mut cache = TemplateCache::new(Duration::from_secs(10), Duration::from_secs(30));
    for (domain, expected, scope) in [(42, 0, 0), (43, 0, 1), (42, 1, 0), (43, 1, 1)] {
        let mut b = batch();
        b["observation_domain_id"] = json!(domain);
        b["templates"][0]["scope_count"] = json!(scope);
        assert!(matches!(
            send(&state, id, b).await,
            ClientSendOutcome::Executed { .. }
        ));
        let (w, source) = receive(&peer).await;
        let m = cache
            .ingest(source, &w, tokio::time::Instant::now())
            .unwrap();
        assert_eq!(m.sequence_number, expected);
        assert_eq!(m.record_count, 1);
        assert_eq!(m.data_sets[0].template.scope_count, scope);
    }
    let mut bad = batch();
    bad["templates"][0]["fields"][0]["element"] = json!("destination_ipv4_address");
    assert!(matches!(
        send(&state, id, bad).await,
        ClientSendOutcome::Rejected { .. }
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), peer.recv(&mut [0; 8193]))
            .await
            .is_err()
    );
    state.remove_client(id).await;
}
#[tokio::test]
async fn response_event_queue_action_cap_and_removal_are_bounded() {
    let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let(state,id)=start(peer.local_addr().unwrap().to_string(),Some(vec![json!({"event_pattern":"ipfix_connected","handler":{"type":"manual","timeout_secs":300}})]),Some(json!({"template_refresh_seconds":3600}))).await;
    for _ in 0..33 {
        assert!(matches!(
            send(&state, id, batch()).await,
            ClientSendOutcome::Executed { .. }
        ));
        receive(&peer).await;
    }
    disconnected(&state, id).await;
    assert!(state.list_intercepts().await.is_empty());
    state.remove_client(id).await;
    let(state,id)=start(peer.local_addr().unwrap().to_string(),Some(vec![json!({"event_pattern":"ipfix_connected","handler":{"type":"static","actions":vec![json!({"type":"export_ipfix_records","batch":batch()});33]}})]),None).await;
    disconnected(&state, id).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), peer.recv(&mut [0; 8193]))
            .await
            .is_err()
    );
    state.remove_client(id).await;
    let(state,id)=start(peer.local_addr().unwrap().to_string(),Some(vec![json!({"event_pattern":"ipfix_connected","handler":{"type":"manual","timeout_secs":300}})]),None).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.list_intercepts().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    state.remove_client(id).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !state.list_intercepts().await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("aborted owned handler must release its intercept");
}
#[tokio::test]
async fn unsolicited_reply_closes_one_way_session() {
    let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (state, id) = start(peer.local_addr().unwrap().to_string(), None, None).await;
    send(&state, id, batch()).await;
    let (_, source) = receive(&peer).await;
    peer.send_to(b"unexpected", source).await.unwrap();
    disconnected(&state, id).await;
    state.remove_client(id).await;
}
#[tokio::test]
async fn native_pair_uses_common_memory_and_stops_at_followup_depth() {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let server = ServerForm {
        protocol: "ipfix".into(),
        host: Some("127.0.0.1".into()),
        port: Some(0),
        instruction: Some(String::new()),
        ..Default::default()
    }
    .create(&state, tx.clone())
    .await
    .unwrap();
    let addr = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(addr) = state.get_server(server).await.and_then(|s| s.local_addr) {
                break addr;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let b = batch();
    let code=format!("import json,sys\nx=json.load(sys.stdin)\nn=int(x['client']['memory'] or '0')+1\nassert x['event']['local_transport_only']\nprint(json.dumps({{'actions':[{{'type':'set_memory','value':str(n)}},{{'type':'export_ipfix_records','batch':json.loads({})}}]}}))",serde_json::to_string(&b.to_string()).unwrap());
    let client=ClientForm{protocol:"ipfix".into(),remote_addr:Some(addr.to_string()),instruction:Some(String::new()),event_handlers:Some(vec![json!({"event_pattern":"ipfix_connected","handler":{"type":"static","actions":[{"type":"export_ipfix_records","batch":b}]}}),json!({"event_pattern":"ipfix_exported","handler":{"type":"script","language":"python","code":code}})]),..Default::default()}.create(&state,netget::llm::OllamaClient::new("http://127.0.0.1:1"),tx).await.unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        while state.get_client(client).await.unwrap().memory != "8"
            || state
                .list_access_logs_for(Some(AccessLogOwner::Server(server.as_u32())), None)
                .await
                .iter()
                .filter(|e| e.event_type == "ipfix_message")
                .count()
                != 8
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let logs = state
        .list_access_logs_for(Some(AccessLogOwner::Server(server.as_u32())), None)
        .await;
    assert_eq!(
        logs.iter()
            .filter(|e| e.event_type == "ipfix_message")
            .count(),
        8
    );
    state.remove_client(client).await;
    state.remove_server(server).await;
}
#[test]
fn catalog_domain_template_caps_modulo_sequence_and_canonical_schemas_are_atomic() {
    use netget::client::ipfix::transport::{Catalog, Domain};
    let mut c = Catalog::default();
    let b: Batch = serde_json::from_value(batch()).unwrap();
    for id in 0..32 {
        let mut b = b.clone();
        b.observation_domain_id = id;
        let p = c.prepare(&b).unwrap();
        c.domains.insert(id, p.domain);
    }
    let mut more = b.clone();
    more.observation_domain_id = 32;
    assert!(c.prepare(&more).is_err());
    assert_eq!(c.domains.len(), 32);
    let mut c = Catalog::default();
    let mut d = Domain::default();
    d.sequence = u32::MAX;
    c.domains.insert(42, d);
    let p = c.prepare(&b).unwrap();
    assert_eq!(p.info.sequence_number, u32::MAX);
    assert_eq!(p.domain.sequence, 0);
    c.domains.insert(42, p.domain);
    let mut same = b.clone();
    same.templates[0].fields[0].length = Some(4);
    assert!(c.prepare(&same).is_ok());
    let mut bad = b.clone();
    bad.data_sets[0].records[0].clear();
    assert!(c.prepare(&bad).is_err());
    assert_eq!(c.domains[&42].sequence, 0);
    let mut all = b.clone();
    all.data_sets.clear();
    all.templates = (256..288)
        .map(|id| {
            let mut t = b.templates[0].clone();
            t.id = id;
            t
        })
        .collect();
    let p = c.prepare(&all).unwrap();
    c.domains.insert(42, p.domain);
    let mut extra = b.clone();
    extra.templates[0].id = 300;
    extra.data_sets[0].template_id = 300;
    assert!(c.prepare(&extra).is_err());
    assert_eq!(c.domains[&42].templates.len(), 32);
    assert!(netget::server::ipfix::duration(0).is_err());
    assert!(netget::server::ipfix::duration(86401).is_err());
    assert!(netget::server::ipfix::duration(1).is_ok());
    assert!(netget::server::ipfix::duration(86400).is_ok());
}
