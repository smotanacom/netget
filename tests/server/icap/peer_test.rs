//! c-icap 0.6.5's `c-icap-client` — an independent ICAP implementation, unchanged — against
//! NetGet's server: OPTIONS, a clean RESPMOD, a blocked RESPMOD, a redacted REQMOD and a body
//! larger than the advertised preview (100 Continue). Fails, never skips, when absent.
use crate::helpers::icap::*;
use netget::state::AccessLogOwner;
use serde_json::json;
use std::time::Duration;

async fn client(port: u16, args: &[&str]) -> String {
    let out = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new(c_icap_prefix().join("bin/c-icap-client"))
            .args(["-i", "127.0.0.1", "-p", &port.to_string(), "-s", "filter"])
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("c-icap-client timed out")
    .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "c-icap-client {args:?} failed: {text}"
    );
    text
}

#[tokio::test(flavor = "multi_thread")]
async fn c_icap_client_gets_options_passes_blocks_redacts_and_continues_a_preview() {
    let state = state();
    let (sid, addr) = server_in(
        &state,
        filter_policy(),
        json!({"services":[{"name":"filter","methods":["REQMOD","RESPMOD"]}],"preview_bytes":512}),
    )
    .await;
    let port = addr.port();
    let dir = tempfile::tempdir().unwrap();
    let file = |name: &str, body: &str| {
        let p = dir.path().join(name);
        std::fs::write(&p, body).unwrap();
        p.display().to_string()
    };
    let out_path = |name: &str| dir.path().join(name).display().to_string();

    let options = client(port, &[]).await;
    assert!(
        options.contains("Methods: REQMOD, RESPMOD")
            && options.contains("Preview: 512")
            && options.contains("ISTag: \"NetGet-1\""),
        "{options}"
    );

    let clean = file("clean.txt", "plain safe content\n");
    let o = out_path("clean.out");
    let text = client(
        port,
        &[
            "-f",
            &clean,
            "-resp",
            "http://example.com/clean",
            "-o",
            &o,
            "-v",
        ],
    )
    .await;
    assert!(
        text.contains("204") || std::fs::read_to_string(&o).unwrap() == "plain safe content\n",
        "{text}"
    );

    let virus = file(
        "virus.txt",
        "X5O!P%@AP[4\\PZX54(P^)7CC)7}$EICAR-STANDARD-ANTIVIRUS-TEST-FILE!$H+H*\n",
    );
    let o = out_path("virus.out");
    let text = client(
        port,
        &[
            "-f",
            &virus,
            "-resp",
            "http://example.com/virus",
            "-o",
            &o,
            "-v",
            "-no204",
        ],
    )
    .await;
    assert!(text.contains("HTTP/1.1 403 Forbidden"), "{text}");
    assert_eq!(std::fs::read_to_string(&o).unwrap(), "Blocked by NetGet");

    let upload = file("upload.txt", "the secret plan\n");
    let o = out_path("upload.out");
    let text = client(
        port,
        &[
            "-f",
            &upload,
            "-req",
            "http://example.com/upload",
            "-method",
            "POST",
            "-o",
            &o,
            "-v",
            "-no204",
        ],
    )
    .await;
    assert!(text.contains("X-Redacted: 1"), "{text}");
    assert_eq!(std::fs::read_to_string(&o).unwrap(), "[redacted]");

    let large = file("large.txt", &"abcdefghij".repeat(300));
    let o = out_path("large.out");
    let large_text = client(
        port,
        &[
            "-f",
            &large,
            "-resp",
            "http://example.com/large",
            "-o",
            &o,
            "-w",
            "512",
            "-no204",
            "-v",
        ],
    )
    .await;
    let got = std::fs::read_to_string(&o).unwrap_or_default().len();
    let seen = state
        .list_access_logs_for(Some(AccessLogOwner::Server(sid.as_u32())), None)
        .await;
    let last = seen
        .iter()
        .filter(|r| r.event_type == "icap_request")
        .next_back()
        .map(|r| r.request.clone());
    assert_eq!(got, 3000, "the unmodified 3000-byte body came back whole after 100 Continue; client said: {large_text}; handler saw: {last:?}");

    let rows = logs(
        &state,
        AccessLogOwner::Server(sid.as_u32()),
        "icap_request",
        4,
    )
    .await;
    let large_row = rows
        .iter()
        .find(|r| r.request["body_bytes"] == 3000)
        .expect("the full previewed body reached the handler");
    assert_eq!(large_row.request["method"], "RESPMOD");
    let upload_row = rows
        .iter()
        .find(|r| r.request["method"] == "REQMOD")
        .unwrap();
    assert_eq!(upload_row.request["http_request"]["method"], "POST");
    assert_eq!(upload_row.request["body_text"], "the secret plan\n");
    state.remove_server(sid).await;
}
