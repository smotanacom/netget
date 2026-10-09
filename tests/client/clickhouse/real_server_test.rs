//! NetGet's ClickHouse client against the official ClickHouse 24.8 server, unchanged
//! (`install_peers.py` downloads it by SHA-256; NETGET_CLICKHOUSE_BIN names it), started in a
//! temporary directory on a probed port with a minimal config: create a MergeTree table,
//! insert typed rows, read them back with an aggregate, raise an exception, refuse a login.
use super::session_test::{client, connected, run};
use crate::helpers::real_server::{InstallHint, RealServer};
use serde_json::json;
use std::time::Duration;

const CONFIG: &str = r#"<clickhouse>
  <logger><console>1</console><level>information</level></logger>
  <path>{dir}/data/</path>
  <tmp_path>{dir}/tmp/</tmp_path>
  <user_files_path>{dir}/user_files/</user_files_path>
  <format_schema_path>{dir}/format_schemas/</format_schema_path>
  <listen_host>127.0.0.1</listen_host>
  <tcp_port>{port}</tcp_port>
  <mark_cache_size>268435456</mark_cache_size>
  <users><default><password>s3cret</password><networks><ip>127.0.0.1</ip></networks><profile>default</profile><quota>default</quota></default></users>
  <profiles><default/></profiles>
  <quotas><default/></quotas>
</clickhouse>
"#;

#[tokio::test]
async fn netget_against_official_clickhouse_server() {
    let binary = std::env::var("NETGET_CLICKHOUSE_BIN").expect("NETGET_CLICKHOUSE_BIN is required: run tests/client/clickhouse/install_peers.py <root> and export what it prints");
    let server = RealServer::builder(
        &binary,
        InstallHint {
            brew: "clickhouse",
            apt: "clickhouse-server (or tests/client/clickhouse/install_peers.py)",
        },
    )
    .config_file("config.xml", CONFIG)
    .args(["server", "--config-file={dir}/config.xml"])
    .ready_when_log_matches("Ready for connections")
    .startup_timeout(Duration::from_secs(60))
    .start()
    .await
    .expect("start the ClickHouse server");
    let refused = client(server.addr(), json!({"user":"default","password":"wrong"})).await;
    let error = format!(
        "{:#}",
        refused.err().expect("a wrong password fails creation")
    );
    assert!(error.contains("516"), "{error}");
    let (state, id) = client(server.addr(), json!({"user":"default","password":"s3cret"}))
        .await
        .unwrap();
    let hello = connected(&state, id).await;
    assert!(
        hello.contains(r#""version":"24.8.14""#) && hello.contains(r#""revision":54429"#),
        "{hello}"
    );
    let r = run(&state, id, json!({"type":"clickhouse_query","query":"CREATE TABLE events (id UInt32, name String, score Nullable(Float64), day Date, at DateTime, ok Bool, delta Int64) ENGINE = MergeTree ORDER BY id"})).await;
    assert_eq!(r["ok"], true, "{r}");
    let r = run(&state, id, json!({"type":"clickhouse_insert","query":"INSERT INTO events (id, name, score, day, at, ok, delta) VALUES","rows":[
        [1, "alpha", 1.5, "2024-01-02", "2024-01-02 03:04:05", true, -7],
        [2, "beta", null, "2024-12-31", "2024-12-31 23:59:59", false, 9007199254740993i64]
    ]})).await;
    assert_eq!(
        (r["ok"].clone(), r["rows_written"].clone()),
        (json!(true), json!(2)),
        "{r}"
    );
    let r = run(
        &state,
        id,
        json!({"type":"clickhouse_query","query":"SELECT * FROM events ORDER BY id"}),
    )
    .await;
    assert_eq!(
        r["rows"],
        json!([
            [
                1,
                "alpha",
                1.5,
                "2024-01-02",
                "2024-01-02 03:04:05",
                true,
                -7
            ],
            [
                2,
                "beta",
                null,
                "2024-12-31",
                "2024-12-31 23:59:59",
                false,
                9007199254740993i64
            ]
        ]),
        "{r}"
    );
    let r = run(&state, id, json!({"type":"clickhouse_query","query":"SELECT count() AS n, sum(delta) AS total FROM events"})).await;
    assert_eq!(
        (r["rows"].clone(), r["columns"].clone()),
        (
            json!([[2, 9007199254740986i64]]),
            json!([{"name":"n","type":"UInt64"},{"name":"total","type":"Int64"}])
        ),
        "{r}"
    );
    let r = run(
        &state,
        id,
        json!({"type":"clickhouse_query","query":"SELECT * FROM nowhere"}),
    )
    .await;
    assert_eq!(
        (r["ok"].clone(), r["exception"]["code"].clone()),
        (json!(false), json!(60)),
        "{r}"
    );
    state.remove_client(id).await;
}
