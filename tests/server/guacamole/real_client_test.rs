//! NetGet's Guacamole server against two independent client implementations: **pyguacamole**
//! and Apache's own **guacamole-common** (Java), the library the Guacamole web application
//! speaks to guacd with. Both complete the handshake, read what the model drew (PNG text
//! decoded by the client side, rectangles, the clipboard), type, click and send their
//! clipboard. `install_peers.py` installs both and prints `NETGET_GUACAMOLE_PYTHON` and
//! `NETGET_GUACAMOLE_JAVA_CP`; the tests fail rather than skip without them. No LLM calls:
//! a python policy is the model. Then raw instructions for the refusals and bounds.
use netget::cli::management::ServerForm;
use netget::state::app_state::AppState;
use netget::state::{AccessLogOwner, ServerId};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

const POLICY: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
a=[]
if t=='guacamole_connect':
  if e['arguments'].get('username')=='mallory':
    a=[{'type':'guacamole_reject','message':'Unknown user'}]
  else:
    a=[{'type':'guacamole_accept'},{'type':'guacamole_fill','color':'#202060'},
       {'type':'guacamole_text','x':8,'y':8,'text':'NetGet','scale':2},
       {'type':'guacamole_clipboard','text':'welcome'}]
elif t=='guacamole_typed':
  a=[{'type':'guacamole_text','x':8,'y':40,'text':'you typed: '+e['text'],'background':'#202060','scale':1}]
elif t=='guacamole_key':
  a=[{'type':'guacamole_text','x':8,'y':56,'text':'key: '+e['key'],'scale':1}]
elif t=='guacamole_click':
  a=[{'type':'guacamole_fill','x':e['x'],'y':e['y'],'width':10,'height':10,'color':'#ff0000'}]
elif t=='guacamole_clipboard_received':
  a=[{'type':'guacamole_clipboard','text':'echo: '+e['text']}]
print(json.dumps({'actions':a}))"#;

fn env(var: &str) -> String {
    let v = std::env::var(var).unwrap_or_default();
    assert!(
        !v.is_empty(),
        "{var} is required: python3 tests/server/guacamole/install_peers.py <dir> and export what it prints"
    );
    v
}

async fn start(handlers: Option<Vec<Value>>) -> (AppState, ServerId, u16) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "guacamole".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Be a remote desktop".into()),
        event_handlers: handlers,
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .unwrap();
    let port = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(a) = state.get_server(id).await.and_then(|s| s.local_addr) {
                break a.port();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (state, id, port)
}

fn policy() -> Option<Vec<Value>> {
    Some(vec![
        json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":POLICY}}),
    ])
}

async fn events(state: &AppState, id: ServerId, event_type: &str) -> Vec<Value> {
    state
        .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
        .await
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .filter(|e| e["event_type"] == event_type)
        .map(|e| e["request"].clone())
        .collect()
}

async fn run(program: &str, args: &[&str]) -> Vec<Value> {
    let out = tokio::time::timeout(
        Duration::from_secs(90),
        tokio::process::Command::new(program).args(args).output(),
    )
    .await
    .expect("the peer did not finish")
    .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    text.lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l:?}\n{text}")))
        .collect()
}

fn step<'a>(steps: &'a [Value], name: &str) -> &'a Value {
    steps
        .iter()
        .find(|s| s["step"] == name)
        .unwrap_or_else(|| panic!("no {name}: {steps:?}"))
}

#[tokio::test]
async fn pyguacamole_drives_netget() {
    let (state, id, port) = start(policy()).await;
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/server/guacamole/pyguac_session.py"
    );
    let steps = run(
        &env("NETGET_GUACAMOLE_PYTHON"),
        &[script, "127.0.0.1", &port.to_string(), "alice"],
    )
    .await;
    assert!(
        step(&steps, "ready")["id"]
            .as_str()
            .unwrap()
            .starts_with('$'),
        "{steps:?}"
    );
    let first = step(&steps, "first");
    assert_eq!(
        first["sizes"],
        json!([["0", "800", "600"]]),
        "the display is the size asked for"
    );
    assert_eq!(
        first["rects"],
        json!([["0", "0", "0", "800", "600"]]),
        "{first}"
    );
    assert_eq!(
        first["fills"],
        json!([["14", "0", "32", "32", "96", "255"]]),
        "#202060"
    );
    // "NetGet" in an 8x8 font at scale 2: 6 characters × 16 px by 16 px.
    assert_eq!(
        first["images"],
        json!([{"mimetype": "image/png", "x": 8, "y": 8, "width": 96, "height": 16}])
    );
    assert_eq!(first["clipboard"], "welcome");
    // "hi", BackSpace, "o", Enter: the model got the edited line.
    let typed = step(&steps, "typed");
    assert_eq!(
        typed["images"][0]["width"],
        8 * "you typed: ho".len(),
        "{typed}"
    );
    assert_eq!(
        step(&steps, "escape")["images"][0]["width"],
        8 * "key: Escape".len()
    );
    assert_eq!(
        step(&steps, "click")["rects"],
        json!([["0", "50", "60", "10", "10"]])
    );
    assert_eq!(step(&steps, "clipboard")["clipboard"], "echo: from python");

    let connect = &events(&state, id, "guacamole_connect").await[0];
    assert_eq!(connect["protocol"], "vnc");
    assert_eq!(
        connect["arguments"],
        json!({"hostname": "desktop.example", "port": "5900", "username": "alice"}),
        "the password is not shown"
    );
    assert_eq!(connect["secret_arguments"], json!(["password"]));
    assert_eq!(
        (connect["width"].as_u64(), connect["height"].as_u64()),
        (Some(800), Some(600))
    );
    assert_eq!(events(&state, id, "guacamole_typed").await[0]["text"], "ho");
    assert_eq!(
        events(&state, id, "guacamole_click").await[0]["button"],
        "left"
    );
}

#[tokio::test]
async fn guacamole_common_drives_netget() {
    let (state, id, port) = start(policy()).await;
    let cp = env("NETGET_GUACAMOLE_JAVA_CP");
    let steps = run(
        "java",
        &[
            "-cp",
            &cp,
            "GuacPeer",
            "127.0.0.1",
            &port.to_string(),
            "alice",
        ],
    )
    .await;
    let ready = step(&steps, "ready");
    assert!(ready["id"].as_str().unwrap().starts_with('$'), "{ready}");
    assert_eq!(
        ready["version"], "VERSION_1_3_0",
        "guacamole-common negotiated 1.3.0"
    );
    let first = step(&steps, "first");
    assert_eq!(first["pngs"], 1, "the text arrived as a PNG: {first}");
    assert_eq!(first["clipboard"], "welcome", "{first}");
    assert!(
        first["ops"].as_array().unwrap().contains(&json!("img")),
        "{first}"
    );
    assert_eq!(step(&steps, "typed")["pngs"], 1);
    assert_eq!(step(&steps, "clipboard")["clipboard"], "echo: from java");
    let connect = &events(&state, id, "guacamole_connect").await[0];
    assert_eq!(connect["timezone"], "Europe/Bratislava");
    assert_eq!(
        (connect["width"].as_u64(), connect["height"].as_u64()),
        (Some(640), Some(480))
    );
    assert_eq!(
        events(&state, id, "guacamole_typed").await[0]["text"],
        "java"
    );

    // A refused user: guacamole-common raises the unauthorized exception for status 769.
    let steps = run(
        "java",
        &[
            "-cp",
            &cp,
            "GuacPeer",
            "127.0.0.1",
            &port.to_string(),
            "mallory",
        ],
    )
    .await;
    let refused = step(&steps, "refused");
    assert_eq!(
        refused["exception"], "GuacamoleUnauthorizedException",
        "{refused}"
    );
    assert_eq!(refused["message"], "Unknown user");
}

#[tokio::test]
async fn refusals_and_bounds() {
    let (_state, _id, port) = start(None).await;
    // An unreachable model: the connection is refused with SERVER_ERROR, not left waiting.
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    s.write_all(b"6.select,3.vnc;").await.unwrap();
    let mut buf = vec![0u8; 512];
    let n = s.read(&mut buf).await.unwrap();
    let args = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(
        args.starts_with("4.args,13.VERSION_1_3_0,8.hostname,"),
        "{args}"
    );
    s.write_all(b"4.size,3.320,3.200,2.96;5.audio;5.video;5.image,9.image/png;7.connect,13.VERSION_1_3_0,1.h,0.,0.,0.;")
        .await
        .unwrap();
    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), s.read_to_end(&mut rest))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&rest),
        "5.error,14.Internal error,3.512;"
    );

    // Joining an existing connection is refused.
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    s.write_all(b"6.select,5.$abcd;").await.unwrap();
    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut rest))
        .await
        .unwrap()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&rest).starts_with("5.error,"),
        "{rest:?}"
    );

    // One instruction longer than guacd's bound closes the connection without an answer.
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let big = format!(
        "6.select,{}.{};",
        netget::server::guacamole::wire::MAX_INSTRUCTION + 1,
        "a".repeat(netget::server::guacamole::wire::MAX_INSTRUCTION + 1)
    );
    let _ = s.write_all(big.as_bytes()).await;
    let mut rest = Vec::new();
    let n = tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut rest))
        .await
        .unwrap()
        .unwrap_or(0);
    assert_eq!(n, 0, "{:?}", String::from_utf8_lossy(&rest));
}

#[test]
fn framing_counts_code_points() {
    use netget::server::guacamole::wire;
    assert_eq!(wire::encode("name", &["Žltá"]), "4.name,4.Žltá;");
    let (ins, used) = wire::parse("4.name,4.Žltá;5.extra".as_bytes())
        .unwrap()
        .unwrap();
    assert_eq!((ins.opcode.as_str(), ins.arg(0)), ("name", "Žltá"));
    assert_eq!(used, "4.name,4.Žltá;".len());
    assert!(wire::parse(b"4.name,4.Zl").unwrap().is_none(), "incomplete");
    assert!(wire::parse(b"4.name;5.x").is_ok());
    assert!(
        wire::parse(b"4.nam;x").is_err(),
        "a length that lies: no separator after it"
    );
    assert_eq!(wire::keysym_of_name("F1").unwrap(), 0xffbe);
    assert_eq!(wire::keysym_of_char('ž'), 0x0100_017e);
    assert_eq!(wire::name_of_keysym(0xff1b), "Escape");
}
