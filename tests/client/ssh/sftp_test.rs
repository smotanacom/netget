//! Independent OpenSSH internal-sftp, NetGet pairing, and malicious framing probes.
use super::real_server_test::{current_user, start_sshd, start_sshd_with_subsystem};
use crate::helpers::real_server::RealServer;
use netget::state::client_handles::ClientSendOutcome;
use netget::{
    cli::management::{ClientForm, ServerForm},
    llm::OllamaClient,
    state::{AccessLogOwner, AppState, ClientId},
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
fn state() -> AppState {
    AppState::new_with_options(false, "http://127.0.0.1:1".into())
}
fn empty() -> Value {
    json!({"event_pattern":"*","handler":{"type":"static","actions":[]}})
}
fn fingerprint(peer: &RealServer) -> String {
    let out = std::process::Command::new("ssh-keygen")
        .args(["-l", "-E", "sha256", "-f"])
        .arg(peer.dir().join("host_key.pub"))
        .output()
        .expect("ssh-keygen is required");
    assert!(out.status.success());
    String::from_utf8(out.stdout)
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_string()
}
fn credentials(peer: &RealServer) -> Value {
    json!({"username":current_user(),"private_key_path":peer.dir().join("user_key"),"host_key_sha256":fingerprint(peer)})
}
async fn client(
    state: &AppState,
    remote: String,
    params: Value,
    handlers: Vec<Value>,
) -> anyhow::Result<ClientId> {
    let llm = OllamaClient::new("http://127.0.0.1:1");
    state.set_llm_client(llm.clone()).await;
    let (tx, _) = tokio::sync::mpsc::unbounded_channel();
    ClientForm {
        protocol: "ssh".into(),
        remote_addr: Some(remote),
        startup_params: Some(params),
        event_handlers: Some(handlers),
        ..Default::default()
    }
    .create(state, llm, tx)
    .await
}
async fn send(state: &AppState, id: ClientId, action: Value) -> anyhow::Result<ClientSendOutcome> {
    state
        .send_to_client(id, action, Duration::from_secs(8))
        .await
}
async fn result(state: &AppState, id: ClientId, path: &str, operation: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            for entry in state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await
                .into_iter()
                .rev()
            {
                if entry.event_type == "ssh_sftp_result"
                    && entry.request["path"] == path
                    && entry.request["operation"] == operation
                {
                    return entry.request;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}
#[tokio::test]
async fn sftp_openssh_stat_listing_chunked_text_and_followup_actions() {
    let peer = start_sshd().await.unwrap();
    let dir = peer.dir().join("content");
    std::fs::create_dir(&dir).unwrap();
    let file = dir.join("report.txt");
    let text = "OpenSSH chunk marker\n".repeat(6000);
    std::fs::write(&file, &text).unwrap();
    let state = state();
    let path = file.display().to_string();
    let handlers = vec![
        json!({"event_pattern":"ssh_connected","handler":{"type":"static","actions":[{"type":"set_memory","value":"SFTP session marker"},{"type":"sftp_stat","path":path}]}}),
        json!({"event_pattern":"ssh_sftp_result","handler":{"type":"script","language":"python","code":"import json,sys\nd=json.load(sys.stdin)\nassert d['client']['memory']=='SFTP session marker'\ne=d['event']\na=[]\nif e['operation']=='stat':\n a=[{'type':'sftp_read_file','path':e['path'],'length':e['attributes']['size']+1}]\nprint(json.dumps({'actions':a}))"}}),
        empty(),
    ];
    let id = client(&state, peer.addr(), credentials(&peer), handlers)
        .await
        .unwrap();
    let read = result(&state, id, &path, "read_file").await;
    assert_eq!(
        state.get_memory_for_client(id).await.as_deref(),
        Some("SFTP session marker")
    );
    assert_eq!(read["text"], text);
    assert_eq!(read["bytes_read"], text.len());
    assert_eq!(read["eof"], true);
    assert!(matches!(
        send(&state, id, json!({"type":"sftp_list_directory","path":dir}))
            .await
            .unwrap(),
        ClientSendOutcome::Executed { .. }
    ));
    let listing = result(&state, id, &dir.display().to_string(), "list_directory").await;
    assert!(listing["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["name"] == "report.txt" && entry["attributes"]["size"] == text.len()));
    assert!(matches!(
        send(
            &state,
            id,
            json!({"type":"sftp_read_file","path":file,"offset":8,"length":5})
        )
        .await
        .unwrap(),
        ClientSendOutcome::Executed { .. }
    ));
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await
                .iter()
                .any(|e| {
                    e.event_type == "ssh_sftp_result"
                        && e.request["offset"] == 8
                        && e.request["text"] == &text[8..13]
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let connection = state.get_client(id).await.unwrap().connection.unwrap();
    assert_ne!(connection.local_addr, connection.connected_addr);
    assert_eq!(connection.protocol_info.data["host_key_verified"], true);
    assert!(matches!(
        send(&state, id, json!({"type":"disconnect"}))
            .await
            .unwrap(),
        ClientSendOutcome::Disconnected
    ));
    peer.wait_for_log("Received disconnect from 127.0.0.1", Duration::from_secs(3))
        .await
        .unwrap();
    state.remove_client(id).await;
}
#[tokio::test]
async fn sftp_pins_host_keys_rejects_unpinned_sessions_and_bounds_actions() {
    let peer = start_sshd().await.unwrap();
    let state = state();
    let mut params = credentials(&peer);
    params["host_key_sha256"] = json!("SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
    assert!(client(&state, peer.addr(), params, vec![empty()])
        .await
        .is_err());
    let mut params = credentials(&peer);
    params.as_object_mut().unwrap().remove("host_key_sha256");
    let id = client(&state, peer.addr(), params, vec![empty()])
        .await
        .unwrap();
    assert!(
        matches!(send(&state,id,json!({"type":"sftp_stat","path":"/"})).await.unwrap(),ClientSendOutcome::Rejected{error} if error.contains("host_key_sha256"))
    );
    for action in [
        json!({"type":"sftp_stat","path":"x".repeat(4097)}),
        json!({"type":"sftp_read_file","path":"/","length":1048577}),
        json!({"type":"sftp_read_file","path":"/","offset":u64::MAX}),
        json!({"type":"sftp_stat","path":"bad\u{0}path"}),
        json!({"type":"sftp_stat","path":"/","follow_symlinks":"yes"}),
        json!({"type":"execute_command","command":"x".repeat(4097)}),
    ] {
        assert!(matches!(
            send(&state, id, action).await.unwrap(),
            ClientSendOutcome::Rejected { .. }
        ));
    }
    state.remove_client(id).await;
}
#[tokio::test]
async fn sftp_openssh_missing_binary_data_symlinks_and_entry_limit_recover() {
    let peer = start_sshd().await.unwrap();
    let state = state();
    let id = client(&state, peer.addr(), credentials(&peer), vec![empty()])
        .await
        .unwrap();
    assert!(format!(
        "{:#}",
        send(
            &state,
            id,
            json!({"type":"sftp_stat","path":peer.dir().join("missing")})
        )
        .await
        .unwrap_err()
    )
    .contains("SFTP status 2"));
    let binary = peer.dir().join("binary");
    std::fs::write(&binary, [255u8, 254]).unwrap();
    assert!(format!(
        "{:#}",
        send(&state, id, json!({"type":"sftp_read_file","path":binary}))
            .await
            .unwrap_err()
    )
    .contains("not UTF-8"));
    let target = peer.dir().join("target");
    std::fs::write(&target, "target").unwrap();
    #[cfg(unix)]
    {
        let link = peer.dir().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        send(&state, id, json!({"type":"sftp_stat","path":link}))
            .await
            .unwrap();
        assert_eq!(
            result(&state, id, &link.display().to_string(), "stat").await["attributes"]
                ["is_symlink"],
            true
        );
        send(
            &state,
            id,
            json!({"type":"sftp_stat","path":link,"follow_symlinks":true}),
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if state
                    .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                    .await
                    .iter()
                    .any(|entry| {
                        entry.event_type == "ssh_sftp_result"
                            && entry.request["path"] == link.display().to_string()
                            && entry.request["attributes"]["is_symlink"] == false
                    })
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    let dir = peer.dir().join("too-many");
    std::fs::create_dir(&dir).unwrap();
    for n in 0..1025 {
        std::fs::write(dir.join(format!("file-{n}")), "").unwrap();
    }
    assert!(format!(
        "{:#}",
        send(&state, id, json!({"type":"sftp_list_directory","path":dir}))
            .await
            .unwrap_err()
    )
    .contains("1024 entries"));
    send(&state, id, json!({"type":"sftp_read_file","path":target}))
        .await
        .unwrap();
    assert_eq!(
        result(&state, id, &target.display().to_string(), "read_file").await["text"],
        "target"
    );
    state.remove_client(id).await;
}
#[tokio::test]
async fn sftp_parked_handlers_reserve_slots_and_disconnect_clears_them() {
    let peer = start_sshd().await.unwrap();
    let state = state();
    let id = client(
        &state,
        peer.addr(),
        credentials(&peer),
        vec![json!({"event_pattern":"*","handler":{"type":"manual","timeout_secs":300}})],
    )
    .await
    .unwrap();
    for expected in 1..=16 {
        tokio::time::timeout(Duration::from_secs(3), async {
            while state.list_intercepts().await.len() != expected {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        if expected < 16 {
            send(&state, id, json!({"type":"sftp_stat","path":peer.dir()}))
                .await
                .unwrap();
        }
    }
    assert!(format!(
        "{:#}",
        send(&state, id, json!({"type":"sftp_stat","path":"/"}))
            .await
            .unwrap_err()
    )
    .contains("16 operations or handlers"));
    assert!(matches!(
        send(&state, id, json!({"type":"disconnect"}))
            .await
            .unwrap(),
        ClientSendOutcome::Disconnected
    ));
    tokio::time::timeout(Duration::from_secs(3), async {
        while !state.list_intercepts().await.is_empty() || state.has_client_handle(id).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    state.remove_client(id).await;
}
#[tokio::test]
async fn sftp_partial_packet_deadline_and_active_removal_close_channels() {
    // OpenSSH authenticates and hosts this deliberately malformed/stalled wire
    // fixture. It is a bound probe; independent SFTP evidence is internal-sftp.
    let script="import struct,sys,time\nr=sys.stdin.buffer;w=sys.stdout.buffer\nn=struct.unpack('>I',r.read(4))[0];r.read(n)\nw.write(struct.pack('>IBI',5,2,3));w.flush()\nn=struct.unpack('>I',r.read(4))[0];r.read(n)\nw.write(struct.pack('>I',100));w.flush()\ntime.sleep(30)\n";
    let peer = start_sshd_with_subsystem("/usr/bin/python3 {dir}/sftp_peer.py", script)
        .await
        .unwrap();
    let state = state();
    let mut params = credentials(&peer);
    params["operation_timeout_secs"] = json!(1);
    let id = client(&state, peer.addr(), params, vec![empty()])
        .await
        .unwrap();
    let error = send(&state, id, json!({"type":"sftp_stat","path":"/stalled"}))
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("operation deadline"));
    send(
        &state,
        id,
        json!({"type":"execute_command","command":"printf recovered"}),
    )
    .await
    .unwrap();
    let pending = tokio::spawn({
        let state = state.clone();
        async move { send(&state, id, json!({"type":"sftp_stat","path":"/remove"})).await }
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while peer
            .log()
            .matches("Starting session: subsystem 'sftp'")
            .count()
            < 2
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    state.remove_client(id).await;
    assert!(tokio::time::timeout(Duration::from_secs(2), pending)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    peer.wait_for_log("Connection closed", Duration::from_secs(3))
        .await
        .unwrap();
}

#[tokio::test]
async fn sftp_netget_pair_answers_semantic_listing_stat_and_text() {
    let state = state();
    let (tx, _) = tokio::sync::mpsc::unbounded_channel();
    let code = "import json,sys\ne=json.load(sys.stdin)['event']\nop=e['operation']\nif op in ('open','opendir'): a={'type':'sftp_handle','handle':e['path']}\nelif op=='readdir': a={'type':'sftp_directory_listing','entries':[] if e['path']=='/empty' else [{'name':'report.txt','size':10,'is_dir':False}]}\nelif op=='read': a={'type':'sftp_file_content','content':'pair text\\n'}\nelif op in ('lstat','stat','fstat'): a={'type':'sftp_file_attributes','size':10,'is_dir':False}\nelse: a={'type':'sftp_error','code':4}\nprint(json.dumps({'actions':[a]}))";
    let server = ServerForm {
        protocol: "ssh".into(),
        port: Some(0),
        instruction: Some(String::new()),
        event_handlers: Some(vec![
            json!({"event_pattern":"ssh_auth","handler":{"type":"static","actions":[{"type":"ssh_auth_decision","allowed":true}]}}),
            json!({"event_pattern":"sftp_operation","handler":{"type":"script","language":"python","code":code}}),
            empty(),
        ]),
        ..Default::default()
    }.create(&state,tx).await.unwrap();
    let port = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(address) = state
                .get_server(server)
                .await
                .and_then(|server| server.local_addr)
            {
                break address.port();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // The fixture's generated key is read independently; production receives an
    // operator-supplied pin and never trusts a scanned key automatically.
    let scanned = tokio::process::Command::new("ssh-keyscan")
        .args([
            "-T",
            "2",
            "-t",
            "ed25519",
            "-p",
            &port.to_string(),
            "127.0.0.1",
        ])
        .output()
        .await
        .expect("OpenSSH ssh-keyscan is required");
    assert!(
        scanned.status.success(),
        "{}",
        String::from_utf8_lossy(&scanned.stderr)
    );
    let scanned = String::from_utf8(scanned.stdout).unwrap();
    let encoded = scanned
        .lines()
        .find(|line| !line.starts_with('#'))
        .unwrap()
        .split_whitespace()
        .nth(2)
        .unwrap();
    let pin = format!(
        "SHA256:{}",
        russh_keys::parse_public_key_base64(encoded)
            .unwrap()
            .fingerprint()
    );
    let id = client(
        &state,
        format!("127.0.0.1:{port}"),
        json!({"username":"pair","password":"test","host_key_sha256":pin}),
        vec![empty()],
    )
    .await
    .unwrap();
    for (action, operation) in [
        (json!({"type":"sftp_stat","path":"/report.txt"}), "stat"),
        (
            json!({"type":"sftp_list_directory","path":"/"}),
            "list_directory",
        ),
        (
            json!({"type":"sftp_read_file","path":"/report.txt","length":11}),
            "read_file",
        ),
    ] {
        send(&state, id, action.clone()).await.unwrap();
        let output = result(&state, id, action["path"].as_str().unwrap(), operation).await;
        match operation {
            "stat" => assert_eq!(output["attributes"]["size"], 10),
            "list_directory" => assert_eq!(output["entries"][0]["name"], "report.txt"),
            _ => {
                assert_eq!(output["text"], "pair text\n");
                assert_eq!(output["eof"], true);
            }
        }
    }
    send(
        &state,
        id,
        json!({"type":"sftp_list_directory","path":"/empty"}),
    )
    .await
    .unwrap();
    assert_eq!(
        result(&state, id, "/empty", "list_directory").await["entries"],
        json!([])
    );
    send(&state, id, json!({"type":"disconnect"}))
        .await
        .unwrap();
    state.remove_client(id).await;
    state.remove_server(server).await;
}

#[tokio::test]
async fn ssh_handshake_and_idle_deadlines_close_owned_sockets() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = state();
    let pending = tokio::spawn({
        let state = state.clone();
        async move {
            client(
                &state,
                addr.to_string(),
                json!({"username":"test","password":"test","handshake_timeout_secs":1}),
                vec![empty()],
            )
            .await
        }
    });
    let (mut peer, _) = listener.accept().await.unwrap();
    let error = tokio::time::timeout(Duration::from_secs(3), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(format!("{error:#}").contains("handshake deadline"));
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), peer.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert!(bytes.starts_with(b"SSH-2.0-"));
    let peer = start_sshd().await.unwrap();
    let mut params = credentials(&peer);
    params["idle_timeout_secs"] = json!(1);
    let id = client(&state, peer.addr(), params, vec![empty()])
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(4), async {
        while state.has_client_handle(id).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    peer.wait_for_log("Connection closed", Duration::from_secs(3))
        .await
        .unwrap();
    state.remove_client(id).await;
}

#[tokio::test]
async fn ssh_combined_output_bound_leaves_other_channels_usable() {
    let peer = start_sshd().await.unwrap();
    let state = state();
    let id = client(&state, peer.addr(), credentials(&peer), vec![empty()])
        .await
        .unwrap();
    let error = send(&state,id,json!({"type":"execute_command","command":"python3 -c 'import sys; sys.stdout.write(\"x\"*600000); sys.stdout.flush(); sys.stderr.write(\"y\"*600000)'"})).await.unwrap_err();
    assert!(format!("{error:#}").contains("output exceeds 1 MiB"));
    send(
        &state,
        id,
        json!({"type":"execute_command","command":"printf recovered"}),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await
                .iter()
                .any(|entry| {
                    entry.event_type == "ssh_output_received"
                        && entry.request["output"] == "recovered"
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    state.remove_client(id).await;
}
#[tokio::test]
async fn sftp_framing_rejects_lengths_counts_and_trailing_bytes_before_allocation() {
    use netget::client::ssh::sftp::{decode_reply, read_packet};
    for length in [0, 65537, u32::MAX] {
        let (mut w, mut r) = tokio::io::duplex(8);
        w.write_u32(length).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), read_packet(&mut r))
                .await
                .unwrap()
                .is_err()
        );
    }
    for kind in [102u8, 103, 104] {
        let mut p = vec![kind];
        p.extend_from_slice(&1u32.to_be_bytes());
        p.extend_from_slice(&u32::MAX.to_be_bytes());
        assert!(decode_reply(&p, 1).is_err());
    }
    let mut attrs = vec![105];
    attrs.extend_from_slice(&1u32.to_be_bytes());
    attrs.extend_from_slice(&0u32.to_be_bytes());
    assert!(decode_reply(&attrs, 2).is_err());
    assert!(decode_reply(&attrs, 1).is_ok());
    attrs.push(0);
    assert!(decode_reply(&attrs, 1).is_err());
    let mut extension_bomb = vec![105, 0, 0, 0, 1];
    extension_bomb.extend_from_slice(&0x80000000u32.to_be_bytes());
    extension_bomb.extend_from_slice(&u32::MAX.to_be_bytes());
    assert!(decode_reply(&extension_bomb, 1).is_err());
}

#[tokio::test]
async fn sftp_total_reply_budget_and_empty_batch_bound_are_enforced() {
    use netget::client::ssh::sftp::{exchange, read_packet};
    for empty_batches in [false, true] {
        let (client, mut peer) = tokio::io::duplex(65536);
        let peer_run = async {
            assert_eq!(read_packet(&mut peer).await.unwrap(), vec![1, 0, 0, 0, 3]);
            peer.write_u32(5).await.unwrap();
            peer.write_all(&[2, 0, 0, 0, 3]).await.unwrap();
            let open = read_packet(&mut peer).await.unwrap();
            assert_eq!(open[0], 11);
            peer.write_u32(10).await.unwrap();
            peer.write_all(&[102, 0, 0, 0, 1, 0, 0, 0, 1, b'h'])
                .await
                .unwrap();
            let rounds = if empty_batches { 17 } else { 37 };
            for _ in 0..rounds {
                let request = read_packet(&mut peer).await.unwrap();
                assert_eq!(request[0], 12);
                let mut response = vec![104];
                response.extend_from_slice(&request[1..5]);
                response.extend_from_slice(&(if empty_batches { 0u32 } else { 7 }).to_be_bytes());
                if !empty_batches {
                    for _ in 0..7 {
                        for _ in 0..2 {
                            response.extend_from_slice(&4096u32.to_be_bytes());
                            response.extend(std::iter::repeat_n(b'x', 4096));
                        }
                        response.extend_from_slice(&0u32.to_be_bytes());
                    }
                }
                peer.write_u32(response.len() as u32).await.unwrap();
                peer.write_all(&response).await.unwrap();
            }
        };
        let action = json!({"type":"sftp_list_directory","path":"/"});
        let (result, ()) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(exchange(client, &action), peer_run)
        })
        .await
        .unwrap();
        let error = format!("{:#}", result.unwrap_err());
        assert!(
            error.contains(if empty_batches {
                "made no progress"
            } else {
                "packet budget exceeded"
            }),
            "{error}"
        );
    }
}

struct FloodPeer {
    controls: bool,
}
#[async_trait::async_trait]
impl russh::server::Handler for FloodPeer {
    type Error = anyhow::Error;
    async fn auth_password(&mut self, _: &str, _: &str) -> anyhow::Result<russh::server::Auth> {
        Ok(russh::server::Auth::Accept)
    }
    async fn channel_open_session(
        &mut self,
        _: russh::Channel<russh::server::Msg>,
        _: &mut russh::server::Session,
    ) -> anyhow::Result<bool> {
        Ok(true)
    }
    async fn exec_request(
        &mut self,
        channel: russh::ChannelId,
        _: &[u8],
        session: &mut russh::server::Session,
    ) -> anyhow::Result<()> {
        session.channel_success(channel);
        if self.controls {
            for _ in 0..=netget::client::ssh::MAX_CHANNEL_MESSAGES {
                session.xon_xoff_request(channel, false);
            }
        } else {
            // Type 2 is not stdout or stderr and is ignored by the command's
            // semantic reader. The SSH handler must still bound its buffering.
            let bytes = vec![b'x'; netget::client::ssh::MAX_CHANNEL_BYTES + 1];
            session.extended_data(channel, 2, russh::CryptoVec::from_slice(&bytes));
        }
        Ok(())
    }
}
#[tokio::test]
async fn ssh_transport_queues_bound_ignored_data_and_control_message_floods() {
    for controls in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let key = russh_keys::key::KeyPair::generate_ed25519().unwrap();
        let pin = format!("SHA256:{}", key.clone_public_key().unwrap().fingerprint());
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let config = russh::server::Config {
                keys: vec![key],
                ..Default::default()
            };
            let session = russh::server::run_stream(
                std::sync::Arc::new(config),
                stream,
                FloodPeer { controls },
            )
            .await
            .unwrap();
            let _ = session.await;
        });
        let state = state();
        let id = client(
            &state,
            addr.to_string(),
            json!({"username":"test","password":"test","host_key_sha256":pin}),
            vec![empty()],
        )
        .await
        .unwrap();
        let pending = tokio::spawn({
            let state = state.clone();
            async move {
                send(
                    &state,
                    id,
                    json!({"type":"execute_command","command":"flood"}),
                )
                .await
            }
        });
        // The ordinary operation deadline is 30 seconds. The transport bound,
        // rather than that timeout or the stdout/stderr reader, must close SSH.
        tokio::time::timeout(Duration::from_secs(4), async {
            while state.has_client_handle(id).await {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(pending.await.unwrap().is_err());
        tokio::time::timeout(Duration::from_secs(2), peer)
            .await
            .unwrap()
            .unwrap();
        state.remove_client(id).await;
    }
}
