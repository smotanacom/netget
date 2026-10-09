//! A2S server over raw datagrams: answers per query kind, the challenge exchange (always for
//! players and rules, optionally for info), split responses, and no reply for a wrong-kind
//! answer, a refusal, a handler failure, an oversized request or junk.
use netget::cli::management::ServerForm;
use netget::server::a2s::wire::{self, Kind, Reassembly};
use netget::state::{app_state::AppState, ServerId};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::{net::UdpSocket, sync::mpsc};

/// Answers each query kind; "rules" carries 150 entries so the answer must be split.
pub const QUERY_SCRIPT: &str = "import json,sys\nq=json.load(sys.stdin)['event']['query']\nif q=='info':\n  a={'type':'a2s_info','name':'NetGet Arena','map':'de_dust2','folder':'csgo','game':'Counter-Strike','app_id':730,'players':2,'max_players':16,'bots':1,'vac':True,'version':'1.38.8.1','port':27015,'keywords':'netget,test'}\nelif q=='players':\n  a={'type':'a2s_players','players':[{'name':'alice','score':12,'duration_secs':330.5},{'name':'bob','score':-1,'duration_secs':5}]}\nelse:\n  a={'type':'a2s_rules','rules':{('rule_%03d' % i):('value_%03d' % i) for i in range(150)}}\nprint(json.dumps({'actions':[a]}))";

pub fn handlers() -> Vec<Value> {
    vec![
        json!({"event_pattern":"a2s_query","handler":{"type":"script","language":"python","code":QUERY_SCRIPT}}),
    ]
}

pub async fn start(handlers: Vec<Value>, params: Value) -> (AppState, ServerId, SocketAddr) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "a2s".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Report a game server".into()),
        startup_params: Some(params),
        event_handlers: Some(handlers),
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .unwrap();
    let addr = tokio::time::timeout(Duration::from_secs(10), async {
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

/// Send a request and collect the logical reply; `None` after 1.5 s of silence.
async fn exchange(
    socket: &UdpSocket,
    addr: SocketAddr,
    request: &[u8],
) -> Option<(Vec<u8>, usize)> {
    socket.send_to(request, addr).await.unwrap();
    let mut reassembly = Reassembly::default();
    let mut buf = vec![0u8; 4096];
    let mut packets = 0;
    loop {
        let Ok(Ok((n, _))) =
            tokio::time::timeout(Duration::from_millis(1500), socket.recv_from(&mut buf)).await
        else {
            return None;
        };
        packets += 1;
        assert!(n <= wire::MAX_PACKET, "datagram of {n} bytes exceeds 1400");
        if let Some(payload) = reassembly.feed(&buf[..n]).unwrap() {
            return Some((payload, packets));
        }
    }
}

fn challenge_of(reply: &[u8]) -> u32 {
    assert_eq!(
        (reply.len(), reply[4]),
        (9, b'A'),
        "a challenge reply: {reply:?}"
    );
    u32::from_le_bytes(reply[5..9].try_into().unwrap())
}

#[tokio::test]
async fn queries_challenges_and_split_answers() {
    let (state, id, addr) = start(handlers(), json!({})).await;
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (info, packets) = exchange(&socket, addr, &wire::encode_request(Kind::Info, None))
        .await
        .expect("info answered without a challenge");
    assert_eq!(packets, 1);
    assert_eq!(&info[..6], &[0xFF, 0xFF, 0xFF, 0xFF, b'I', 17]);
    let decoded = wire::decode_response(Kind::Info, &info).unwrap();
    assert_eq!(decoded["name"], "NetGet Arena");
    assert_eq!(
        (
            decoded["app_id"].clone(),
            decoded["bots"].clone(),
            decoded["vac"].clone()
        ),
        (json!(730), json!(1), json!(true))
    );
    assert_eq!(
        (decoded["port"].clone(), decoded["keywords"].clone()),
        (json!(27015), json!("netget,test"))
    );
    // Players and rules: ask for a challenge, be refused a wrong one, succeed with the right one.
    let (reply, _) = exchange(
        &socket,
        addr,
        &wire::encode_request(Kind::Players, Some(wire::NO_CHALLENGE)),
    )
    .await
    .unwrap();
    let challenge = challenge_of(&reply);
    let (again, _) = exchange(
        &socket,
        addr,
        &wire::encode_request(Kind::Players, Some(challenge ^ 1)),
    )
    .await
    .unwrap();
    assert_eq!(
        challenge_of(&again),
        challenge,
        "a wrong challenge is answered with the right one"
    );
    let (players, _) = exchange(
        &socket,
        addr,
        &wire::encode_request(Kind::Players, Some(challenge)),
    )
    .await
    .unwrap();
    let players = wire::decode_response(Kind::Players, &players).unwrap();
    assert_eq!(players[0]["name"], "alice");
    assert_eq!(
        (
            players[1]["score"].clone(),
            players[0]["duration_secs"].clone()
        ),
        (json!(-1), json!(330.5))
    );
    let (rules, packets) = exchange(
        &socket,
        addr,
        &wire::encode_request(Kind::Rules, Some(challenge)),
    )
    .await
    .unwrap();
    assert!(packets > 1, "150 rules do not fit one datagram");
    let rules = wire::decode_response(Kind::Rules, &rules).unwrap();
    assert_eq!(rules.as_object().unwrap().len(), 150);
    assert_eq!(rules["rule_149"], "value_149");
    state.remove_server(id).await;
    // info_challenge makes info ask for one too.
    let (state, id, addr) = start(handlers(), json!({"info_challenge": true})).await;
    let (reply, _) = exchange(&socket, addr, &wire::encode_request(Kind::Info, None))
        .await
        .unwrap();
    let challenge = challenge_of(&reply);
    let (info, _) = exchange(
        &socket,
        addr,
        &wire::encode_request(Kind::Info, Some(challenge)),
    )
    .await
    .unwrap();
    assert_eq!(info[4], b'I');
    state.remove_server(id).await;
}

#[tokio::test]
async fn no_answer_unless_the_handler_gives_the_right_one() {
    let wrong_kind = vec![
        json!({"event_pattern":"a2s_query","handler":{"type":"static","actions":[{"type":"a2s_players","players":[]}]}}),
    ];
    let refuse = vec![
        json!({"event_pattern":"a2s_query","handler":{"type":"static","actions":[{"type":"a2s_refuse","reason":"maintenance"}]}}),
    ];
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    for handlers in [wrong_kind, refuse, vec![]] {
        let (state, id, addr) = start(handlers, json!({})).await;
        assert!(
            exchange(&socket, addr, &wire::encode_request(Kind::Info, None))
                .await
                .is_none()
        );
        state.remove_server(id).await;
    }
    let (state, id, addr) = start(handlers(), json!({})).await;
    let mut oversized = wire::encode_request(Kind::Info, None);
    oversized.resize(1401, b'x');
    assert!(
        exchange(&socket, addr, &oversized).await.is_none(),
        "a request over 1400 bytes is dropped"
    );
    assert!(
        exchange(&socket, addr, b"\xff\xff\xff\xffZjunk")
            .await
            .is_none(),
        "an unknown request is dropped"
    );
    assert!(
        exchange(&socket, addr, &wire::encode_request(Kind::Info, None))
            .await
            .is_some(),
        "and the server still answers"
    );
    state.remove_server(id).await;
}
