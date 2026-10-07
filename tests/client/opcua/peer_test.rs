use crate::helpers::ics::*;
use serde_json::json;
#[tokio::test(flavor = "multi_thread")]
async fn independent_asyncua_server_all_operations() {
    let (mut p, a) = peer_server("opcua").await;
    let s = state();
    let id = client_in(&s, a.to_string(), "opcua", quiet(), json!({}))
        .await
        .unwrap();
    let r = send(
        &s,
        id,
        json!({"type":"opcua_browse","node_id":"ns=2;s=Device"}),
    )
    .await;
    assert!(r["references"].as_array().unwrap().len() >= 2);
    assert_eq!(
        send(
            &s,
            id,
            json!({"type":"opcua_read","node_id":"ns=2;s=Value"})
        )
        .await["value"],
        12.5
    );
    assert_eq!(
        send(
            &s,
            id,
            json!({"type":"opcua_subscribe","node_id":"ns=2;s=Value"})
        )
        .await["success"],
        true
    );
    assert_eq!(send(&s,id,json!({"type":"opcua_write","node_id":"ns=2;s=Value","value_type":"double","value":23.5})).await["success"],true);
    assert_eq!(
        send(
            &s,
            id,
            json!({"type":"opcua_read","node_id":"ns=2;s=Value"})
        )
        .await["value"],
        23.5
    );
    assert_eq!(send(&s,id,json!({"type":"opcua_call","object_id":"ns=2;s=Device","method_id":"ns=2;s=Method","arguments":[{"value_type":"double","value":3.0}]})).await["outputs"][0]["value"],6.0);
    assert_eq!(
        send(
            &s,
            id,
            json!({"type":"opcua_read","node_id":"ns=2;s=missing"})
        )
        .await["success"],
        false
    );
    s.remove_client(id).await;
    drop(p.stdin.take());
    assert!(p.wait().await.unwrap().success());
}
#[tokio::test(flavor = "multi_thread")]
async fn remote_stop_updates_client_status_without_another_command() {
    let (mut p, a) = peer_server("opcua").await;
    let s = state();
    let id = client_in(&s, a.to_string(), "opcua", quiet(), json!({}))
        .await
        .unwrap();
    p.kill().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if matches!(
                s.get_client(id).await.unwrap().status,
                netget::state::ClientStatus::Disconnected
            ) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("remote close left UA client connected");
    s.remove_client(id).await;
}
