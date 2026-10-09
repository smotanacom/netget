//! thriftpy2 0.7.1 (IDL-driven, C codecs) and Apache Thrift 0.25.0 (its own codecs, no generated
//! code), both independent and unchanged, call NetGet's Users service over every transport and
//! protocol pairing they share: results, a declared exception, an application error, a oneway
//! call, and an unknown method. Fails, never skips.
use crate::helpers::thrift::*;
use netget::state::AccessLogOwner;
use serde_json::json;

fn port(addr: std::net::SocketAddr) -> String {
    addr.port().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn thriftpy2_calls_netget() {
    let state = state();
    let (sid, addr) = server_in(&state, policy()).await;
    let idl = idl_path().display().to_string();
    for (transport, protocol) in [("framed", "binary"), ("buffered", "compact")] {
        let out = run_peer(&["thriftpy2-call", &idl, &port(addr), transport, protocol]).await;
        let by = |c: &str| {
            out.iter()
                .find(|l| l["call"] == c)
                .unwrap_or_else(|| panic!("{c} missing from {out:?}"))
                .clone()
        };
        assert_eq!(by("add")["result"], 42, "{transport}/{protocol}");
        assert_eq!(
            by("get_user")["result"],
            json!({"id": 7, "name": "Ada", "role": 1, "tags": ["x", "y"]})
        );
        let missing = by("get_user_missing");
        assert_eq!(
            (
                missing["exception"].as_str(),
                missing["message"].as_str(),
                missing["id"].as_i64()
            ),
            (Some("NotFound"), Some("no user 404"), Some(404))
        );
        assert_eq!(
            by("find")["result"],
            json!([{"id": 1, "name": "A1", "role": 1, "tags": []}, {"id": 2, "name": "A2", "role": 2, "tags": []}])
        );
        assert_eq!(by("touch")["result"], json!(null));
        let refused = by("add_refused");
        assert_eq!(refused["app_error"], 6, "{refused}");
        assert!(refused["message"].as_str().unwrap().contains("too large"));
        assert_eq!(by("add_after")["result"], 3);
    }
    let owner = AccessLogOwner::Server(sid.as_u32());
    let calls = logs(&state, owner, "thrift_call", 16).await;
    let touch = calls
        .iter()
        .find(|c| c.request["method"] == "touch")
        .unwrap();
    assert_eq!(
        touch.request["args"]["u"],
        json!({"id": 9, "name": "Grace", "role": "USER", "tags": ["navy"]})
    );
    let ping = calls
        .iter()
        .find(|c| c.request["method"] == "ping")
        .unwrap();
    assert_eq!(
        (
            ping.request["oneway"].as_bool(),
            ping.request["args"]["note"].as_str()
        ),
        (Some(true), Some("hello"))
    );
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn apache_thrift_calls_netget() {
    let state = state();
    let (sid, addr) = server_in(&state, policy()).await;
    for (transport, protocol) in [("buffered", "binary"), ("framed", "compact")] {
        let out = run_peer(&["apache-call", &port(addr), transport, protocol]).await;
        let calls: Vec<_> = out.iter().map(|l| l["call"].as_str().unwrap()).collect();
        assert_eq!(
            calls,
            [
                "add",
                "get_user",
                "get_user",
                "find",
                "delete_everything",
                "add"
            ],
            "{transport}/{protocol}: {out:?}"
        );
        // Results arrive as the result struct: field 0 for a return, the throws id otherwise.
        assert_eq!(out[0]["result"], json!({"0": 42}));
        assert_eq!(
            out[1]["result"],
            json!({"0": {"1": 7, "2": "Ada", "3": 1, "4": ["x", "y"]}})
        );
        assert_eq!(
            out[2]["result"],
            json!({"1": {"1": "no user 404", "2": 404}})
        );
        assert_eq!(
            out[3]["result"],
            json!({"0": [{"1": 1, "2": "B1", "3": 2, "4": []}]})
        );
        assert_eq!(out[4]["app_error"], 1, "{:?}", out[4]);
        assert!(out[4]["message"]
            .as_str()
            .unwrap()
            .contains("delete_everything"));
        assert_eq!(out[5]["result"], json!({"0": 3}));
    }
    let owner = AccessLogOwner::Server(sid.as_u32());
    let calls = logs(&state, owner, "thrift_call", 10).await;
    assert!(
        calls
            .iter()
            .all(|c| c.request["method"] != "delete_everything"),
        "an unknown method reached the handler"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|c| c.request["method"] == "ping" && c.request["args"]["note"] == "apache")
            .count(),
        2
    );
    state.remove_server(sid).await;
}
