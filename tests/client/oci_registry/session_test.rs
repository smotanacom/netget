use super::common::*;
use netget::state::client_handles::ClientSendOutcome;
use serde_json::json;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    task::{JoinHandle, JoinSet},
};
struct Peer {
    origin: String,
    mode: Arc<Mutex<&'static str>>,
    reads: Arc<AtomicUsize>,
    closed: Arc<AtomicUsize>,
    authorizations: Arc<Mutex<Vec<bool>>>,
    task: JoinHandle<()>,
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Peer {
    async fn start(mode: &'static str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let mode = Arc::new(Mutex::new(mode));
        let reads = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicUsize::new(0));
        let authorizations = Arc::new(Mutex::new(Vec::new()));
        let (m, r, c, a) = (
            mode.clone(),
            reads.clone(),
            closed.clone(),
            authorizations.clone(),
        );
        let task = tokio::spawn(async move {
            let mut children = JoinSet::new();
            loop {
                tokio::select! {
                    accepted=listener.accept()=>{
                        let Ok((mut io,_))=accepted else{break;};let(m,r,c,a)=(m.clone(),r.clone(),c.clone(),a.clone());
                        children.spawn(async move {
                            let mut bytes=Vec::new();let mut byte=[0];
                            while !bytes.ends_with(b"\r\n\r\n")&&bytes.len()<65536 {
                                if io.read(&mut byte).await.unwrap_or(0)==0{return;}bytes.push(byte[0]);
                            }
                            let request=String::from_utf8(bytes).unwrap();let path=request.split_whitespace().nth(1).unwrap();
                            a.lock().unwrap().push(request.to_ascii_lowercase().contains("\r\nauthorization:"));
                            r.fetch_add(1,Ordering::SeqCst);let mode=*m.lock().unwrap();
                            if mode=="startup-hang" || (mode=="hang"&&path!="/v2/") {
                                if io.read(&mut byte).await.unwrap_or(0)==0 {c.fetch_add(1,Ordering::SeqCst);}return;
                            }
                        if mode=="oversized"&&path!="/v2/" {
                                let header=format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",netget::client::oci_registry::api::MAX_BODY+1);
                                let _=io.write_all(header.as_bytes()).await;
                            if io.read(&mut byte).await.unwrap_or(0)==0 {c.fetch_add(1,Ordering::SeqCst);}return;
                        }
                        if mode.starts_with("chunked")&&path!="/v2/" {
                            let _=io.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n").await;
                            let mut remaining=netget::client::oci_registry::api::MAX_BODY+usize::from(mode=="chunked-plus");
                            while remaining>0 {
                                let len=remaining.min(65536);let mut chunk=format!("{len:x}\r\n").into_bytes();chunk.extend(std::iter::repeat_n(b'x',len));chunk.extend_from_slice(b"\r\n");
                                if io.write_all(&chunk).await.is_err(){c.fetch_add(1,Ordering::SeqCst);return;}remaining-=len;
                            }
                            let _=io.write_all(b"0\r\n\r\n").await;return;
                        }
                        if mode=="redirect"&&path!="/v2/" {
                            let _=io.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://example.invalid/v2/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;return;
                        }
                        if mode=="encoded"&&path!="/v2/" {
                            let _=io.write_all(b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await;return;
                        }
                            let body=if path=="/v2/" {b"{}".as_slice()}else{br#"{"name":"library/demo","tags":["latest"]}"#};
                            let header=format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",body.len());
                            let _=io.write_all(header.as_bytes()).await;let _=io.write_all(body).await;
                        });
                    },
                    _=children.join_next(),if !children.is_empty()=>{}
                }
            }
        });
        Self {
            origin,
            mode,
            reads,
            closed,
            authorizations,
            task,
        }
    }
    async fn observed(&self, n: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.reads.load(Ordering::SeqCst) < n {
                tokio::time::sleep(Duration::from_millis(10)).await
            }
        })
        .await
        .unwrap();
    }
    async fn closed(&self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.closed.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await
            }
        })
        .await
        .unwrap();
    }
    async fn stop(self) {
        self.task.abort();
    }
}
#[tokio::test]
async fn startup_parameters_origins_public_probe_and_local_tokens_are_honest() {
    let peer = Peer::start("normal").await;
    let state = state();
    for params in [
        json!({"request_timeout_secs":0}),
        json!({"request_timeout_secs":31}),
        json!({"request_timeout_secs":"1"}),
        json!({"token":"bad\r\n"}),
        json!({"trusted_token_origin":"http://example.invalid"}),
    ] {
        refused_start(&state, peer.origin.clone(), params).await;
    }
    for addr in [
        "http://user:pass@127.0.0.1:5000",
        "http://127.0.0.1:5000/path",
        "http://127.0.0.1:5000?x=1",
    ] {
        refused_start(&state, addr.into(), json!({})).await;
    }
    let id = client(
        &state,
        peer.origin.clone(),
        json!({"token":"private-token"}),
        vec![static_handler("*", json!([]))],
    )
    .await;
    let connected = event(&state, id, "oci_connected", 0).await.1;
    assert_eq!(connected["token_present"], true);
    assert_eq!(connected["authentication_verified"], false);
    assert_eq!(state.client_task_count(id).await, 2);
    assert_eq!(*peer.authorizations.lock().unwrap(), vec![false]);
    send(
        &state,
        id,
        json!({"type":"oci_request","operation":"tags","repository":"library/demo"}),
    )
    .await;
    event(&state, id, "oci_result", 0).await;
    assert_eq!(*peer.authorizations.lock().unwrap(), vec![false, true]);
    let before = latest(&state, id).await;
    send(&state, id, json!({"type":"oci_clear_token"})).await;
    assert_eq!(
        event(&state, id, "oci_authentication", before).await.1["token_present"],
        false
    );
    let before = latest(&state, id).await;
    send(
        &state,
        id,
        json!({"type":"oci_request","operation":"tags","repository":"library/demo"}),
    )
    .await;
    event(&state, id, "oci_result", before).await;
    assert_eq!(
        *peer.authorizations.lock().unwrap(),
        vec![false, true, false]
    );
    let before = latest(&state, id).await;
    send(
        &state,
        id,
        json!({"type":"oci_set_token","token":"private-action-token"}),
    )
    .await;
    let auth = event(&state, id, "oci_authentication", before).await.1;
    assert_eq!(auth["token_present"], true);
    assert_eq!(auth["authentication_verified"], false);
    let before = latest(&state, id).await;
    send(
        &state,
        id,
        json!({"type":"oci_request","operation":"tags","repository":"library/demo"}),
    )
    .await;
    event(&state, id, "oci_result", before).await;
    assert_eq!(
        *peer.authorizations.lock().unwrap(),
        vec![false, true, false, true]
    );
    let logs = state
        .list_access_logs_for(
            Some(netget::state::AccessLogOwner::Client(id.as_u32())),
            None,
        )
        .await;
    assert!(!serde_json::to_string(&logs)
        .unwrap()
        .contains("private-action-token"));
    state.remove_client(id).await;
    assert_eq!(state.client_task_count(id).await, 0);
    peer.stop().await;
}
#[tokio::test]
async fn pending_request_rejects_overlap_and_disconnect_cancels_owned_http() {
    let peer = Peer::start("hang").await;
    let state = state();
    let id = connected_client(&state, peer.origin.clone()).await;
    let injected = state.clone();
    let pending = tokio::spawn(async move {
        injected
            .send_to_client(
                id,
                json!({"type":"oci_request","operation":"tags","repository":"library/demo"}),
                Duration::from_secs(5),
            )
            .await
    });
    peer.observed(2).await;
    rejected(
        &state,
        id,
        json!({"type":"oci_request","operation":"probe"}),
        "pending",
    )
    .await;
    rejected(
        &state,
        id,
        json!({"type":"disconnect","unknown":"value"}),
        "pending",
    )
    .await;
    let outcome = state
        .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(2))
        .await
        .unwrap();
    assert!(matches!(outcome, ClientSendOutcome::Disconnected));
    assert!(pending.await.unwrap().is_err());
    peer.closed().await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.has_client_handle(id).await {
            tokio::time::sleep(Duration::from_millis(10)).await
        }
    })
    .await
    .unwrap();
    state.remove_client(id).await;
    assert_eq!(state.client_task_count(id).await, 0);
    peer.stop().await;
}
#[tokio::test]
async fn removal_cancels_pending_request_and_parked_handler() {
    let peer = Peer::start("hang").await;
    let state = state();
    let id = client(
        &state,
        peer.origin.clone(),
        json!({}),
        vec![json!({"event_pattern":"*","handler":{"type":"manual"}})],
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.list_intercepts().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await
        }
    })
    .await
    .unwrap();
    let injected = state.clone();
    let pending = tokio::spawn(async move {
        injected
            .send_to_client(
                id,
                json!({"type":"oci_request","operation":"tags","repository":"library/demo"}),
                Duration::from_secs(5),
            )
            .await
    });
    peer.observed(2).await;
    state.remove_client(id).await;
    assert!(pending.await.unwrap().is_err());
    peer.closed().await;
    assert_eq!(state.client_task_count(id).await, 0);
    assert!(state.list_intercepts().await.is_empty());
    peer.stop().await;
}
#[tokio::test]
async fn whole_deadline_and_oversized_native_body_refuse_without_success_then_recover() {
    for mode in ["hang", "oversized"] {
        let peer = Peer::start(mode).await;
        let state = state();
        let id = client(
            &state,
            peer.origin.clone(),
            json!({"request_timeout_secs":1}),
            vec![static_handler("*", json!([]))],
        )
        .await;
        event(&state, id, "oci_connected", 0).await;
        let before = latest(&state, id).await;
        let request = json!({"type":"oci_request","operation":"tags","repository":"library/demo"});
        assert!(state
            .send_to_client(id, request, Duration::from_secs(3))
            .await
            .is_err());
        event(&state, id, "oci_request_error", before).await;
        peer.closed().await;
        assert!(!state
            .list_access_logs_for(
                Some(netget::state::AccessLogOwner::Client(id.as_u32())),
                None
            )
            .await
            .iter()
            .any(|e| e.id > before && e.event_type == "oci_result"));
        *peer.mode.lock().unwrap() = "normal";
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
        state.remove_client(id).await;
        peer.stop().await;
    }
}
#[tokio::test]
async fn startup_probe_has_a_whole_deadline() {
    let peer = Peer::start("startup-hang").await;
    let state = state();
    let error = refused_start(
        &state,
        peer.origin.clone(),
        json!({"request_timeout_secs":1}),
    )
    .await;
    assert!(!error.is_empty());
    assert_eq!(
        peer.reads.load(Ordering::SeqCst),
        1,
        "failure occurred after the native hanging probe"
    );
    peer.closed().await;
    peer.stop().await;
}
#[tokio::test]
async fn manual_handler_queue_is_exactly_bounded_and_overflow_stops_owned_tasks() {
    let peer = Peer::start("normal").await;
    let state = state();
    let id = client(
        &state,
        peer.origin.clone(),
        json!({}),
        vec![json!({"event_pattern":"*","handler":{"type":"manual"}})],
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.list_intercepts().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await
        }
    })
    .await
    .unwrap();
    for _ in 0..netget::client::oci_registry::QUEUE_CAPACITY {
        send(
            &state,
            id,
            json!({"type":"oci_request","operation":"probe"}),
        )
        .await;
    }
    assert!(
        state.has_client_handle(id).await,
        "eight events fit while connected is parked"
    );
    send(
        &state,
        id,
        json!({"type":"oci_request","operation":"probe"}),
    )
    .await;
    failed(&state, id, "bounded session").await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.has_client_handle(id).await || !state.list_intercepts().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await
        }
    })
    .await
    .unwrap();
    state.remove_client(id).await;
    assert_eq!(state.client_task_count(id).await, 0);
    peer.stop().await;
}
#[tokio::test]
async fn handler_followups_are_bounded_and_fresh_injection_remains_available() {
    let peer = Peer::start("normal").await;
    let state = state();
    let id = client(
        &state,
        peer.origin.clone(),
        json!({}),
        vec![
            static_handler(
                "oci_connected",
                json!([{"type":"oci_request","operation":"probe"}]),
            ),
            static_handler(
                "oci_result",
                json!([{"type":"oci_request","operation":"probe"}]),
            ),
        ],
    )
    .await;
    let mut after = 0;
    for _ in 0..4 {
        after = event(&state, id, "oci_result", after).await.0;
    }
    // A local clear action is ordered after the followup refusal and does not
    // create another handler chain, so it is a concrete synchronization point.
    send(&state, id, json!({"type":"oci_clear_token"})).await;
    assert_eq!(peer.reads.load(Ordering::SeqCst), 5);
    state.remove_client(id).await;
    peer.stop().await;
}
#[tokio::test]
async fn native_chunked_blob_bound_is_exact_and_redirects_or_encoding_are_refused() {
    for mode in ["chunked-max", "chunked-plus", "redirect", "encoded"] {
        let peer = Peer::start(mode).await;
        let state = state();
        let id = connected_client(&state, peer.origin.clone()).await;
        let digest = netget::server::oci_registry::actions::sha256_digest(
            &vec![b'x'; netget::client::oci_registry::api::MAX_BODY],
        );
        let before = latest(&state, id).await;
        let outcome=state.send_to_client(id,json!({"type":"oci_request","operation":"blob","repository":"library/demo","reference":digest}),Duration::from_secs(3)).await;
        if mode == "chunked-max" {
            assert!(matches!(outcome.unwrap(), ClientSendOutcome::Sent { .. }));
            let result = event(&state, id, "oci_result", before).await.1;
            assert_eq!(result["data"]["digest"], digest);
            assert_eq!(result["data"]["digest_verified"], true);
            assert_eq!(result["data"]["content_omitted"], true);
        } else {
            assert!(outcome.is_err());
            event(&state, id, "oci_request_error", before).await;
        }
        assert_eq!(
            peer.reads.load(Ordering::SeqCst),
            2,
            "no redirect or automatic retry"
        );
        state.remove_client(id).await;
        peer.stop().await;
    }
}
