//! GoBGP 4.9.0 (Go, independent, unchanged) as the monitored router: it peers with a second
//! GoBGP and exports BMP to NetGet's collector — Initiation, Peer Up with both OPENs, Route
//! Monitoring for announcements (communities, MED) and a withdrawal, Statistics, and Peer Down
//! when the peer dies. Fails, never skips.
use crate::helpers::bmp::*;
use netget::state::AccessLogOwner;
use serde_json::{json, Value};
use std::time::Duration;

async fn route_monitoring(
    state: &netget::state::app_state::AppState,
    owner: AccessLogOwner,
    want: impl Fn(&Value) -> bool,
) -> Value {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(e) = state
                .list_access_logs_for(Some(owner), None)
                .await
                .into_iter()
                .find(|e| e.event_type == "bmp_route_monitoring" && want(&e.request["update"]))
            {
                break e.request;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("GoBGP did not report the expected route monitoring message")
}

#[tokio::test(flavor = "multi_thread")]
async fn gobgp_exports_to_netget() {
    let state = state();
    let (sid, collector) = server_in(&state, policy()).await;
    let owner = AccessLogOwner::Server(sid.as_u32());
    let source = start_route_source().await.unwrap();
    let bgp_port = source.extra_ports[0];
    let exporter = start_exporter(bgp_port, collector).await.unwrap();

    let init = &logs(&state, owner, "bmp_initiation", 1, Duration::from_secs(30)).await[0].request;
    assert_eq!(init["sys_name"], "GoBGP", "{init}");
    let up = &logs(&state, owner, "bmp_peer_up", 1, Duration::from_secs(60)).await[0].request;
    assert_eq!(
        (
            up["peer"]["type"].as_str(),
            up["peer"]["address"].as_str(),
            up["peer"]["asn"].as_u64(),
            up["peer"]["bgp_id"].as_str()
        ),
        (
            Some("global"),
            Some("127.0.0.1"),
            Some(65002),
            Some("10.0.0.2")
        ),
        "{up}"
    );
    assert_eq!(up["remote_port"], bgp_port);
    assert_eq!(
        (
            up["sent_open"]["asn"].as_u64(),
            up["sent_open"]["bgp_id"].as_str()
        ),
        (Some(65001), Some("10.0.0.1"))
    );
    assert_eq!(up["received_open"]["asn"], 65002);
    assert_eq!(up["router"], "GoBGP");

    gobgp(
        &source.addr(),
        &[
            "global",
            "rib",
            "add",
            "10.9.0.0/24",
            "origin",
            "igp",
            "community",
            "65002:7",
            "med",
            "50",
        ],
    )
    .await;
    gobgp(&source.addr(), &["global", "rib", "add", "10.9.1.0/24"]).await;
    let has = |prefix: &'static str, key: &'static str| {
        move |u: &Value| {
            u[key]
                .as_array()
                .is_some_and(|a| a.iter().any(|p| p == prefix))
        }
    };
    let first = route_monitoring(&state, owner, has("10.9.0.0/24", "nlri")).await;
    let u = &first["update"];
    assert_eq!(
        (
            u["as_path"].clone(),
            u["next_hop"].as_str(),
            u["origin"].as_str()
        ),
        (json!([65002]), Some("127.0.0.1"), Some("IGP")),
        "{first}"
    );
    let attrs = u["path_attributes"].as_array().unwrap();
    assert!(
        attrs.iter().any(|a| a["communities"] == json!(["65002:7"])),
        "{u}"
    );
    assert!(attrs.iter().any(|a| a["med"] == 50), "{u}");
    assert_eq!(first["peer"]["asn"], 65002);
    route_monitoring(&state, owner, has("10.9.1.0/24", "nlri")).await;
    gobgp(&source.addr(), &["global", "rib", "del", "10.9.1.0/24"]).await;
    route_monitoring(&state, owner, has("10.9.1.0/24", "withdrawn_routes")).await;

    // GoBGP reports statistics every 15 s at the earliest.
    let stats = &logs(&state, owner, "bmp_statistics", 1, Duration::from_secs(40)).await[0].request;
    let counters = stats["counters"].as_array().unwrap();
    let adj = counters
        .iter()
        .find(|c| c["type"] == "adj_rib_in_routes")
        .unwrap_or_else(|| panic!("{stats}"));
    assert_eq!(adj["value"], 1, "{stats}");

    drop(source);
    let down = &logs(&state, owner, "bmp_peer_down", 1, Duration::from_secs(60)).await[0].request;
    assert_eq!(down["peer"]["asn"], 65002, "{down}");
    assert!(
        [
            "remote_no_data",
            "local_no_notification",
            "remote_notification",
            "local_notification"
        ]
        .contains(&down["reason"].as_str().unwrap()),
        "{down}"
    );
    drop(exporter);
    state.remove_server(sid).await;
}
