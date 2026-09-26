//! Docker Engine API routing and rendering, without a socket.
//!
//! `real_client_test.rs` shows the Docker CLI decodes what these render; this file pins the
//! routing table, the defaults that keep the Go decoder happy, and every refusal.

#![cfg(feature = "docker")]

use netget::server::docker::api::{
    self, parse_version, render_container, render_container_list, render_images, render_info,
    render_networks, render_version, render_volumes, resolve, split_version, EngineIdentity, Route,
};
use serde_json::json;

fn identity() -> EngineIdentity {
    EngineIdentity {
        engine_version: "27.5.1".into(),
        api_version: "1.47".into(),
    }
}

#[test]
fn version_prefixes_are_split_and_validated() {
    assert_eq!(
        split_version("/v1.45/containers/json"),
        (Some("1.45".into()), "/containers/json".into())
    );
    assert_eq!(split_version("/_ping"), (None, "/_ping".into()));
    // A first segment that merely starts with v is a path, not a version.
    assert_eq!(split_version("/volumes"), (None, "/volumes".into()));
    assert_eq!(split_version("/vX.1/info"), (None, "/vX.1/info".into()));
    assert!(
        parse_version("1.47") > parse_version("1.9"),
        "compared numerically"
    );
    assert_eq!(parse_version("1."), None);
}

#[test]
fn the_routing_table() {
    assert_eq!(resolve("HEAD", "/_ping"), Route::Ping);
    assert_eq!(resolve("GET", "/_ping"), Route::Ping);
    assert_eq!(resolve("GET", "/version"), Route::Version);
    assert_eq!(resolve("GET", "/info"), Route::Info);
    assert_eq!(resolve("GET", "/containers/json"), Route::ContainerList);
    assert_eq!(
        resolve("GET", "/containers/web/json"),
        Route::ContainerInspect("web".into())
    );
    assert_eq!(resolve("GET", "/images/json"), Route::ImageList);
    assert_eq!(resolve("GET", "/networks"), Route::NetworkList);
    assert_eq!(resolve("GET", "/volumes"), Route::VolumeList);
    for (m, p) in [
        ("POST", "/containers/create"),
        ("POST", "/containers/web/start"),
        ("POST", "/containers/web/exec"),
        ("POST", "/images/create"),
        ("DELETE", "/containers/web"),
        ("POST", "/_ping"),
    ] {
        assert_eq!(resolve(m, p), Route::Mutating, "{m} {p}");
    }
    assert_eq!(resolve("GET", "/containers/web/logs"), Route::NotFound);
    assert_eq!(resolve("GET", "/images/nginx/json"), Route::NotFound);
    assert_eq!(
        Route::ContainerList.answering_action(),
        Some("send_docker_containers")
    );
    assert_eq!(Route::Ping.resource(), None);
}

#[test]
fn a_minimal_container_gets_every_field_the_decoder_needs() {
    let list = render_container_list(&json!([{"names": ["web"], "image": "nginx"}])).unwrap();
    let c = &list[0];
    assert_eq!(
        c["Names"],
        json!(["/web"]),
        "names are slash-prefixed like the daemon's"
    );
    assert_eq!(c["State"], "running");
    assert_eq!(c["Status"], "Up");
    assert_eq!(
        c["Id"].as_str().unwrap().len(),
        64,
        "an ID is derived from the name"
    );
    assert!(c["ImageID"].as_str().unwrap().starts_with("sha256:"));
    assert!(c["Created"].is_i64());
    assert_eq!(c["Ports"], json!([]));
    assert_eq!(c["HostConfig"]["NetworkMode"], "bridge");
    // The same name derives the same ID in inspect, so `ps` and `inspect` agree.
    let one = render_container(&json!({"names": ["web"], "image": "nginx"})).unwrap();
    assert_eq!(one["Id"], c["Id"]);
    assert_eq!(one["State"]["Running"], true);
    assert_eq!(one["Name"], "/web");
}

#[test]
fn ports_status_and_timestamps_render_as_docker_writes_them() {
    let c = render_container_list(&json!([{
        "names": ["db"], "image": "postgres:16", "state": "exited", "exit_code": 3,
        "created": "2026-09-01T10:00:00Z",
        "ports": [{"private_port": 5432, "public_port": 15432, "type": "tcp"},
                  {"private_port": 53, "type": "udp"}]
    }]))
    .unwrap();
    assert_eq!(c[0]["Status"], "Exited (3)");
    assert_eq!(c[0]["Created"], 1788256800);
    assert_eq!(
        c[0]["Ports"],
        json!([
            {"PrivatePort": 5432, "Type": "tcp", "PublicPort": 15432, "IP": "0.0.0.0"},
            {"PrivatePort": 53, "Type": "udp"}
        ])
    );
    let one = render_container(&json!({
        "names": ["db"], "image": "postgres:16", "state": "exited",
        "ports": [{"private_port": 5432, "public_port": 15432}]
    }))
    .unwrap();
    assert_eq!(
        one["NetworkSettings"]["Ports"]["5432/tcp"][0]["HostPort"],
        "15432"
    );
    assert_eq!(one["State"]["Running"], false);
}

#[test]
fn every_malformed_answer_is_refused_with_a_reason() {
    let cases = [
        (json!([{"image": "nginx"}]), "needs a name"),
        (json!([{"names": ["-bad"], "image": "nginx"}]), "must match"),
        (json!([{"names": ["web"]}]), "needs an image"),
        (
            json!([{"names": ["web"], "image": "nginx", "state": "sleeping"}]),
            "is not one of",
        ),
        (
            json!([{"names": ["web"], "image": "nginx", "ports": [{"private_port": 70000}]}]),
            "is not a port",
        ),
        (
            json!([{"names": ["web"], "image": "nginx", "ports": [{"private_port": 80, "type": "icmp"}]}]),
            "tcp, udp or sctp",
        ),
        (
            json!([{"names": ["web"], "image": "nginx", "id": "not hex!"}]),
            "letters and digits",
        ),
        (
            json!([{"names": ["web"], "image": "nginx", "created": "yesterday"}]),
            "RFC 3339",
        ),
        (json!({"names": ["web"]}), "must be an array"),
    ];
    for (value, needle) in cases {
        let reason = render_container_list(&value).expect_err(&value.to_string());
        assert!(
            reason.contains(needle),
            "{value}: expected {needle:?} in {reason}"
        );
    }
    assert!(render_images(&json!([{"repo_tags": ["has space:1"]}])).is_err());
    assert!(render_networks(&json!([{"driver": "bridge"}])).is_err());
    assert!(render_volumes(&json!([{"name": "/abs"}])).is_err());
    assert!(render_version(&json!({"api_version": "latest"}), &identity()).is_err());
    assert!(render_info(&json!({"images": -1}), &identity()).is_err());
}

#[test]
fn version_and_info_default_everything_the_model_left_out() {
    let v = render_version(&json!({}), &identity()).unwrap();
    assert_eq!(v["Version"], "27.5.1");
    assert_eq!(v["ApiVersion"], "1.47");
    assert_eq!(v["MinAPIVersion"], api::MIN_API_VERSION);
    assert_eq!(v["Components"][0]["Name"], "Engine");
    let i = render_info(
        &json!({"containers_running": 2, "containers_stopped": 1}),
        &identity(),
    )
    .unwrap();
    assert_eq!(
        i["Containers"], 3,
        "the total defaults to the sum of the states"
    );
    assert_eq!(i["ServerVersion"], "27.5.1");
    assert_eq!(i["Swarm"]["LocalNodeState"], "inactive");
    let images = render_images(&json!([{"repo_tags": ["nginx:1.27"]}])).unwrap();
    assert_eq!(images[0]["SharedSize"], -1);
    assert!(images[0]["Id"].as_str().unwrap().starts_with("sha256:"));
    let vols = render_volumes(&json!([])).unwrap();
    assert_eq!(vols, json!({"Volumes": [], "Warnings": []}));
}
