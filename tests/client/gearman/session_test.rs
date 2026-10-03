use super::common::*;
use netget::client::gearman::wire as w;
use netget::server::gearman::wire as g;
use serde_json::json;
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
};
#[tokio::test]
async fn netget_submitter_server_pair_progress_data_exception_and_background() {
    let state = super::common::state();
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = tokio::sync::mpsc::unbounded_channel();
    let sid=netget::cli::management::ServerForm{protocol:"gearman".into(),host:Some("127.0.0.1".into()),port:Some(0),event_handlers:Some(vec![static_handler("gearman_job_submitted",json!([{"type":"send_gearman_status","numerator":1,"denominator":2},{"type":"send_gearman_data","data":"partial:"},{"type":"complete_gearman_job","result":"pair ✓"}]))]),..Default::default()}.create(&state,tx).await.unwrap();
    let addr = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(addr) = state.get_server(sid).await.and_then(|s| s.local_addr) {
                break addr;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let id = connected_client(&state, addr.to_string(), "submitter").await;
    let job=request(&state,id,json!({"operation":"submit","function_name":"reverse","unique_id":"pair","workload":"work ✓","priority":"high"})).await;
    let (progress_id, progress) = event(&state, id, "gearman_job_update", 0).await;
    assert_eq!(progress["kind"], "progress");
    assert_eq!(progress["numerator"], 1);
    assert_eq!(progress["denominator"], 2);
    assert_eq!(progress["job_handle"], job["job_handle"]);
    assert_eq!(progress["request"]["unique_id"], "pair");
    let (data_id, data) = event(&state, id, "gearman_job_update", progress_id).await;
    assert_eq!(data["payload"]["text"], "partial:");
    assert_eq!(
        event(&state, id, "gearman_job_update", data_id).await.1["payload"]["text"],
        "pair ✓"
    );
    assert_eq!(
        request(
            &state,
            id,
            json!({"operation":"status","job_handle":job["job_handle"]})
        )
        .await["known"],
        false
    );
    assert_eq!(request(&state,id,json!({"operation":"submit","function_name":"reverse","workload":"background","background":true})).await["background"],true);
    state.remove_client(id).await;
    // The existing NetGet server remains model-as-worker and refuses worker clients.
    let worker = connected_client(&state, addr.to_string(), "worker").await;
    send(
        &state,
        worker,
        json!({"type":"gearman_worker","operation":"register","function_name":"reverse"}),
    )
    .await;
    assert_eq!(
        event(&state, worker, "gearman_error", 0).await.1["code"],
        "not_supported"
    );
    failed(&state, worker, "peer refused").await;
    state.remove_client(worker).await;
    state.remove_server(sid).await;
}
#[tokio::test]
async fn fragmented_job_created_is_not_cancelled_by_busy_injection_and_binary_outcome_drains() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (started_tx, started_rx) = oneshot::channel();
    let (resume_tx, resume_rx) = oneshot::channel();
    let fixture = tokio::spawn(async move {
        let (mut peer, _) = listener.accept().await.unwrap();
        assert_eq!(read_request(&mut peer).await.packet_type, g::SUBMIT_JOB);
        let frame = g::job_created(b"H:test:1");
        peer.write_all(&frame[..3]).await.unwrap();
        started_tx.send(()).unwrap();
        resume_rx.await.unwrap();
        for byte in &frame[3..] {
            peer.write_all(&[*byte]).await.unwrap();
            tokio::task::yield_now().await;
        }
        peer.write_all(&g::work_complete(b"H:test:1", &[0xff, 0, 1]))
            .await
            .unwrap();
    });
    let state = super::common::state();
    let id = connected_client(&state, addr.to_string(), "submitter").await;
    send(
        &state,
        id,
        json!({"operation":"submit","function_name":"work","workload":"x"}),
    )
    .await;
    started_rx.await.unwrap();
    rejected(
        &state,
        id,
        json!({"type":"gearman_request","operation":"echo","data":"busy"}),
        "request pending",
    )
    .await;
    resume_tx.send(()).unwrap();
    let (_, outcome) = event(&state, id, "gearman_job_update", 0).await;
    assert_eq!(outcome["kind"], "complete");
    assert_eq!(
        outcome["payload"],
        json!({"text":null,"bytes":3,"utf8":false})
    );
    fixture.await.unwrap();
    state.remove_client(id).await;
}
#[tokio::test]
async fn disconnect_and_removal_close_pending_reads_and_all_owned_tasks_even_with_manual_handler() {
    for remove in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let fixture = tokio::spawn(async move {
            let (mut peer, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            peer.read_to_end(&mut bytes).await.unwrap();
        });
        let state = super::common::state();
        let id = client(
            &state,
            addr.to_string(),
            "submitter",
            vec![json!({"event_pattern":"*","handler":{"type":"manual","timeout_secs":60}})],
        )
        .await;
        assert_eq!(state.client_task_count(id).await, 3);
        send(
            &state,
            id,
            json!({"operation":"submit","function_name":"work","workload":"pending"}),
        )
        .await;
        if remove {
            state.remove_client(id).await;
        } else {
            let result = state
                .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(1))
                .await
                .unwrap();
            assert!(matches!(
                result,
                netget::state::client_handles::ClientSendOutcome::Disconnected
            ));
        }
        tokio::time::timeout(Duration::from_secs(1), fixture)
            .await
            .unwrap()
            .unwrap();
        state.remove_client(id).await;
        assert_eq!(state.client_task_count(id).await, 0);
    }
}
#[tokio::test]
async fn wrong_roles_assigned_handles_and_ability_bound_are_refused_before_io() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        let (mut peer, _) = listener.accept().await.unwrap();
        let mut registrations = 0;
        loop {
            let mut first = [0];
            if peer.read(&mut first).await.unwrap() == 0 {
                break;
            }
            let mut header = [0; 12];
            header[0] = first[0];
            peer.read_exact(&mut header[1..]).await.unwrap();
            let h = g::parse_header(&header).unwrap();
            let mut body = vec![0; h.size as usize];
            peer.read_exact(&mut body).await.unwrap();
            assert_eq!(h.packet_type, g::CAN_DO);
            registrations += 1;
        }
        assert_eq!(registrations, w::MAX_ABILITIES);
    });
    let state = super::common::state();
    let id = connected_client(&state, addr.to_string(), "worker").await;
    rejected(&state,id,json!({"type":"gearman_request","operation":"submit","function_name":"work","workload":"x"}),"submitter role").await;
    rejected(
        &state,
        id,
        json!({"type":"gearman_worker","operation":"fail","job_handle":"H:other:1"}),
        "assigned",
    )
    .await;
    rejected(
        &state,
        id,
        json!({"type":"gearman_worker","operation":"sleep"}),
        "no_job",
    )
    .await;
    for n in 0..w::MAX_ABILITIES {
        send(&state,id,json!({"type":"gearman_worker","operation":"register","function_name":format!("ability{n}")})).await;
    }
    rejected(
        &state,
        id,
        json!({"type":"gearman_worker","operation":"register","function_name":"overflow"}),
        "ability limit",
    )
    .await;
    state.remove_client(id).await;
    fixture.await.unwrap();
}
#[tokio::test]
async fn foreground_and_assignment_count_limits_prevent_unbounded_job_tables() {
    for role in ["submitter", "worker"] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let fixture = tokio::spawn(async move {
            let (mut peer, _) = listener.accept().await.unwrap();
            if role == "worker" {
                assert_eq!(read_request(&mut peer).await.packet_type, g::CAN_DO);
            }
            for n in 0..w::MAX_JOBS {
                let request = read_request(&mut peer).await;
                let handle = format!("H:test:{n}");
                let packet = if role == "worker" {
                    assert_eq!(request.packet_type, g::GRAB_JOB_UNIQ);
                    g::response(
                        w::JOB_ASSIGN_UNIQ,
                        &[handle.as_bytes(), b"work", b"unique", b"payload"],
                    )
                } else {
                    assert_eq!(request.packet_type, g::SUBMIT_JOB);
                    g::job_created(handle.as_bytes())
                };
                peer.write_all(&packet).await.unwrap();
            }
            let mut remaining = Vec::new();
            peer.read_to_end(&mut remaining).await.unwrap();
            assert!(remaining.is_empty());
        });
        let state = super::common::state();
        let id = connected_client(&state, addr.to_string(), role).await;
        if role == "worker" {
            send(
                &state,
                id,
                json!({"type":"gearman_worker","operation":"register","function_name":"work"}),
            )
            .await;
        }
        let action = if role == "worker" {
            json!({"type":"gearman_worker","operation":"grab_unique"})
        } else {
            json!({"type":"gearman_request","operation":"submit","function_name":"work","workload":"payload"})
        };
        for _ in 0..w::MAX_JOBS {
            request(&state, id, action.clone()).await;
        }
        rejected(&state, id, action, "job limit").await;
        state.remove_client(id).await;
        fixture.await.unwrap();
    }
}
#[tokio::test]
async fn malformed_or_uncorrelated_responses_fail_closed_and_refusal_event_drains() {
    for (action, reply, needle) in [
        (
            json!({"operation":"echo","data":"x"}),
            g::response(g::ECHO_RES, &[b"wrong"]),
            "echo does not match",
        ),
        (
            json!({"operation":"status","job_handle":"H:test:1"}),
            g::status_res(b"H:other:2", false, false, 0, 0),
            "handle does not match",
        ),
        (
            json!({"operation":"status","job_handle":"H:test:1"}),
            g::response(g::STATUS_RES, &[b"H:test:1", b"2", b"0", b"0", b"0"]),
            "status flags",
        ),
        (
            json!({"operation":"status","job_handle":"H:test:1"}),
            g::status_res(b"H:test:1", false, true, 0, 0),
            "Inconsistent",
        ),
        (
            json!({"operation":"echo","data":"x"}),
            g::work_complete(b"H:unknown:1", b"x"),
            "unknown foreground",
        ),
        (
            json!({"operation":"echo","data":"x"}),
            g::job_created(b"H:test:1"),
            "does not match",
        ),
        (
            json!({"operation":"echo","data":"x"}),
            g::response(w::NO_JOB, &[]),
            "does not match",
        ),
        (
            json!({"operation":"echo","data":"x"}),
            g::response(999, &[]),
            "Unsupported",
        ),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let fixture = tokio::spawn(async move {
            let (mut peer, _) = listener.accept().await.unwrap();
            read_request(&mut peer).await;
            peer.write_all(&reply).await.unwrap();
            let mut tail = Vec::new();
            peer.read_to_end(&mut tail).await.unwrap();
        });
        let state = super::common::state();
        let id = connected_client(&state, addr.to_string(), "submitter").await;
        send(&state, id, action).await;
        failed(&state, id, needle).await;
        fixture.await.unwrap();
        state.remove_client(id).await;
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        let (mut peer, _) = listener.accept().await.unwrap();
        read_request(&mut peer).await;
        peer.write_all(&g::error("bad_request", "refused").unwrap())
            .await
            .unwrap();
    });
    let state = super::common::state();
    let id = connected_client(&state, addr.to_string(), "submitter").await;
    send(&state, id, json!({"operation":"echo","data":"x"})).await;
    let (_, error) = event(&state, id, "gearman_error", 0).await;
    assert_eq!(error["code"], "bad_request");
    assert_eq!(error["request"]["operation"], "echo");
    fixture.await.unwrap();
    state.remove_client(id).await;
}
#[tokio::test]
async fn stalled_handler_queue_and_followup_chain_have_limits() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        let (mut peer, _) = listener.accept().await.unwrap();
        read_request(&mut peer).await;
        let mut bytes = g::job_created(b"H:test:1");
        for _ in 0..20 {
            bytes.extend(g::work_data(b"H:test:1", b"waiting"));
        }
        peer.write_all(&bytes).await.unwrap();
        let mut tail = Vec::new();
        peer.read_to_end(&mut tail).await.unwrap();
    });
    let state = super::common::state();
    let id=client(&state,addr.to_string(),"submitter",vec![static_handler("gearman_connected",json!([])),static_handler("gearman_response",json!([])),json!({"event_pattern":"gearman_job_update","handler":{"type":"manual","timeout_secs":60}})]).await;
    event(&state, id, "gearman_connected", 0).await;
    send(
        &state,
        id,
        json!({"operation":"submit","function_name":"work","workload":"x"}),
    )
    .await;
    failed(&state, id, "event queue full").await;
    fixture.await.unwrap();
    state.remove_client(id).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (count_tx, count_rx) = oneshot::channel();
    let fixture = tokio::spawn(async move {
        let (mut peer, _) = listener.accept().await.unwrap();
        for _ in 0..w::MAX_FOLLOWUPS {
            let r = read_request(&mut peer).await;
            assert_eq!(r.packet_type, g::ECHO_REQ);
            peer.write_all(&g::response(g::ECHO_RES, &[b"followup"]))
                .await
                .unwrap();
        }
        count_tx.send(()).unwrap();
        let mut tail = Vec::new();
        peer.read_to_end(&mut tail).await.unwrap();
        assert!(tail.is_empty());
    });
    let state = super::common::state();
    let id = client(
        &state,
        addr.to_string(),
        "submitter",
        vec![static_handler(
            "*",
            json!([{"type":"gearman_request","operation":"echo","data":"followup"}]),
        )],
    )
    .await;
    count_rx.await.unwrap();
    let mut cursor = 0;
    for _ in 0..w::MAX_FOLLOWUPS {
        cursor = event(&state, id, "gearman_response", cursor).await.0;
    }
    state
        .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(1))
        .await
        .unwrap();
    fixture.await.unwrap();
    state.remove_client(id).await;
}

#[tokio::test]
async fn unanswered_request_has_complete_reply_deadline_and_closes_peer() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        let (mut peer, _) = listener.accept().await.unwrap();
        assert_eq!(read_request(&mut peer).await.packet_type, g::ECHO_REQ);
        let mut tail = Vec::new();
        peer.read_to_end(&mut tail).await.unwrap();
    });
    let state = super::common::state();
    let id = connected_client(&state, addr.to_string(), "submitter").await;
    send(&state, id, json!({"operation":"echo","data":"no response"})).await;
    tokio::time::timeout(w::DEADLINE + Duration::from_secs(5), async {
        loop {
            if let netget::state::ClientStatus::Error(e) =
                state.get_client(id).await.unwrap().status
            {
                assert!(e.contains("request reply deadline"), "{e}");
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    fixture.await.unwrap();
    state.remove_client(id).await;
}
