//! Cap'n Proto RPC server over raw messages built with NetGet's own encoder: bootstrap, calls
//! on the import and on the promised bootstrap answer, the schema-driven JSON mapping (unions,
//! groups, defaults, lists, nested structs, Data), exceptions, unimplemented, and the bounds:
//! segments, size, nesting, traversal amplification, and the fail-closed paths.
use netget::cli::management::ServerForm;
use netget::server::capnp_rpc::{
    layout::{self, Builder, Message, Target},
    rpc::{self, Incoming, Returned},
    schema::Schema,
};
use netget::state::{app_state::AppState, AccessLogOwner, ServerId};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
};

pub const SCHEMA: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/server/capnp_rpc/directory.capnp"
);

/// ping and add compute; lookup builds an entry (and refuses "missing"); store echoes its entry
/// with a count; fail raises.
pub const DIRECTORY_SCRIPT: &str = r#"import json,sys
e=json.load(sys.stdin)['event']; m=e['method']; p=e['params']
if m=='ping': a={'type':'capnp_return','results':{'pong':'pong from netget'}}
elif m=='add': a={'type':'capnp_return','results':{'sum':p['a']+p['b']}}
elif m=='lookup' and p['name']=='missing': a={'type':'capnp_exception','reason':'no entry named missing'}
elif m=='lookup': a={'type':'capnp_return','results':{'entry':{'name':p['name'],'size':1234,'kind':'file','tags':['doc',p['name']],'owner':{'uid':501},'target':'/docs/'+p['name'],'scores':[0.5]}}}
elif m=='store': a={'type':'capnp_return','results':{'stored':p['entry'],'count':len(p['entry']['children'] or [])+1}}
else: a={'type':'capnp_exception','reason':'refused: '+p['why'],'kind':'failed'}
print(json.dumps({'actions':[a]}))"#;

pub fn handlers() -> Vec<Value> {
    vec![
        json!({"event_pattern":"capnp_call","handler":{"type":"script","language":"python","code":DIRECTORY_SCRIPT}}),
    ]
}

/// Fail, naming the package, when the Cap'n Proto compiler is absent.
pub fn require_capnp() {
    let ok = std::process::Command::new("capnp")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    assert!(
        ok,
        "the capnp compiler is required: apt-get install capnproto (or brew install capnp)"
    );
}

pub async fn start(handlers: Vec<Value>, schema: &str) -> (AppState, ServerId, SocketAddr) {
    require_capnp();
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "capnp-rpc".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Serve the directory".into()),
        startup_params: Some(json!({"schema": schema, "interface": "Directory"})),
        event_handlers: Some(handlers),
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .unwrap();
    let addr = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(a) = state.get_server(id).await.and_then(|s| s.local_addr) {
                break a;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (state, id, SocketAddr::from(([127, 0, 0, 1], addr.port())))
}

pub async fn handler_saw(state: &AppState, id: ServerId, needle: &str) -> bool {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if state
                .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
                .await
                .iter()
                .any(|e| serde_json::to_string(e).unwrap().contains(needle))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

fn schema() -> Schema {
    let out = std::process::Command::new("capnp")
        .args(["compile", "-o-", SCHEMA])
        .output()
        .expect("capnp compile");
    Schema::load(&out.stdout).unwrap()
}

async fn send(s: &mut TcpStream, words: &[u64]) {
    s.write_all(&layout::frame(words)).await.unwrap();
}

async fn recv(s: &mut TcpStream) -> Message {
    let segments = tokio::time::timeout(
        Duration::from_secs(30),
        layout::read_message(s, Duration::from_secs(30)),
    )
    .await
    .expect("a reply in time")
    .unwrap()
    .expect("a reply, not a close");
    Message::new(segments)
}

/// The connection was closed with nothing more written.
async fn closed_silently(s: &mut TcpStream) -> bool {
    let mut buf = [0u8; 1];
    matches!(
        tokio::time::timeout(Duration::from_secs(20), s.read(&mut buf)).await,
        Ok(Ok(0)) | Ok(Err(_))
    )
}

fn directory(schema: &Schema) -> (u64, u64) {
    let d = schema.interface("Directory").unwrap();
    let b = schema.interface("Base").unwrap();
    (d.id, b.id)
}

/// Call `method` with `params` on `target` (an import, or the bootstrap question's answer).
fn call_words(
    schema: &Schema,
    question: u32,
    iface: u64,
    method: &str,
    params: &Value,
    promised: Option<u32>,
) -> Vec<u64> {
    let m = schema
        .methods_of(iface)
        .into_iter()
        .find(|(_, _, m)| m.name == method)
        .map(|(i, _, m)| (i, m.clone()))
        .unwrap();
    let (dw, pc) = schema.struct_size(m.1.params).unwrap();
    let mut words = rpc::call(question, 0, m.0, m.1.id, |b, payload| {
        let s = b.init_struct(payload, 0, dw, pc)?;
        schema.from_json(b, m.1.params, s, params)
    })
    .unwrap();
    if let Some(q) = promised {
        // Re-address the call to the promised answer of bootstrap question q: rewrite the
        // MessageTarget (the first struct after Message, Call) as promisedAnswer.
        let mut b = Builder {
            words: std::mem::take(&mut words),
        };
        let call_at = target_of(&b.words, 2);
        let target_at = target_of(&b.words, call_at + 3);
        let target = layout::StructBuilder {
            data: target_at,
            data_words: 1,
            ptrs: target_at + 1,
            ptr_count: 1,
        };
        b.set_u16(target, 2, 1);
        let pa = b.init_struct(target, 0, 1, 1).unwrap();
        b.set_u32(pa, 0, q);
        words = b.words;
    }
    words
}

/// Where the struct or list pointer at word `at` points.
fn target_of(words: &[u64], at: usize) -> usize {
    (at as i64 + 1 + ((words[at] as u32 as i32) >> 2) as i64) as usize
}

fn results(
    schema: &Schema,
    msg: &Message,
    answer: u32,
    results_id: u64,
) -> Result<Value, (String, u16)> {
    match rpc::decode(msg).unwrap() {
        Incoming::Return {
            answer: a,
            returned,
        } => {
            assert_eq!(a, answer);
            match returned {
                Returned::Results {
                    content: Target::Struct(s),
                    ..
                } => Ok(schema.to_json(results_id, s).unwrap()),
                Returned::Exception { reason, kind } => Err((reason, kind)),
                _ => panic!("unexpected return"),
            }
        }
        _ => panic!("not a Return"),
    }
}

fn results_id(schema: &Schema, iface: u64, method: &str) -> u64 {
    schema
        .methods_of(iface)
        .into_iter()
        .find(|(_, _, m)| m.name == method)
        .unwrap()
        .2
        .results
}

#[tokio::test]
async fn bootstrap_calls_and_mapping() {
    let (state, id, addr) = start(handlers(), SCHEMA).await;
    let schema = schema();
    let (dir, _) = directory(&schema);
    let mut s = TcpStream::connect(addr).await.unwrap();
    // A call pipelined on the bootstrap answer, sent before the bootstrap is answered.
    send(&mut s, &rpc::bootstrap(0).unwrap()).await;
    send(
        &mut s,
        &call_words(&schema, 1, dir, "add", &json!({"a": 40, "b": 2}), Some(0)),
    )
    .await;
    match rpc::decode(&recv(&mut s).await).unwrap() {
        Incoming::Return {
            answer: 0,
            returned:
                Returned::Results {
                    content: Target::Capability(0),
                    caps,
                },
        } => {
            assert_eq!(caps, vec![Some(0)])
        }
        _ => panic!("bootstrap did not return the capability"),
    }
    let r = recv(&mut s).await;
    assert_eq!(
        results(&schema, &r, 1, results_id(&schema, dir, "add")),
        Ok(json!({"sum": 42}))
    );
    // A superclass method through the import.
    send(
        &mut s,
        &call_words(&schema, 2, dir, "ping", &json!({}), None),
    )
    .await;
    let r = recv(&mut s).await;
    assert_eq!(
        results(&schema, &r, 2, results_id(&schema, dir, "ping")),
        Ok(json!({"pong": "pong from netget"}))
    );
    // Unions, groups, defaults, Data, nested lists of structs survive a round trip.
    let entry = json!({"name":"src","size":4096,"kind":"directory","tags":["code","rust"],"owner":{"uid":1000,"gid":100},
        "target":"/srv/src","children":[{"name":"main.rs","size":12,"kind":"file","priority":-3,"blob":{"$hex":"00ff6869"}}],
        "scores":[1.5,-2.25],"hidden":false});
    send(
        &mut s,
        &call_words(&schema, 3, dir, "store", &json!({"entry": entry}), None),
    )
    .await;
    let r = recv(&mut s).await;
    let got = results(&schema, &r, 3, results_id(&schema, dir, "store")).unwrap();
    assert_eq!(got["count"], 2);
    let stored = &got["stored"];
    assert_eq!(stored["target"], "/srv/src");
    assert!(
        stored.get("none").is_none() && stored.get("blob").is_none(),
        "{stored}"
    );
    assert_eq!(
        stored["priority"], 5,
        "an absent field reads as its default"
    );
    assert_eq!(stored["hidden"], false);
    assert_eq!(stored["owner"], json!({"uid": 1000, "gid": 100}));
    assert_eq!(stored["scores"], json!([1.5, -2.25]));
    let child = &stored["children"][0];
    assert_eq!(
        (child["priority"].clone(), child["hidden"].clone()),
        (json!(-3), json!(true))
    );
    assert_eq!(child["blob"], json!({"$hex": "00ff6869"}));
    assert_eq!(child["none"], Value::Null);
    // The handler saw the decoded params too.
    assert!(handler_saw(&state, id, "\"name\":\"main.rs\"").await);
    // A handler exception.
    send(
        &mut s,
        &call_words(&schema, 4, dir, "lookup", &json!({"name": "missing"}), None),
    )
    .await;
    let r = recv(&mut s).await;
    assert_eq!(
        results(&schema, &r, 4, results_id(&schema, dir, "lookup")),
        Err(("no entry named missing".into(), rpc::EXC_FAILED))
    );
}

#[tokio::test]
async fn unimplemented_and_wrong_targets() {
    let (_state, _id, addr) = start(handlers(), SCHEMA).await;
    let schema = schema();
    let (dir, _) = directory(&schema);
    let mut s = TcpStream::connect(addr).await.unwrap();
    // A method id past the interface: unimplemented, no handler call.
    let mut words = call_words(&schema, 1, dir, "add", &json!({"a": 1, "b": 1}), None);
    let call_at = target_of(&words, 2);
    words[call_at] = (words[call_at] & !(0xffff << 32)) | (99u64 << 32);
    send(&mut s, &words).await;
    let r = recv(&mut s).await;
    let (reason, kind) = results(&schema, &r, 1, 0).unwrap_err();
    assert_eq!(kind, rpc::EXC_UNIMPLEMENTED, "{reason}");
    // A call on an import NetGet never exported.
    let mut words = call_words(&schema, 2, dir, "add", &json!({"a": 1, "b": 1}), None);
    let target_at = target_of(&words, call_at + 3);
    words[target_at] = 7;
    send(&mut s, &words).await;
    let r = recv(&mut s).await;
    assert!(results(&schema, &r, 2, 0)
        .unwrap_err()
        .0
        .contains("only the bootstrap"));
    // A message type NetGet does not handle is echoed back as unimplemented.
    let (mut b, m) = Builder::with_root(1, 1);
    b.set_u16(m, 0, 10); // provide
    b.init_struct(m, 0, 1, 2).unwrap();
    send(&mut s, &b.words).await;
    let r = recv(&mut s).await;
    let root = r.root_struct().unwrap();
    assert_eq!(root.u16(0), rpc::MSG_UNIMPLEMENTED);
    assert_eq!(root.struct_field(0).unwrap().unwrap().u16(0), 10);
}

#[tokio::test]
async fn precompiled_schema_needs_no_compiler() {
    require_capnp();
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("directory.bin");
    let out = std::process::Command::new("capnp")
        .args(["compile", "-o-", SCHEMA])
        .output()
        .unwrap();
    std::fs::write(&bin, out.stdout).unwrap();
    let (_state, _id, addr) = start(handlers(), bin.to_str().unwrap()).await;
    let schema = schema();
    let (dir_id, _) = directory(&schema);
    let mut s = TcpStream::connect(addr).await.unwrap();
    send(
        &mut s,
        &call_words(&schema, 1, dir_id, "add", &json!({"a": -5, "b": 2}), None),
    )
    .await;
    let r = recv(&mut s).await;
    assert_eq!(
        results(&schema, &r, 1, results_id(&schema, dir_id, "add")),
        Ok(json!({"sum": -3}))
    );
}

#[tokio::test]
async fn bounds_close_the_connection() {
    let (_state, _id, addr) = start(handlers(), SCHEMA).await;
    let schema = schema();
    let (dir, _) = directory(&schema);
    // More than 64 segments.
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(&(layout::MAX_SEGMENTS as u32).to_le_bytes())
        .await
        .unwrap();
    assert!(closed_silently(&mut s).await);
    // A segment table announcing more than 4 MiB: refused before anything is allocated.
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut head = 0u32.to_le_bytes().to_vec();
    head.extend_from_slice(&((layout::MAX_MESSAGE_WORDS as u32) + 1).to_le_bytes());
    s.write_all(&head).await.unwrap();
    assert!(closed_silently(&mut s).await);
    // Traversal amplification: 1000 tags pointing at one 64 KiB text. The message is ~70 KiB,
    // decoding it would visit 64 MiB.
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut words = call_words(
        &schema,
        1,
        dir,
        "store",
        &json!({"entry": {"name": "x", "tags": vec![""; 1000]}}),
        None,
    );
    let text_at = words.len();
    words.extend(std::iter::repeat_n(0x6161_6161_6161_6161u64, 8192));
    // Find the tags list: the pointer list of 1000 entries.
    let list_slot_value = |w: u64| w & 3 == 1 && (w >> 32) & 7 == 6 && (w >> 35) == 1000;
    let tags_ptr = words
        .iter()
        .position(|w| list_slot_value(*w))
        .expect("tags pointer");
    let first = target_of(&words, tags_ptr);
    for i in 0..1000 {
        let at = first + i;
        let off = (text_at as i64 - at as i64 - 1) as i32 as u32;
        words[at] = ((off << 2) as u64) | 1 | (2 << 32) | ((8192u64 * 8) << 35);
    }
    *words.last_mut().unwrap() &= 0x00ff_ffff_ffff_ffff; // NUL-terminate the text
    send(&mut s, &words).await;
    assert!(closed_silently(&mut s).await, "traversal limit");
    // Nesting: an Entry whose children nest 100 deep (built on a thread with room for the
    // encoder's recursion; the server's decoder stops at its bound).
    let mut s = TcpStream::connect(addr).await.unwrap();
    let words = std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(move || {
            let schema = super::wire_test::schema();
            let mut entry = json!({"name": "leaf"});
            for _ in 0..100 {
                entry = json!({"name": "n", "children": [entry]});
            }
            let words = call_words(&schema, 1, dir, "store", &json!({"entry": entry}), None);
            // Dropping a 100-deep JSON value recurses too.
            std::mem::forget(entry);
            words
        })
        .unwrap()
        .join()
        .unwrap();
    send(&mut s, &words).await;
    assert!(closed_silently(&mut s).await, "nesting limit");
    // Within bounds, still answered.
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut entry = json!({"name": "leaf"});
    for _ in 0..20 {
        entry = json!({"name": "n", "children": [entry]});
    }
    send(
        &mut s,
        &call_words(&schema, 1, dir, "store", &json!({"entry": entry}), None),
    )
    .await;
    let r = recv(&mut s).await;
    assert_eq!(
        results(&schema, &r, 1, results_id(&schema, dir, "store")).unwrap()["count"],
        2
    );
}

#[tokio::test]
async fn results_that_do_not_fit_and_no_handler_fail_closed() {
    // A handler answering a field the results struct does not have.
    let bad = vec![
        json!({"event_pattern":"capnp_call","handler":{"type":"static","actions":[{"type":"capnp_return","results":{"nope":1}}]}}),
    ];
    let schema = schema();
    let (dir, _) = directory(&schema);
    for handlers in [bad, vec![]] {
        let (_state, _id, addr) = start(handlers, SCHEMA).await;
        let mut s = TcpStream::connect(addr).await.unwrap();
        send(
            &mut s,
            &call_words(&schema, 1, dir, "add", &json!({"a": 1, "b": 2}), None),
        )
        .await;
        let r = recv(&mut s).await;
        let (reason, _) = results(&schema, &r, 1, 0).unwrap_err();
        assert!(reason.starts_with("netget:"), "{reason}");
    }
}

#[tokio::test]
async fn inline_schema_source() {
    // Inline source with no file id: one is derived from the text, so both sides agree.
    let inline = "interface Directory {\n  add @0 (a :Int32, b :Int32) -> (sum :Int64);\n}";
    let script = vec![
        json!({"event_pattern":"capnp_call","handler":{"type":"script","language":"python","code":DIRECTORY_SCRIPT}}),
    ];
    let (_state, _id, addr) = start(script, inline).await;
    let schema = netget::server::capnp_rpc::load_schema(inline)
        .await
        .unwrap();
    let (dir, _) = (schema.interface("Directory").unwrap().id, ());
    let mut s = TcpStream::connect(addr).await.unwrap();
    send(
        &mut s,
        &call_words(&schema, 1, dir, "add", &json!({"a": 20, "b": 22}), None),
    )
    .await;
    let r = recv(&mut s).await;
    assert_eq!(
        results(&schema, &r, 1, results_id(&schema, dir, "add")),
        Ok(json!({"sum": 42}))
    );
}
