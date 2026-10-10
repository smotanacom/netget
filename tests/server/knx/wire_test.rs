//! KNXnet/IP gateway over raw UDP frames built with NetGet's own codec: tunnels, acks and
//! confirmations, a read answered by DPT, a write that triggers feedback, routing between
//! tunnels, duplicate sequence numbers, and the bounds (tunnel cap, unknown channels, size).
use netget::cli::management::ServerForm;
use netget::server::knx::wire::{self, Apci, Data, Telegram};
use netget::state::{app_state::AppState, ServerId};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

/// 1/2/4 reads 21.5 °C, 1/2/5 a text; a write to 1/2/3 is fed back on 1/2/10; reads of
/// anything else go unanswered.
pub const BUS_SCRIPT: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
a=[{'type':'knx_ignore'}]
if t=='knx_group_read':
  if e['destination']=='1/2/4': a=[{'type':'knx_group_response','value':21.5}]
  elif e['destination']=='1/2/5': a=[{'type':'knx_group_response','value':'NetGet','dpt':'16'}]
  else: a=[]
elif e['kind']=='write' and e['destination']=='1/2/3':
  a=[{'type':'knx_group_write','group_address':'1/2/10','value':e['value']}]
print(json.dumps({'actions':a}))"#;

pub fn handlers() -> Vec<Value> {
    vec![
        json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":BUS_SCRIPT}}),
    ]
}

pub fn group_types() -> Value {
    json!({"1/2/3": "1", "1/2/4": "9.001", "1/2/10": "1"})
}

pub async fn start(handlers: Vec<Value>, extra: Value) -> (AppState, ServerId, SocketAddr) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let mut params = json!({"group_types": group_types()});
    if let Value::Object(m) = extra {
        for (k, v) in m {
            params[k] = v;
        }
    }
    let id = ServerForm {
        protocol: "knx".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Be the bus".into()),
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

struct Tunnel {
    s: UdpSocket,
    gw: SocketAddr,
    channel: u8,
    address: u16,
    seq: u8,
}

async fn recv(s: &UdpSocket) -> Option<(u16, Vec<u8>)> {
    let mut buf = [0u8; 1024];
    let (n, _) = tokio::time::timeout(Duration::from_secs(20), s.recv_from(&mut buf))
        .await
        .ok()?
        .ok()?;
    let (svc, body) = wire::parse_frame(&buf[..n]).ok()?;
    Some((svc, body.to_vec()))
}

fn nat() -> Vec<u8> {
    vec![8, 1, 0, 0, 0, 0, 0, 0]
}

async fn connect(gw: SocketAddr) -> Result<Tunnel, u8> {
    let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut body = nat();
    body.extend(nat());
    body.extend([4, 4, 2, 0]);
    s.send_to(&wire::frame(wire::CONNECT_REQUEST, &body), gw)
        .await
        .unwrap();
    let (svc, b) = recv(&s).await.expect("a CONNECT_RESPONSE");
    assert_eq!(svc, wire::CONNECT_RESPONSE);
    if b[1] != 0 {
        return Err(b[1]);
    }
    Ok(Tunnel {
        s,
        gw,
        channel: b[0],
        address: u16::from_be_bytes([b[12], b[13]]),
        seq: 0,
    })
}

impl Tunnel {
    async fn send(&mut self, t: &Telegram) {
        let bytes = wire::tunnelling(self.channel, self.seq, &wire::cemi(t));
        self.seq = self.seq.wrapping_add(1);
        self.s.send_to(&bytes, self.gw).await.unwrap();
    }
    /// The next telegram the gateway sends, acked; acks for our own requests are returned
    /// as `None` telegrams with their status.
    async fn next(&self) -> Option<Result<Telegram, (u8, u8)>> {
        let (svc, b) = recv(&self.s).await?;
        match svc {
            wire::TUNNELLING_ACK => Some(Err((b[2], b[3]))),
            wire::TUNNELLING_REQUEST => {
                self.s
                    .send_to(&wire::tunnelling_ack(self.channel, b[2], 0), self.gw)
                    .await
                    .unwrap();
                Some(Ok(wire::parse_cemi(&b[4..]).unwrap().unwrap()))
            }
            _ => None,
        }
    }
    /// Telegrams until one matches.
    async fn until(&self, f: impl Fn(&Telegram) -> bool) -> Telegram {
        loop {
            match self.next().await {
                Some(Ok(t)) if f(&t) => return t,
                Some(_) => {}
                None => panic!("no matching telegram in time"),
            }
        }
    }
}

fn req(apci: Apci, ga: &str, data: Data) -> Telegram {
    Telegram {
        message_code: wire::L_DATA_REQ,
        source: 0,
        destination: wire::parse_group(ga).unwrap(),
        apci,
        data,
    }
}

#[tokio::test]
async fn tunnels_reads_writes_and_routing() {
    let (_state, _id, gw) = start(handlers(), json!({})).await;
    let mut a = connect(gw).await.unwrap();
    let b = connect(gw).await.unwrap();
    assert_ne!(a.channel, b.channel);
    assert_ne!(a.address, b.address);
    assert_eq!(
        wire::format_individual(a.address)
            .rsplit_once('.')
            .unwrap()
            .0,
        "1.1"
    );
    // A write: acked, confirmed with the tunnel's own address, heard by the other tunnel, and
    // the handler's feedback on 1/2/10 reaches both.
    a.send(&req(Apci::Write, "1/2/3", Data::Small(1))).await;
    assert_eq!(a.next().await, Some(Err((0, 0))));
    let con = a.until(|t| t.message_code == wire::L_DATA_CON).await;
    assert_eq!(
        (con.source, con.apci, con.data.clone()),
        (a.address, Apci::Write, Data::Small(1))
    );
    let heard = b
        .until(|t| t.destination == wire::parse_group("1/2/3").unwrap())
        .await;
    assert_eq!(
        (heard.message_code, heard.source),
        (wire::L_DATA_IND, a.address)
    );
    for t in [&a, &b] {
        let fb = t
            .until(|t| t.destination == wire::parse_group("1/2/10").unwrap())
            .await;
        assert_eq!(
            (fb.apci, fb.data, wire::format_individual(fb.source)),
            (Apci::Write, Data::Small(1), "1.1.250".into())
        );
    }
    // A read: answered by DPT 9 from the gateway's address, to every tunnel.
    a.send(&req(Apci::Read, "1/2/4", Data::Small(0))).await;
    let r = a.until(|t| t.apci == Apci::Response).await;
    assert_eq!(wire::decode("9.001", &r.data).unwrap(), json!(21.5));
    let r = b.until(|t| t.apci == Apci::Response).await;
    assert_eq!(wire::format_group(r.destination), "1/2/4");
    a.send(&req(Apci::Read, "1/2/5", Data::Small(0))).await;
    let r = a.until(|t| t.apci == Apci::Response).await;
    assert_eq!(wire::decode("16", &r.data).unwrap(), json!("NetGet"));
    // A repeated sequence number is acked again and not processed twice.
    let dup = wire::tunnelling(
        a.channel,
        a.seq.wrapping_sub(1),
        &wire::cemi(&req(Apci::Read, "1/2/5", Data::Small(0))),
    );
    a.s.send_to(&dup, gw).await.unwrap();
    assert_eq!(a.next().await, Some(Err((a.seq.wrapping_sub(1), 0))));
    // Heartbeat and disconnect.
    let mut body = vec![a.channel, 0];
    body.extend(nat());
    a.s.send_to(&wire::frame(wire::CONNECTIONSTATE_REQUEST, &body), gw)
        .await
        .unwrap();
    loop {
        if let Some((wire::CONNECTIONSTATE_RESPONSE, b)) = recv(&a.s).await {
            assert_eq!(b, vec![a.channel, 0]);
            break;
        }
    }
    a.s.send_to(&wire::frame(wire::DISCONNECT_REQUEST, &body), gw)
        .await
        .unwrap();
    loop {
        if let Some((wire::DISCONNECT_RESPONSE, b)) = recv(&a.s).await {
            assert_eq!(b, vec![a.channel, 0]);
            break;
        }
    }
    a.s.send_to(&wire::frame(wire::CONNECTIONSTATE_REQUEST, &body), gw)
        .await
        .unwrap();
    loop {
        if let Some((wire::CONNECTIONSTATE_RESPONSE, b)) = recv(&a.s).await {
            assert_eq!(b[1], wire::E_CONNECTION_ID, "a closed channel is unknown");
            break;
        }
    }
}

#[tokio::test]
async fn bounds_and_refusals() {
    let (_state, _id, gw) = start(handlers(), json!({"max_tunnels": 1})).await;
    let _a = connect(gw).await.unwrap();
    assert_eq!(connect(gw).await.err(), Some(wire::E_NO_MORE_CONNECTIONS));
    // A non-tunnel connection type is refused.
    let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut body = nat();
    body.extend(nat());
    body.extend([4, 3, 2, 0]);
    s.send_to(&wire::frame(wire::CONNECT_REQUEST, &body), gw)
        .await
        .unwrap();
    let (_, b) = recv(&s).await.unwrap();
    assert_eq!(b[1], wire::E_CONNECTION_TYPE);
    // Oversized and malformed frames are ignored; the gateway still answers afterwards.
    s.send_to(&vec![0x06; wire::MAX_FRAME + 1], gw)
        .await
        .unwrap();
    s.send_to(&[0x06, 0x10, 0x02, 0x03, 0xff, 0xff], gw)
        .await
        .unwrap();
    s.send_to(&wire::frame(wire::DESCRIPTION_REQUEST, &nat()), gw)
        .await
        .unwrap();
    let (svc, b) = recv(&s).await.unwrap();
    assert_eq!(svc, wire::DESCRIPTION_RESPONSE);
    assert!(String::from_utf8_lossy(&b).contains("NetGet KNX/IP"));
}

#[tokio::test]
async fn unanswered_read_and_unreachable_model_send_nothing() {
    // A read nobody answers (the script's silence) and a model that cannot be reached both
    // leave the bus quiet: no invented value.
    for handlers in [handlers(), vec![]] {
        let (_state, _id, gw) = start(handlers, json!({})).await;
        let mut a = connect(gw).await.unwrap();
        a.send(&req(Apci::Read, "1/2/9", Data::Small(0))).await;
        assert_eq!(a.next().await, Some(Err((0, 0))));
        let _con = a.until(|t| t.message_code == wire::L_DATA_CON).await;
        let mut buf = [0u8; 64];
        let quiet = tokio::time::timeout(Duration::from_secs(3), a.s.recv_from(&mut buf)).await;
        assert!(quiet.is_err(), "nothing after the confirmation");
    }
}
