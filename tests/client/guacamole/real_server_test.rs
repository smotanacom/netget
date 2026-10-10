//! NetGet's Guacamole client against **guacd** 1.3 (Apache's own daemon) driving TigerVNC's
//! **Xvnc**. What NetGet typed is read back from a file an xterm on that display writes its
//! input to; the clipboard is read and set with **xclip**. Fails rather than skips when any
//! of guacd (with its VNC plugin), Xvnc, xterm or xclip is missing. No LLM calls: a python
//! chain is the model.
//!
//! The chain: when the first frame arrives, click into the xterm, type a line, and set the
//! remote clipboard; when the remote clipboard changes, answer with "ack: " and its text.
use netget::cli::management::ClientForm;
use netget::state::app_state::AppState;
use netget::state::client_handles::ClientSendOutcome;
use netget::state::{AccessLogOwner, ClientId};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

const CHAIN: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
a=[]
if t=='guacamole_ready':
  a=[{'type':'guacamole_click','x':40,'y':40},{'type':'guacamole_type','text':'hello netget\n'},
     {'type':'guacamole_clipboard','text':'from netget'}]
elif t=='guacamole_clipboard_received' and not e['text'].startswith('ack: '):
  a=[{'type':'guacamole_clipboard','text':'ack: '+e['text']}]
print(json.dumps({'actions':a}))"#;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn spawn(program: &str, args: &[&str], hint: &str) -> Child {
    Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap_or_else(|e| panic!("{program}: {e}. Install it: {hint}"))
}

async fn wait_port(port: u16, what: &str) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err()
        {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what} never listened on {port}"));
}

/// Xvnc on a display of its own, and guacd; their ports and the display.
struct Desktop {
    _xvnc: Child,
    _guacd: Child,
    display: String,
    vnc: u16,
    guacd: u16,
}

/// `slot` keeps the tests in one run on different displays.
async fn desktop(slot: u32) -> Desktop {
    let display = format!(":{}", 100 + (std::process::id() % 400) * 2 + slot);
    let vnc = free_port();
    let xvnc = spawn(
        "Xvnc",
        &[
            &display,
            "-SecurityTypes",
            "None",
            "-geometry",
            "800x600",
            "-rfbport",
            &vnc.to_string(),
            "-localhost",
        ],
        "apt-get install tigervnc-standalone-server",
    );
    wait_port(vnc, "Xvnc").await;
    let guacd_port = free_port();
    let guacd = spawn(
        "guacd",
        &["-f", "-b", "127.0.0.1", "-l", &guacd_port.to_string()],
        "apt-get install guacd libguac-client-vnc0",
    );
    wait_port(guacd_port, "guacd").await;
    Desktop {
        _xvnc: xvnc,
        _guacd: guacd,
        display,
        vnc,
        guacd: guacd_port,
    }
}

async fn xclip_read(display: &str) -> String {
    let out = Command::new("xclip")
        .args(["-display", display, "-o", "-selection", "clipboard"])
        .output()
        .await
        .expect("xclip (apt-get install xclip)");
    String::from_utf8_lossy(&out.stdout).to_string()
}

async fn wait_for<F: std::future::Future<Output = bool>>(what: &str, mut f: impl FnMut() -> F) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !f().await {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("never: {what}"));
}

async fn events(state: &AppState, id: ClientId, kind: &str) -> Vec<Value> {
    state
        .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
        .await
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .filter(|e| e["event_type"] == kind)
        .map(|e| e["request"].clone())
        .collect()
}

async fn first_event(state: &AppState, id: ClientId, kind: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(e) = events(state, id, kind).await.into_iter().next() {
                return e;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {kind} event"))
}

async fn client(d: &Desktop, vnc_port: u16, handlers: Vec<Value>) -> (AppState, ClientId) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "guacamole".into(),
        remote_addr: Some(format!("127.0.0.1:{}", d.guacd)),
        instruction: Some("Use the desktop".into()),
        startup_params: Some(json!({"protocol": "vnc", "width": 800, "height": 600,
            "arguments": {"hostname": "127.0.0.1", "port": vnc_port.to_string()}})),
        event_handlers: Some(handlers),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
    .await
    .expect("connect");
    (state, id)
}

#[tokio::test]
async fn netget_drives_a_vnc_desktop_through_guacd() {
    let d = desktop(0).await;
    let dir = tempfile::tempdir().unwrap();
    let typed = dir.path().join("typed.txt");
    let _xterm = spawn(
        "xterm",
        &[
            "-display",
            &d.display,
            "-geometry",
            "80x24+0+0",
            "-e",
            "sh",
            "-c",
            &format!("cat > {}", typed.display()),
        ],
        "apt-get install xterm",
    );
    // The xterm has mapped once its pty is open and cat has created the file.
    wait_for("the xterm started", || async { typed.exists() }).await;

    let (state, id) = client(
        &d,
        d.vnc,
        vec![json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":CHAIN}})],
    )
    .await;

    let ready = first_event(&state, id, "guacamole_ready").await;
    assert_eq!(
        (ready["width"].as_u64(), ready["height"].as_u64()),
        (Some(800), Some(600)),
        "{ready}"
    );
    assert_eq!(ready["protocol"], "vnc");
    assert!(
        ready["connection_id"].as_str().unwrap().starts_with('$'),
        "{ready}"
    );

    // What the xterm read from the keyboard guacd forwarded.
    wait_for("the xterm received the typing", || async {
        std::fs::read_to_string(&typed).unwrap_or_default() == "hello netget\n"
    })
    .await;
    // NetGet's clipboard, as X holds it.
    wait_for("X holds NetGet's clipboard", || async {
        xclip_read(&d.display).await == "from netget"
    })
    .await;

    // The remote clipboard changes; NetGet hears it and answers.
    let mut setter = Command::new("xclip")
        .args(["-display", &d.display, "-i", "-selection", "clipboard"])
        .stdin(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = setter.stdin.take().unwrap();
        stdin.write_all(b"from x").await.unwrap();
    }
    wait_for("NetGet answered the remote clipboard", || async {
        xclip_read(&d.display).await == "ack: from x"
    })
    .await;
    let clip = events(&state, id, "guacamole_clipboard_received").await;
    assert!(clip.iter().any(|c| c["text"] == "from x"), "{clip:?}");
    assert!(
        clip[0]["display_updates"].as_u64().unwrap() > 0,
        "frames were drawn: {clip:?}"
    );

    // Injected: a key, and an action refused before anything is sent.
    let outcome = state
        .send_to_client(
            id,
            json!({"type":"guacamole_key","key":"Return"}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(
        matches!(outcome, ClientSendOutcome::Sent { .. }),
        "{outcome:?}"
    );
    wait_for("the injected Return", || async {
        std::fs::read_to_string(&typed).unwrap_or_default() == "hello netget\n\n"
    })
    .await;
    let outcome = state
        .send_to_client(
            id,
            json!({"type":"guacamole_key","key":"NoSuchKey"}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(
        matches!(&outcome, ClientSendOutcome::Rejected { error } if error.contains("unknown key")),
        "{outcome:?}"
    );
    drop(setter);
}

#[tokio::test]
async fn an_unreachable_vnc_server_is_reported() {
    let d = desktop(1).await;
    let closed = free_port();
    let (state, id) = client(
        &d,
        closed,
        vec![json!({"event_pattern":"*","handler":{"type":"static","actions":[]}})],
    )
    .await;
    let err = first_event(&state, id, "guacamole_error").await;
    assert_eq!(err["status"], 519, "UPSTREAM_NOT_FOUND: {err}");
}
