//! NetGet's Thrift client against a thriftpy2 0.7.1 server (independent, unchanged) over framed
//! binary and buffered compact: results, a declared exception, a struct argument and a oneway call
//! the server prints, and an unknown method the server refuses. Fails, never skips.
use crate::helpers::thrift::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::json;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn client_calls_thriftpy2() {
    // The client's IDL declares one function the server's does not have.
    let client_idl = idl().replace(
        "oneway void ping(1: string note),",
        "oneway void ping(1: string note),\n  i32 nope(),",
    );
    for (transport, protocol) in [("framed", "binary"), ("buffered", "compact")] {
        let server = start_server(transport, protocol).await.unwrap();
        let state = state();
        let cid = client_in(
            &state,
            server.addr(),
            json!({"idl": client_idl, "protocol": protocol, "transport": transport}),
        )
        .await
        .unwrap();
        let owner = AccessLogOwner::Client(cid.as_u32());
        let connected = &logs(&state, owner, "thrift_connected", 1).await[0].request;
        assert_eq!(connected["service"], "Users");
        assert_eq!(connected["functions"].as_array().unwrap().len(), 6);
        for a in [
            json!({"type": "thrift_call", "method": "add", "args": {"a": 40, "b": 2}}),
            json!({"type": "thrift_call", "method": "get_user", "args": {"id": 7}}),
            json!({"type": "thrift_call", "method": "get_user", "args": {"id": 404}}),
            json!({"type": "thrift_call", "method": "find", "args": {"prefix": "Q", "roles": ["USER", "ADMIN"]}}),
            json!({"type": "thrift_call", "method": "touch", "args": {"u": {"id": 9, "name": "Grace", "role": "USER", "tags": ["navy"]}}}),
            json!({"type": "thrift_call", "method": "ping", "args": {"note": "hi"}}),
            json!({"type": "thrift_call", "method": "nope"}),
            json!({"type": "thrift_call", "method": "add", "args": {"a": 1, "b": 2}}),
        ] {
            let sent = state
                .send_to_client(cid, a.clone(), Duration::from_secs(20))
                .await
                .unwrap();
            assert!(
                matches!(sent, ClientSendOutcome::Sent { .. }),
                "{a}: {sent:?}"
            );
        }
        // Arguments the IDL rejects never reach the wire.
        let bad = state
            .send_to_client(
                cid,
                json!({"type": "thrift_call", "method": "touch", "args": {"u": {"name": "no id"}}}),
                Duration::from_secs(20),
            )
            .await;
        assert!(
            format!("{bad:?}").contains("id"),
            "{transport}/{protocol}: {bad:?}"
        );
        let r: Vec<_> = logs(&state, owner, "thrift_result", 7)
            .await
            .into_iter()
            .map(|e| e.request)
            .collect();
        assert_eq!(r[0]["result"], 42, "{transport}/{protocol}: {r:?}");
        assert_eq!(
            r[1]["result"],
            json!({"id": 7, "name": "Ada", "role": "ADMIN", "tags": ["x", "y"]})
        );
        assert_eq!(
            r[2]["exception"],
            json!({"name": "missing", "type": "NotFound", "value": {"message": "no user 404", "id": 404}})
        );
        assert_eq!(
            r[3]["result"],
            json!([{"id": 1, "name": "Q1", "role": "ADMIN", "tags": []}, {"id": 2, "name": "Q2", "role": "USER", "tags": []}])
        );
        assert_eq!(r[4]["result"], json!(null));
        assert_eq!(r[5]["application_error"]["type"], 1, "{:?}", r[5]);
        assert_eq!(r[6]["result"], 3);

        server
            .wait_for_log("\"served\": \"ping\"", Duration::from_secs(10))
            .await
            .unwrap();
        let printed = lines(&server.log());
        let touch = printed.iter().find(|l| l["served"] == "touch").unwrap();
        assert_eq!(
            touch["user"],
            json!({"id": 9, "name": "Grace", "role": 2, "tags": ["navy"]})
        );
        let ping = printed.iter().find(|l| l["served"] == "ping").unwrap();
        assert_eq!(ping["note"], "hi");
        let find = printed.iter().find(|l| l["served"] == "find").unwrap();
        assert_eq!(find["roles"], json!([1, 2]));
        state.remove_client(cid).await;
    }
}
