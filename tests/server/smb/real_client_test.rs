//! SMB against real, independent clients.
//!
//! **Samba's `smbclient`** (GPLv3; Homebrew `samba`, Ubuntu `smbclient`) is a separate C
//! implementation of SMB2 that NetGet neither links nor wrote. It is run as a subprocess and
//! takes the server through the whole of an anonymous session: NEGOTIATE, SPNEGO/NTLMSSP
//! SESSION_SETUP in two legs, TREE_CONNECT, CREATE of the share root, QUERY_DIRECTORY until
//! `STATUS_NO_MORE_FILES`, the file-system size query behind `ls`'s "blocks available" line,
//! CREATE, QUERY_INFO and READ for `get`, then CLOSE, TREE_DISCONNECT and LOGOFF. The file it
//! fetches is 70 000 non-text bytes, more than one 64 KiB READ, so the range the server slices
//! out of the model's content is checked at two offsets; the bytes on disk afterwards are
//! compared with what the model said.
//!
//! **The Python `smbprotocol` library** (MIT; `python3 -m pip install smbprotocol`) is a
//! second, unrelated implementation, driven as a subprocess. It logs in as a named guest over
//! bare NTLMSSP, lists the share and reads the same file. It trusts the EndOfFile of the CREATE
//! response and never asks again, which `smbclient` does not, so it is the check on
//! `smb_create_file`'s `size`.
//!
//! Every answer comes from a mocked model (`.with_mock`), and every mock rule is counted.
//! The smbclient session is recorded through a relay and handed to the pcap oracle, which
//! reads it with Wireshark's `nbss` and `smb2` dissectors and must find no malformed frame.
//!
//! **These tests FAIL, they do not skip, when a client is absent.** A skip gate returns
//! `Ok(())` on a runner without the tool, so the suite reports a silent pass and the maturity
//! rating ends up resting on nothing; `tests/server/memcached/real_client_test.rs` is the
//! precedent. `registry-audit` installs both.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features smb --test server -- smb::real_client --test-threads=100

#![cfg(feature = "smb")]

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

use crate::helpers::pcap_oracle::PcapOracle;
use crate::server::helpers::{start_netget_server, E2EResult, NetGetConfig};

/// Locate a binary, or fail saying why a skip would be worse. Named `require_tool("…")` so
/// `scripts/beta_evidence_table.py` can see which third-party client this file drives.
fn require_tool(name: &str) -> String {
    for prefix in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
        let candidate = std::path::Path::new(prefix).join(name);
        if candidate.exists() {
            return candidate.to_string_lossy().into_owned();
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        if let Some(found) = path
            .split(':')
            .map(|dir| std::path::Path::new(dir).join(name))
            .find(|candidate| candidate.exists())
        {
            return found.to_string_lossy().into_owned();
        }
    }
    panic!(
        "`{name}` not found (searched /opt/homebrew/bin, /usr/local/bin, /usr/bin and $PATH). \
         This test drives Samba's own smbclient against NetGet's SMB server, and it is the \
         only independent check that our framing, headers, NTLMSSP exchange and file replies \
         are what a real SMB2 client expects. Skipping would leave SMB's evidence resting on \
         nothing, so this is a failure and not a skip. Install with `brew install samba` \
         (macOS) or `apt-get install -y smbclient` (Debian/Ubuntu)."
    );
}

/// Fail, naming the install command, unless `python3` can import smbprotocol.
fn require_smbprotocol() {
    let ok = std::process::Command::new("python3")
        .args(["-c", "import smbprotocol, smbclient"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(
        ok,
        "`python3 -c 'import smbprotocol'` failed. This test drives the Python smbprotocol \
         library against NetGet's SMB server as a second, independent SMB2 client; skipping \
         would leave SMB's evidence resting on one client, so this is a failure and not a \
         skip. Install with `python3 -m pip install smbprotocol`."
    );
}

/// The file both clients fetch: 70 000 bytes that are neither text nor a repeating pattern a
/// wrong offset could reproduce, so it arrives in two READs and any slip shows.
fn file_bytes() -> Vec<u8> {
    let mut state: u32 = 0x2545_F491;
    (0..70_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state >> 24) as u8
        })
        .collect()
}

fn file_base64() -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(file_bytes())
}

const FILE_NAME: &str = "report.bin";

/// Bytes that crossed a relayed connection, in order.
#[derive(Clone)]
enum Chunk {
    ToServer(Vec<u8>),
    FromServer(Vec<u8>),
}

/// A TCP relay in front of the server that records the first connection through it.
struct Recorder {
    port: u16,
    chunks: Arc<Mutex<Vec<Chunk>>>,
    done: Arc<tokio::sync::Notify>,
}

impl Recorder {
    async fn start(upstream: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind relay");
        let port = listener.local_addr().unwrap().port();
        let chunks: Arc<Mutex<Vec<Chunk>>> = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(tokio::sync::Notify::new());
        let (rec, fin) = (chunks.clone(), done.clone());
        tokio::spawn(async move {
            let Ok((client, _)) = listener.accept().await else {
                return;
            };
            let server = TcpStream::connect(("127.0.0.1", upstream))
                .await
                .expect("relay connect upstream");
            let (mut cr, mut cw) = client.into_split();
            let (mut sr, mut sw) = server.into_split();
            let rec_up = rec.clone();
            let up = async move {
                let mut buf = [0u8; 16384];
                while let Ok(n) = cr.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    rec_up.lock().await.push(Chunk::ToServer(buf[..n].to_vec()));
                    if sw.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
                let _ = sw.shutdown().await;
            };
            let down = async move {
                let mut buf = [0u8; 16384];
                while let Ok(n) = sr.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    rec.lock().await.push(Chunk::FromServer(buf[..n].to_vec()));
                    if cw.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
                let _ = cw.shutdown().await;
            };
            tokio::join!(up, down);
            fin.notify_one();
        });
        Self { port, chunks, done }
    }

    /// Wait until the recorded connection has closed in both directions.
    async fn finished(&self, secs: u64) -> Vec<Chunk> {
        tokio::time::timeout(Duration::from_secs(secs), self.done.notified())
            .await
            .expect("the recorded connection never finished");
        self.chunks.lock().await.clone()
    }
}

/// The model's side of both sessions: the share root is a directory holding one file.
///
/// `create_size` decides whether `smb_create_file` names the file's size. smbclient is given
/// none, so its `get` has to ask through QUERY_INFO; smbprotocol is given it, because the
/// CREATE response's EndOfFile is all it reads.
fn mock_config(
    prompt: &'static str,
    auth_type: &'static str,
    create_size: bool,
    query_info_calls: usize,
) -> NetGetConfig {
    let content = file_base64();
    let size = file_bytes().len();
    NetGetConfig::new(prompt).with_mock(move |mock| {
        mock.on_event("smb_operation")
            .and_event_data_contains("operation", "session_setup")
            .and_event_data_contains("auth_type", auth_type)
            .respond_with_actions_from_event(|event| {
                serde_json::json!([{
                    "type": "smb_auth_success",
                    "username": event["username"].as_str().unwrap_or_default()
                }])
            })
            .expect_calls(1)
            .and()
            .on_event("smb_operation")
            .and_event_data_contains("operation", "create")
            .respond_with_actions_from_event(move |event| {
                let path = event["path"].as_str().unwrap_or_default().to_string();
                if path == "/" {
                    serde_json::json!([{"type": "smb_create_directory", "path": path}])
                } else if path == format!("/{FILE_NAME}") {
                    if create_size {
                        serde_json::json!([{"type": "smb_create_file", "path": path, "size": size}])
                    } else {
                        serde_json::json!([{"type": "smb_create_file", "path": path}])
                    }
                } else {
                    // Anything else does not exist: no create action is a refusal.
                    serde_json::json!([])
                }
            })
            .expect_at_least(2)
            .and()
            .on_event("smb_operation")
            .and_event_data_contains("operation", "query_directory")
            .respond_with_actions(serde_json::json!([{
                "type": "smb_list_directory",
                "path": "/",
                "files": [{
                    "name": FILE_NAME,
                    "size": size,
                    "is_directory": false,
                    "modified_time": "2026-03-04T05:06:07Z"
                }]
            }]))
            .expect_calls(1)
            .and()
            .on_event("smb_operation")
            .and_event_data_contains("operation", "query_info")
            .respond_with_actions(serde_json::json!([{
                "type": "smb_get_file_info",
                "path": format!("/{FILE_NAME}"),
                "size": size,
                "is_directory": false,
                "modified_time": "2026-03-04T05:06:07Z"
            }]))
            .expect_calls(query_info_calls)
            .and()
            .on_event("smb_operation")
            .and_event_data_contains("operation", "read")
            .respond_with_actions(serde_json::json!([{
                "type": "smb_read_file",
                "path": format!("/{FILE_NAME}"),
                "content": content,
                "encoding": "base64"
            }]))
            .expect_at_least(1)
            .and()
            .on_any()
            .respond_with_actions(serde_json::json!([{
                "type": "open_server",
                "port": 0,
                "base_stack": "SMB",
                "instruction": prompt
            }]))
            .expect_calls(1)
            .and()
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn smbclient_lists_the_share_and_gets_the_exact_bytes_and_the_pcap_oracle_reads_clean_smb2(
) -> E2EResult<()> {
    let smbclient = require_tool("smbclient");
    let server = start_netget_server(mock_config(
        "Serve a share over smb holding report.bin.",
        "anonymous",
        false,
        1,
    ))
    .await?;
    crate::helpers::wait_for_server_listening(&server, Duration::from_secs(30)).await?;
    let relay = Recorder::start(server.port).await;

    let dir = tempfile::TempDir::new()?;
    let local = dir.path().join("got.bin");
    let commands = format!("ls; get {FILE_NAME} {}", local.display());
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new(&smbclient)
            .args([
                "//127.0.0.1/share",
                "-p",
                &relay.port.to_string(),
                // An empty user and password: an anonymous login, not an attempt as the
                // Unix user running the test that is refused and then retried anonymously.
                "-U",
                "%",
                "-N",
                "-m",
                "SMB2",
                "-c",
                &commands,
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("smbclient did not exit within 60s")?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    println!(
        "--- smbclient -c '{commands}' (exit {:?}) ---\n{text}",
        output.status.code()
    );

    assert!(
        output.status.success(),
        "smbclient exited non-zero:\n{text}"
    );
    let listed = text
        .lines()
        .find(|l| l.trim_start().starts_with(FILE_NAME))
        .unwrap_or_else(|| panic!("`ls` did not list {FILE_NAME}:\n{text}"));
    assert!(
        listed.contains(" N ") || listed.contains(" A "),
        "`ls` shows {FILE_NAME} as a plain file: {listed:?}"
    );
    assert!(
        listed.contains("70000"),
        "`ls` shows the size the model gave: {listed:?}"
    );
    assert!(
        listed.contains("2026"),
        "`ls` shows the modified time the model gave: {listed:?}"
    );
    assert!(
        text.contains("blocks available"),
        "`ls` read the volume size (FileFsFullSizeInformation):\n{text}"
    );
    let got = std::fs::read(&local)?;
    assert_eq!(
        got.len(),
        file_bytes().len(),
        "smbclient fetched the whole file"
    );
    assert!(
        got == file_bytes(),
        "smbclient fetched exactly the bytes the model served"
    );

    // Everything that crossed the wire, read by Wireshark's own SMB2 dissector.
    let chunks = relay.finished(30).await;
    let mut oracle = PcapOracle::tcp("smb");
    for chunk in &chunks {
        oracle = match chunk {
            Chunk::ToServer(b) => oracle.to_server(b),
            Chunk::FromServer(b) => oracle.from_server(b),
        };
    }
    oracle.assert_clean();

    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}

/// The smbprotocol driver. `smbclient` here is smbprotocol's high-level module, not Samba's
/// binary.
const SMBPROTOCOL_DRIVER: &str = r#"
import sys
import smbprotocol
import smbclient

port = int(sys.argv[1])
out = sys.argv[2]
# A guest session has no key, so there is nothing to sign FSCTL_VALIDATE_NEGOTIATE_INFO
# with; smbprotocol itself says to turn the check off for guest access.
smbclient.ClientConfig(require_secure_negotiate=False)
smbclient.register_session("127.0.0.1", port=port, username="guest", password="",
                           auth_protocol="ntlm", require_signing=False)
names = smbclient.listdir(r"\\127.0.0.1\share", port=port)
print("LIST " + ",".join(sorted(names)))
with smbclient.open_file(r"\\127.0.0.1\share\report.bin", mode="rb", port=port) as f:
    data = f.read()
open(out, "wb").write(data)
print("READ %d" % len(data))
smbclient.reset_connection_cache()
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn smbprotocol_lists_the_share_and_reads_the_exact_bytes() -> E2EResult<()> {
    require_smbprotocol();
    // A named guest over NTLMSSP: the model sees `auth_type` "ntlm" and `password_verified`
    // false. smbprotocol trusts the CREATE's EndOfFile, so the size rides on
    // smb_create_file and QUERY_INFO is never asked.
    let server = start_netget_server(mock_config(
        "Serve a share over smb holding report.bin.",
        "ntlm",
        true,
        0,
    ))
    .await?;
    crate::helpers::wait_for_server_listening(&server, Duration::from_secs(30)).await?;

    let dir = tempfile::TempDir::new()?;
    let local = dir.path().join("got.bin");
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new("python3")
            .args([
                "-c",
                SMBPROTOCOL_DRIVER,
                &server.port.to_string(),
                &local.display().to_string(),
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("the smbprotocol driver did not exit within 60s")?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    println!(
        "--- smbprotocol (exit {:?}) ---\n{text}",
        output.status.code()
    );

    assert!(
        output.status.success(),
        "the smbprotocol driver failed:\n{text}"
    );
    assert!(
        text.contains(&format!("LIST {FILE_NAME}")),
        "smbprotocol listed exactly the one file the model named:\n{text}"
    );
    let got = std::fs::read(&local)?;
    assert_eq!(
        got.len(),
        file_bytes().len(),
        "smbprotocol read the whole file"
    );
    assert!(
        got == file_bytes(),
        "smbprotocol read exactly the bytes the model served"
    );

    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}
