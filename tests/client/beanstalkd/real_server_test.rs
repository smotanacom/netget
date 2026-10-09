use super::common::*;
use crate::helpers::real_server::{InstallHint, RealServer};
use serde_json::json;
#[tokio::test]
async fn real_beanstalkd_job_lifecycle_and_tubes() -> crate::helpers::E2EResult<()> {
    let server = RealServer::builder(
        "beanstalkd",
        InstallHint {
            brew: "beanstalkd",
            apt: "beanstalkd",
        },
    )
    // Let the daemon reserve its own port, then read its successful bind from its
    // verbose log. A released probe port could belong to another test by the time
    // the daemon starts, making a TCP-only readiness check accept the wrong peer.
    .args(["-V", "-l", "127.0.0.1", "-p", "0"])
    .port_from_log(r"(?m)^bind [0-9]+ 127\.0\.0\.1:([0-9]+)$")
    .start()
    .await?;
    let state = state();
    let id = client(&state, server.addr(), json!([])).await;
    assert_eq!(
        request(&state, id, json!({"operation":"use","tube":"images"})).await["tube"],
        "images"
    );
    assert_eq!(
        request(&state, id, json!({"operation":"watch","tube":"images"})).await["count"],
        2
    );
    assert_eq!(
        request(&state, id, json!({"operation":"ignore","tube":"default"})).await["count"],
        1
    );
    assert_eq!(
        request(&state, id, json!({"operation":"list_tube_used"})).await["tube"],
        "images"
    );
    assert_eq!(
        request(&state, id, json!({"operation":"list_tubes_watched"})).await["data"],
        json!(["images"])
    );
    let body = "job ✓\r\nDELETED";
    let inserted = request(&state, id, json!({"operation":"put","body":body})).await;
    assert_eq!(inserted["status"], "INSERTED");
    let job = inserted["id"].as_u64().unwrap();
    let reserved = request(&state, id, json!({"operation":"reserve","timeout_secs":1})).await;
    assert_eq!(reserved["id"], job);
    assert_eq!(reserved["body"], body);
    assert_eq!(
        request(&state, id, json!({"operation":"touch","id":job})).await["status"],
        "TOUCHED"
    );
    assert_eq!(
        request(&state, id, json!({"operation":"release","id":job})).await["status"],
        "RELEASED"
    );
    assert_eq!(
        request(&state, id, json!({"operation":"peek","id":job})).await["body"],
        body
    );
    assert_eq!(
        request(&state, id, json!({"operation":"reserve_job","id":job})).await["status"],
        "RESERVED"
    );
    assert_eq!(
        request(&state, id, json!({"operation":"bury","id":job})).await["status"],
        "BURIED"
    );
    assert_eq!(
        request(&state, id, json!({"operation":"stats_job","id":job})).await["data"]["state"],
        "buried"
    );
    assert_eq!(
        request(&state, id, json!({"operation":"kick","count":1})).await["count"],
        1
    );
    assert_eq!(
        request(
            &state,
            id,
            json!({"operation":"stats_tube","tube":"images"})
        )
        .await["data"]["current-jobs-ready"],
        1
    );
    assert!(
        request(&state, id, json!({"operation":"stats"})).await["data"]["total-jobs"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(
        request(&state, id, json!({"operation":"list_tubes"})).await["data"]
            .as_array()
            .unwrap()
            .contains(&json!("images"))
    );
    assert_eq!(
        request(
            &state,
            id,
            json!({"operation":"pause_tube","tube":"images","delay":0})
        )
        .await["status"],
        "PAUSED"
    );
    assert_eq!(
        request(&state, id, json!({"operation":"delete","id":job})).await["status"],
        "DELETED"
    );
    assert_eq!(
        request(&state, id, json!({"operation":"peek","id":job})).await["status"],
        "NOT_FOUND"
    );
    assert_eq!(
        request(&state, id, json!({"operation":"reserve","timeout_secs":0})).await["status"],
        "TIMED_OUT"
    );
    state.remove_client(id).await;
    Ok(())
}
