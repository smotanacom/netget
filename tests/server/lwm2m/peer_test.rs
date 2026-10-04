//! The Eclipse Leshan 2.0.0-M15 client demo (Java/Californium, independent, unchanged) against
//! NetGet's LwM2M server: registration with its objects, reads in text and SenML JSON, a write
//! read back, an execute, discovery, a 4.04 for an object it lacks, an observation with
//! notifications, and deregistration on shutdown. Fails, never skips.
use crate::helpers::lwm2m::*;
use netget::state::AccessLogOwner;
use serde_json::Value;

#[tokio::test(flavor = "multi_thread")]
async fn leshan_client_against_netget() {
    let state = state();
    let (sid, addr) = server_in(&state).await;
    let owner = AccessLogOwner::Server(sid.as_u32());
    let device = start_leshan_client(addr, "leshan-dev").await.unwrap();

    let reg = wait_for(&state, owner, "lwm2m_register", |e| {
        e["endpoint"] == "leshan-dev"
    })
    .await;
    let objects: Vec<&str> = reg["objects"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|o| o["path"].as_str())
        .collect();
    for o in ["/3/0", "/6/0", "/3303/0"] {
        assert!(
            objects.iter().any(|p| p.starts_with(o)),
            "{o} not registered: {reg}"
        );
    }
    let response = |op: &'static str, path: &'static str| {
        let state = &state;
        async move {
            wait_for(state, owner, "lwm2m_response", |e| {
                e["operation"] == op && e["path"] == path
            })
            .await
        }
    };
    let value = |r: &Value, path: &str| {
        r["values"]
            .as_array()
            .and_then(|v| v.iter().find(|x| x["path"] == path))
            .map(|x| x["value"].clone())
    };
    let manufacturer = response("read", "/3/0/0").await;
    assert_eq!(
        (
            manufacturer["code"].as_str(),
            value(&manufacturer, "/3/0/0")
        ),
        (Some("2.05"), Some(Value::from("Leshan Demo Device"))),
        "{manufacturer}"
    );
    let temperature = response("read", "/3303/0").await;
    assert!(
        value(&temperature, "/3303/0/5700").is_some_and(|t| t.is_number()),
        "{temperature}"
    );
    assert_eq!(response("write", "/3/0/14").await["code"], "2.04");
    let offset = response("read", "/3/0/14").await;
    assert_eq!(
        value(&offset, "/3/0/14"),
        Some(Value::from("+02")),
        "{offset}"
    );
    assert_eq!(response("execute", "/3/0/12").await["code"], "2.04");
    let discover = response("discover", "/3/0").await;
    assert!(
        discover["links"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["path"] == "/3/0/0"),
        "{discover}"
    );
    assert_eq!(response("read", "/42/0").await["code"], "4.04");
    let observe = response("observe", "/3303/0/5700").await;
    assert_eq!(observe["code"], "2.05", "{observe}");
    let note = wait_for(&state, owner, "lwm2m_notification", |e| {
        e["path"] == "/3303/0/5700"
    })
    .await;
    assert!(
        value(&note, "/3303/0/5700").is_some_and(|t| t.is_number()),
        "{note}"
    );

    drop(device);
    let gone = wait_for(&state, owner, "lwm2m_deregister", |e| {
        e["endpoint"] == "leshan-dev"
    })
    .await;
    assert_eq!(gone["reason"], "deregistered");
    state.remove_server(sid).await;
}
