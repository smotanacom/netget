//! The inetd clients against xinetd, unchanged: its built-in echo, discard, daytime, chargen
//! and time services on TCP and UDP, and an external QOTD program on TCP (xinetd has no
//! built-in QOTD). Fails rather than skips without xinetd (`apt-get install xinetd`).
use super::pair_test::{client, wait_log};
use crate::helpers::real_server::{InstallHint, RealServer};
use serde_json::{json, Value};

const HINT: InstallHint = InstallHint {
    brew: "xinetd",
    apt: "xinetd",
};

fn service(name: &str, id: &str, dgram: bool, port: &str) -> String {
    let (socket, protocol, wait) = if dgram {
        ("dgram", "udp", "yes")
    } else {
        ("stream", "tcp", "no")
    };
    format!(
        "service {name}\n{{\n    type = INTERNAL UNLISTED\n    id = {id}\n    socket_type = {socket}\n    protocol = {protocol}\n    port = {port}\n    wait = {wait}\n    bind = 127.0.0.1\n}}\n"
    )
}

fn config() -> String {
    let mut conf = String::from("defaults\n{\n    instances = 50\n}\n");
    let mut port = 0;
    for name in ["echo", "discard", "daytime", "chargen", "time"] {
        for dgram in [false, true] {
            let placeholder = if port == 0 {
                "{port}".to_string()
            } else {
                format!("{{port{port}}}")
            };
            conf.push_str(&service(
                name,
                &format!("{name}-{}", if dgram { "dgram" } else { "stream" }),
                dgram,
                &placeholder,
            ));
            port += 1;
        }
    }
    // QOTD has no built-in: xinetd runs a program per connection, as inetd sites did.
    let user = if unsafe { libc::geteuid() } == 0 {
        "    user = nobody\n"
    } else {
        ""
    };
    conf.push_str(&format!(
        "service qotd\n{{\n    type = UNLISTED\n    socket_type = stream\n    protocol = tcp\n    port = {{port10}}\n    wait = no\n{user}    server = /bin/echo\n    server_args = Independent quote of the day\n    bind = 127.0.0.1\n}}\n"
    ));
    conf
}

#[tokio::test]
async fn netget_clients_read_xinetd_builtin_services() {
    let xinetd = RealServer::builder("xinetd", HINT)
        .config_file("xinetd.conf", &config())
        .extra_ports(10)
        .args([
            "-dontfork",
            "-stayalive",
            "-f",
            "{dir}/xinetd.conf",
            "-pidfile",
            "{dir}/xinetd.pid",
        ])
        .start()
        .await
        .unwrap_or_else(|e| panic!("xinetd: {e}"));
    let conf = std::fs::read_to_string(xinetd.dir().join("xinetd.conf")).unwrap();
    let ports: Vec<u16> = conf
        .lines()
        .filter_map(|l| l.trim().strip_prefix("port = "))
        .map(|p| p.parse().unwrap())
        .collect();
    assert_eq!(ports.len(), 11, "{conf}");
    let year = chrono::Utc::now().format("%Y").to_string();
    let year_needle = year.as_str();
    let cases: [(&str, &str, Value, &[&str]); 5] = [
        (
            "echo",
            "echo_ready",
            json!({"type":"echo_send","data":"through xinetd"}),
            &[r#""matches":true"#, "through xinetd"],
        ),
        (
            "discard",
            "discard_ready",
            json!({"type":"discard_send","data":"lost"}),
            &[r#""bytes":4"#],
        ),
        (
            "daytime",
            "daytime_ready",
            json!({"type":"daytime_query"}),
            &[year_needle],
        ),
        (
            "chargen",
            "chargen_ready",
            json!({"type":"chargen_query","bytes":500}),
            &[r#""conforms":true"#],
        ),
        (
            "time",
            "time_ready",
            json!({"type":"time_query"}),
            &[year_needle, "seconds_since_1900"],
        ),
    ];
    for (index, (protocol, ready, query, needles)) in cases.into_iter().enumerate() {
        for (offset, transport) in ["tcp", "udp"].into_iter().enumerate() {
            let port = ports[index * 2 + offset];
            let (state, id) = client(
                protocol,
                format!("127.0.0.1:{port}"),
                transport,
                ready,
                query.clone(),
            )
            .await;
            let mut wanted = needles.to_vec();
            let transport_needle = format!(r#""transport":"{transport}""#);
            wanted.push(&transport_needle);
            let event = wait_log(&state, id, &wanted).await;
            if protocol == "time" {
                // xinetd reports its own clock; it must decode to within a minute of ours.
                let value: Value = serde_json::from_str(&event).unwrap();
                let unix = find_number(&value, "unix_seconds").expect("unix_seconds in the event");
                assert!(
                    (unix - chrono::Utc::now().timestamp()).abs() < 60,
                    "{event}"
                );
            }
            state.remove_client(id).await;
        }
    }
    let (state, id) = client(
        "qotd",
        format!("127.0.0.1:{}", ports[10]),
        "tcp",
        "qotd_ready",
        json!({"type":"qotd_query"}),
    )
    .await;
    wait_log(&state, id, &[r#""quote":"Independent quote of the day""#]).await;
    state.remove_client(id).await;
    drop(xinetd);
}

fn find_number(value: &Value, key: &str) -> Option<i64> {
    match value {
        Value::Object(map) => map
            .get(key)
            .and_then(Value::as_i64)
            .or_else(|| map.values().find_map(|v| find_number(v, key))),
        Value::Array(items) => items.iter().find_map(|v| find_number(v, key)),
        _ => None,
    }
}
