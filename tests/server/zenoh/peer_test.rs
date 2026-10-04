//! zenoh-pico 1.10.1 (C, independent of the Rust runtime NetGet uses, unchanged) clients
//! against NetGet's Zenoh router: a publication the handler echoes to a pico subscriber, queries
//! answered and refused by the handler, and a handler-issued get answered by a pico queryable;
//! the links appear as connections. Fails, never skips.
use crate::helpers::zenoh::*;
use netget::state::AccessLogOwner;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn zenoh_pico_through_netget() {
    let state = state();
    let (sid, addr) = server_in(
        &state,
        json!({"subscribe": ["demo/**"], "queryable": ["demo/q/**"]}),
    )
    .await;
    let owner = AccessLogOwner::Server(sid.as_u32());
    let ep = format!("tcp/{addr}");
    let base = ["-m", "client", "-e", ep.as_str()];

    // The subscriber stays until it has one echo; the publisher repeats once a second until then.
    let sub = tokio::spawn({
        let ep = ep.clone();
        async move {
            run_pico(
                "z_sub",
                &["-m", "client", "-e", &ep, "-k", "demo/echo/out", "-n", "1"],
            )
            .await
        }
    });
    let (ok, out) = run_pico(
        "z_pub",
        &[&base[..], &["-k", "demo/echo/in", "-v", "hello", "-n", "4"]].concat(),
    )
    .await;
    assert!(ok, "{out}");
    let (ok, out) = sub.await.unwrap();
    assert!(ok && out.contains("('demo/echo/out': 'echo: ["), "{out}");
    assert!(out.contains("] hello')"), "{out}");
    let sample = wait_for(&state, owner, "zenoh_sample", |e| {
        e["key"] == "demo/echo/in"
    })
    .await;
    assert_eq!(
        (sample["kind"].as_str(), sample["payload_encoding"].as_str()),
        (Some("put"), Some("utf8"))
    );

    let (ok, out) = run_pico("z_get", &[&base[..], &["-k", "demo/q/item"]].concat()).await;
    assert!(
        ok && out.contains("Received PUT ('demo/q/item': 'answer for demo/q/item')"),
        "{out}"
    );
    let (ok, out) = run_pico("z_get", &[&base[..], &["-k", "demo/q/fail"]].concat()).await;
    assert!(
        ok && out.contains("Received an error: no such thing"),
        "{out}"
    );

    // A pico queryable answers the get the handler issues when demo/ask is published.
    let queryable = tokio::spawn({
        let ep = ep.clone();
        async move {
            run_pico(
                "z_queryable",
                &[
                    "-m",
                    "client",
                    "-e",
                    &ep,
                    "-k",
                    "demo/pq",
                    "-v",
                    "pico-value",
                    "-n",
                    "1",
                ],
            )
            .await
        }
    });
    let (ok, out) = run_pico(
        "z_pub",
        &[&base[..], &["-k", "demo/ask", "-v", "go", "-n", "4"]].concat(),
    )
    .await;
    assert!(ok, "{out}");
    let result = wait_for(&state, owner, "zenoh_get_result", |e| {
        e["replies"].as_array().is_some_and(|r| !r.is_empty())
    })
    .await;
    assert_eq!(
        (
            result["replies"][0]["key"].as_str(),
            result["replies"][0]["payload"].as_str()
        ),
        (Some("demo/pq"), Some("pico-value")),
        "{result}"
    );
    let (ok, out) = queryable.await.unwrap();
    assert!(ok && out.contains("Received Query 'demo/pq'"), "{out}");

    let conns = state.get_server(sid).await.unwrap().connections;
    assert!(
        conns.values().any(|c| c.remote_addr.ip().is_loopback()),
        "{conns:?}"
    );
    state.remove_server(sid).await;
}
