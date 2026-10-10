//! The X11 client's refusals and bounds, against an X server written in this file that gets one
//! thing wrong on purpose (a refused setup, a reply announcing more than the client accepts),
//! and the follow-up depth bound against Xvfb. No LLM calls.
use super::real_server_test::{client, wait_match, xvfb};
use netget::client::x11::wire::MAX_REPLY_BYTES;
use netget::state::{AccessLogOwner, ClientStatus};
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A successful setup: one 640x480 screen with root 0x100, vendor "fake".
fn setup_ok() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend(1u32.to_le_bytes()); // release
    body.extend(0x0020_0000u32.to_le_bytes()); // resource-id-base
    body.extend(0x001F_FFFFu32.to_le_bytes()); // resource-id-mask
    body.extend(0u32.to_le_bytes()); // motion buffer
    body.extend(4u16.to_le_bytes()); // vendor length
    body.extend(u16::MAX.to_le_bytes()); // maximum request length
    body.extend([1, 0, 0, 0, 32, 32, 8, 255, 0, 0, 0, 0]); // 1 screen, 0 formats, ...
    body.extend(b"fake");
    for v in [0x100u32, 0x20, 0xFF_FFFF, 0, 0] {
        body.extend(v.to_le_bytes()); // root, colormap, white, black, input masks
    }
    for v in [640u16, 480, 169, 127, 1, 1] {
        body.extend(v.to_le_bytes()); // size, size in mm, min/max installed maps
    }
    body.extend(0x21u32.to_le_bytes()); // root visual
    body.extend([0, 0, 24, 0]); // backing stores, save unders, root depth, 0 depths
    let mut out = vec![1, 0];
    out.extend(11u16.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend(((body.len() / 4) as u16).to_le_bytes());
    out.extend(body);
    out
}

async fn read_setup_request(s: &mut TcpStream) {
    let mut head = [0u8; 12];
    s.read_exact(&mut head).await.unwrap();
    assert_eq!(head[0], b'l', "NetGet asks for little-endian");
    let pad = |n: usize| n.div_ceil(4) * 4;
    let rest = pad(u16::from_le_bytes([head[6], head[7]]) as usize)
        + pad(u16::from_le_bytes([head[8], head[9]]) as usize);
    let mut skip = vec![0u8; rest];
    s.read_exact(&mut skip).await.unwrap();
}

#[tokio::test]
async fn refused_setup_reports_the_servers_reason() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        read_setup_request(&mut s).await;
        let reason = b"No protocol specified";
        let padded = reason.len().div_ceil(4) * 4;
        let mut out = vec![0, reason.len() as u8];
        out.extend(11u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(((padded / 4) as u16).to_le_bytes());
        out.extend(reason);
        out.resize(8 + padded, 0);
        s.write_all(&out).await.unwrap();
    });
    let e = client(addr.to_string(), json!({}), vec![])
        .await
        .err()
        .expect("a refused setup connected");
    assert!(format!("{e:#}").contains("No protocol specified"), "{e:#}");
}

/// Serve setup, then answer the client's first request (ListExtensions) with a reply whose
/// length field announces `total` bytes, sending them all.
async fn oversized(total: usize) -> (netget::state::app_state::AppState, netget::state::ClientId) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        read_setup_request(&mut s).await;
        s.write_all(&setup_ok()).await.unwrap();
        let mut request = [0u8; 4];
        s.read_exact(&mut request).await.unwrap();
        assert_eq!(request[0], 99, "the first request is ListExtensions");
        let mut reply = vec![1, 0];
        reply.extend(1u16.to_le_bytes());
        reply.extend((((total - 32) / 4) as u32).to_le_bytes());
        reply.resize(total, 0);
        let _ = s.write_all(&reply).await;
        // Answer the sync that may follow, and hold the connection open.
        let mut rest = Vec::new();
        let _ = s.read_to_end(&mut rest).await;
    });
    client(
        addr.to_string(),
        json!({}),
        vec![
            json!({"event_pattern":"x11_connected","handler":{"type":"static","actions":[{"type":"x11_list_extensions"}]}}),
            json!({"event_pattern":"*","handler":{"type":"static","actions":[]}}),
        ],
    )
    .await
    .expect("setup succeeds")
}

#[tokio::test]
async fn a_reply_past_the_bound_ends_the_connection() {
    // Exactly the bound is read and answered.
    let (state, id) = oversized(MAX_REPLY_BYTES).await;
    let listed = wait_match(&state, id, "x11_result", |r| {
        r["action"] == "x11_list_extensions"
    })
    .await;
    assert_eq!(listed["extensions"], json!([]), "{listed}");
    // Four bytes more (one length unit) is refused before it is read.
    let (state, id) = oversized(MAX_REPLY_BYTES + 4).await;
    let status = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(c) = state.get_client(id).await {
                if let ClientStatus::Error(e) = c.status {
                    return e;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the client kept the connection");
    assert!(
        status.contains(&format!("the bound is {MAX_REPLY_BYTES}")),
        "{status}"
    );
}

#[tokio::test]
async fn a_self_feeding_chain_stops_at_the_depth_bound() {
    let (_xvfb, display) = xvfb(&["-ac"]).await;
    // Every result asks for another geometry: without the bound this never ends.
    let (state, id) = client(
        format!("127.0.0.1:{}", 6000 + display),
        json!({}),
        vec![json!({"event_pattern":"*","handler":{"type":"static","actions":[{"type":"x11_get_geometry","window":"root"}]}})],
    )
    .await
    .unwrap();
    let results = || async {
        state
            .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
            .await
            .iter()
            .filter(|e| serde_json::to_value(e).unwrap()["event_type"] == "x11_result")
            .count()
    };
    let bound = netget::client::x11::MAX_FOLLOWUP_DEPTH as usize;
    tokio::time::timeout(Duration::from_secs(20), async {
        while results().await < bound - 1 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the chain did not reach the bound"));
    // The thing under test is that nothing further happens, which cannot be waited on.
    tokio::time::sleep(Duration::from_secs(1)).await;
    // The access log records the events a handler answered: results at depths 1 to bound-1.
    // The result at depth `bound` is produced and deliberately not answered, which ends it.
    assert_eq!(
        results().await,
        bound - 1,
        "the chain went past MAX_FOLLOWUP_DEPTH"
    );
}
