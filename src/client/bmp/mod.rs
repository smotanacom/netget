//! BGP Monitoring Protocol exporter over the collector's codec: the handler reports peers,
//! routes and statistics; Rust frames BMP and builds the embedded BGP PDUs.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::bgp::wire;
use crate::server::bmp::codec::{self, PeerHeader};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
use crate::utils::clock::{SystemTime, UNIX_EPOCH};
pub use actions::BmpClientProtocol;
use anyhow::{bail, ensure, Context, Result};
use netgauze_bgp_pkt::{
    community::Community,
    path_attribute::{Communities, PathAttribute, PathAttributeValue},
    update::BgpUpdateMessage,
    BgpMessage,
};
use serde_json::{json, Value as Json};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

pub const DEFAULT_SYS_NAME: &str = "netget";
pub const DEFAULT_SYS_DESCR: &str = "NetGet BMP exporter";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

fn now() -> (u32, u32) {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (d.as_secs() as u32, d.subsec_micros())
}

fn field<'a>(v: &'a Json, key: &str) -> Option<&'a Json> {
    v.get(key).filter(|x| !x.is_null())
}

fn ip(v: &Json, key: &str) -> Result<IpAddr> {
    field(v, key)
        .and_then(Json::as_str)
        .with_context(|| format!("{key} is an IP address"))?
        .parse()
        .with_context(|| format!("{key} is an IP address"))
}

fn ipv4(v: &Json, key: &str) -> Result<Ipv4Addr> {
    field(v, key)
        .and_then(Json::as_str)
        .with_context(|| format!("{key} is an IPv4 address"))?
        .parse()
        .with_context(|| format!("{key} is an IPv4 address"))
}

fn number<T: TryFrom<u64>>(v: &Json, key: &str, default: Option<T>) -> Result<T> {
    match field(v, key) {
        None => default.with_context(|| format!("{key} is required")),
        Some(n) => n
            .as_u64()
            .and_then(|n| T::try_from(n).ok())
            .with_context(|| format!("{key} is out of range")),
    }
}

fn flag(v: &Json, key: &str, default: bool) -> Result<bool> {
    match field(v, key) {
        None => Ok(default),
        Some(b) => b
            .as_bool()
            .with_context(|| format!("{key} is true or false")),
    }
}

fn strings(v: &Json, key: &str) -> Result<Vec<String>> {
    match field(v, key) {
        None => Ok(vec![]),
        Some(Json::Array(a)) => {
            ensure!(a.len() <= codec::MAX_ITEMS, "{key} has too many entries");
            a.iter()
                .map(|s| {
                    s.as_str()
                        .map(str::to_owned)
                        .with_context(|| format!("{key} holds strings"))
                })
                .collect()
        }
        _ => bail!("{key} is an array of strings"),
    }
}

/// Peers announced with bmp_peer_up, by address.
#[derive(Default, Clone)]
pub struct Peers(HashMap<IpAddr, PeerHeader>);

impl Peers {
    /// Refuse a malformed action before anything is sent; unknown peers are not an error here.
    pub fn validate(&self, kind: &str, v: &Json) -> Result<()> {
        self.clone().build(kind, v, true).map(|_| ())
    }

    fn peer(&self, v: &Json, lenient: bool) -> Result<PeerHeader> {
        let address = ip(v, "peer_address")?;
        match self.0.get(&address) {
            Some(p) => Ok(p.clone()),
            None if lenient => Ok(PeerHeader {
                peer_type: 0,
                flags: 0,
                distinguisher: [0; 8],
                address,
                asn: 0,
                bgp_id: Ipv4Addr::UNSPECIFIED,
                seconds: 0,
                micros: 0,
            }),
            None => bail!("peer {address} is not up; report bmp_peer_up first"),
        }
    }

    /// The framed message an action asks for.
    pub fn build(&mut self, kind: &str, v: &Json, lenient: bool) -> Result<Vec<u8>> {
        let (seconds, micros) = now();
        match kind {
            "bmp_peer_up" => {
                let p = field(v, "peer").context("peer is an object")?;
                let peer_type = match field(p, "type").and_then(Json::as_str) {
                    None => 0,
                    Some(t) => codec::peer_type_code(t)
                        .context("peer type is global, rd_instance, local_instance or loc_rib")?,
                };
                let distinguisher = match field(p, "distinguisher").and_then(Json::as_str) {
                    None => [0; 8],
                    Some(d) => codec::parse_distinguisher(d)?,
                };
                let mut flags = 0;
                if flag(p, "post_policy", false)? {
                    flags |= codec::FLAG_POST_POLICY;
                }
                if flag(p, "adj_rib_out", false)? {
                    flags |= codec::FLAG_ADJ_RIB_OUT;
                }
                if !flag(p, "four_octet_as", true)? {
                    flags |= codec::FLAG_LEGACY_AS;
                }
                let header = PeerHeader {
                    peer_type,
                    flags,
                    distinguisher,
                    address: ip(p, "address")?,
                    asn: number(p, "asn", None)?,
                    bgp_id: ipv4(p, "bgp_id")?,
                    seconds,
                    micros,
                };
                let local = ip(v, "local_address")?;
                ensure!(
                    local.is_ipv6() == header.address.is_ipv6(),
                    "local_address and the peer's address are the same family"
                );
                let hold: u16 = number(v, "hold_time", Some(90))?;
                ensure!(hold == 0 || hold >= 3, "hold_time is 0 or at least 3");
                let sent = wire::encode(wire::build_open(
                    number(v, "local_asn", None)?,
                    hold,
                    ipv4(v, "local_bgp_id")?,
                ))?;
                let received = wire::encode(wire::build_open(header.asn, hold, header.bgp_id))?;
                let mut body = Vec::new();
                match local {
                    IpAddr::V4(a) => {
                        body.extend([0u8; 12]);
                        body.extend(a.octets());
                    }
                    IpAddr::V6(a) => body.extend(a.octets()),
                }
                body.extend(number::<u16>(v, "local_port", Some(179))?.to_be_bytes());
                body.extend(number::<u16>(v, "remote_port", Some(179))?.to_be_bytes());
                body.extend(sent);
                body.extend(received);
                let out = codec::with_peer(codec::PEER_UP, &header, &body)?;
                self.0.insert(header.address, header);
                Ok(out)
            }
            "bmp_route_monitoring" => {
                let mut peer = self.peer(v, lenient)?;
                (peer.seconds, peer.micros) = (seconds, micros);
                let next_hop = match field(v, "next_hop") {
                    None => None,
                    Some(_) => Some(ipv4(v, "next_hop")?),
                };
                let as_path = match field(v, "as_path") {
                    None => vec![],
                    Some(Json::Array(a)) => {
                        ensure!(a.len() <= 255, "as_path has at most 255 AS numbers");
                        a.iter()
                            .map(|n| {
                                n.as_u64()
                                    .and_then(|n| u32::try_from(n).ok())
                                    .context("as_path holds AS numbers")
                            })
                            .collect::<Result<_>>()?
                    }
                    _ => bail!("as_path is an array of AS numbers"),
                };
                let origin = match field(v, "origin").and_then(Json::as_str) {
                    None | Some("IGP") => netgauze_bgp_pkt::path_attribute::Origin::IGP,
                    Some("EGP") => netgauze_bgp_pkt::path_attribute::Origin::EGP,
                    Some("INCOMPLETE") => netgauze_bgp_pkt::path_attribute::Origin::Incomplete,
                    Some(o) => bail!("origin {o:?} is IGP, EGP or INCOMPLETE"),
                };
                let intent = wire::UpdateIntent {
                    withdrawn: strings(v, "withdraw")?,
                    nlri: strings(v, "announce")?,
                    next_hop,
                    as_path,
                    origin,
                    med: field(v, "med")
                        .map(|_| number(v, "med", None))
                        .transpose()?,
                    local_pref: field(v, "local_pref")
                        .map(|_| number(v, "local_pref", None))
                        .transpose()?,
                };
                let mut update = wire::build_update(&intent, peer.four_octet_as())?;
                let communities = strings(v, "communities")?;
                if !communities.is_empty() {
                    ensure!(!intent.nlri.is_empty(), "communities go with announcements");
                    let list = communities
                        .iter()
                        .map(|c| {
                            let (a, n) = c.split_once(':').context("a community is asn:value")?;
                            Ok(Community::new(
                                ((a.parse::<u16>()? as u32) << 16) | n.parse::<u16>()? as u32,
                            ))
                        })
                        .collect::<Result<Vec<_>>>()
                        .context("communities are \"asn:value\" with 16-bit halves")?;
                    let BgpMessage::Update(u) = update else {
                        bail!("build_update returns an UPDATE")
                    };
                    let mut attrs = u.path_attributes().clone();
                    attrs.push(
                        PathAttribute::from(
                            true,
                            true,
                            false,
                            false,
                            PathAttributeValue::Communities(Communities::new(list)),
                        )
                        .map_err(|(_, e)| {
                            anyhow::anyhow!("invalid COMMUNITIES attribute: {e:?}")
                        })?,
                    );
                    update = BgpMessage::Update(BgpUpdateMessage::new(
                        u.withdraw_routes().clone(),
                        attrs,
                        u.nlri().clone(),
                    ));
                }
                codec::with_peer(codec::ROUTE_MONITORING, &peer, &wire::encode(update)?)
            }
            "bmp_statistics" => {
                let mut peer = self.peer(v, lenient)?;
                (peer.seconds, peer.micros) = (seconds, micros);
                let counters = match field(v, "counters") {
                    Some(Json::Array(a)) if !a.is_empty() && a.len() <= codec::MAX_ITEMS => a,
                    _ => bail!("counters is a non-empty array of {{type, value}}"),
                };
                let mut body = (counters.len() as u32).to_be_bytes().to_vec();
                for c in counters {
                    let t = match field(c, "type") {
                        Some(Json::String(name)) => codec::stat_code(name)
                            .with_context(|| format!("unknown statistics type {name:?}"))?,
                        Some(n) => n
                            .as_u64()
                            .and_then(|n| u16::try_from(n).ok())
                            .context("a counter type is a name or a number")?,
                        None => bail!("each counter has a type"),
                    };
                    let (gauge, per_afi) = codec::stat_shape(t);
                    let value: u64 = number(c, "value", None)?;
                    let mut bytes = Vec::new();
                    if per_afi {
                        bytes.extend(number::<u16>(c, "afi", Some(1))?.to_be_bytes());
                        bytes.push(number::<u8>(c, "safi", Some(1))?);
                    }
                    if gauge {
                        bytes.extend(value.to_be_bytes());
                    } else {
                        bytes.extend(
                            u32::try_from(value)
                                .context("a counter is 32 bits")?
                                .to_be_bytes(),
                        );
                    }
                    codec::put_tlv(&mut body, t, &bytes)?;
                }
                codec::with_peer(codec::STATISTICS, &peer, &body)
            }
            "bmp_peer_down" => {
                let mut peer = self.peer(v, lenient)?;
                (peer.seconds, peer.micros) = (seconds, micros);
                let reason = field(v, "reason")
                    .and_then(Json::as_str)
                    .unwrap_or_default();
                let code = codec::peer_down_code(reason).context("reason is local_notification, local_no_notification, remote_notification, remote_no_data, peer_deconfigured or local_system_closed")?;
                let mut body = vec![code];
                match code {
                    1 | 3 => {
                        let n = field(v, "notification")
                            .context("this reason needs notification {code, subcode}")?;
                        body.extend(wire::encode_notification(
                            number(n, "code", None)?,
                            number(n, "subcode", Some(0))?,
                            &[],
                        )?);
                    }
                    2 => body.extend(number::<u16>(v, "fsm_event", None)?.to_be_bytes()),
                    _ => {}
                }
                let out = codec::with_peer(codec::PEER_DOWN, &peer, &body)?;
                self.0.remove(&peer.address);
                Ok(out)
            }
            "bmp_termination" => {
                let reason = match field(v, "reason").and_then(Json::as_str) {
                    None => 0,
                    Some(r) => codec::termination_code(r).context("reason is administratively_closed, unspecified, out_of_resources, redundant_connection or permanently_administratively_closed")?,
                };
                let message = field(v, "message").and_then(Json::as_str);
                ensure!(
                    message.is_none_or(|m| m.len() <= 4096),
                    "message is at most 4096 bytes"
                );
                codec::termination(reason, message)
            }
            other => bail!("{other} is not a BMP message"),
        }
    }
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let (sys_name, sys_descr) = match ctx.startup_params.as_ref() {
        Some(p) => (
            p.get_optional_string("sys_name")?
                .unwrap_or_else(|| DEFAULT_SYS_NAME.into()),
            p.get_optional_string("sys_descr")?
                .unwrap_or_else(|| DEFAULT_SYS_DESCR.into()),
        ),
        None => (DEFAULT_SYS_NAME.into(), DEFAULT_SYS_DESCR.into()),
    };
    let initiation = codec::initiation(&sys_name, &sys_descr, &[])?;
    let mut stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&ctx.remote_addr))
        .await
        .context("BMP connect timed out")??;
    let local = stream.local_addr()?;
    let collector = stream.peer_addr()?.to_string();
    stream.write_all(&initiation).await?;
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<Json>(64);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(16);
    event_tx.try_send(Event::new(
        &actions::CONNECTED_EVENT,
        json!({"collector": collector, "sys_name": sys_name}),
    ))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = BmpClientProtocol;
        while let Some(event) = event_rx.recv().await {
            let instruction = events_ctx
                .state
                .get_instruction_for_client(events_ctx.client_id)
                .await
                .unwrap_or_default();
            let memory = events_ctx
                .state
                .get_memory_for_client(events_ctx.client_id)
                .await
                .unwrap_or_default();
            match call_llm_for_client(
                &events_ctx.llm_client,
                &events_ctx.state,
                events_ctx.client_id.to_string(),
                &instruction,
                &memory,
                Some(&event),
                &protocol,
                &events_ctx.status_tx,
            )
            .await
            {
                Ok(result) => {
                    if let Some(memory) = result.memory_updates {
                        events_ctx
                            .state
                            .set_memory_for_client(events_ctx.client_id, memory)
                            .await;
                    }
                    for action in result.actions {
                        if internal_tx.send(action).await.is_err() {
                            return;
                        }
                    }
                }
                Err(e) => {
                    Log::new(Some(&events_ctx.status_tx)).warn(format!("BMP client handler: {e}"))
                }
            }
        }
    });
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        if let Err(e) = run(&session_ctx, &mut stream, external, internal_rx, &event_tx).await {
            Log::new(Some(&session_ctx.status_tx)).warn(format!("BMP client ended: {e:#}"));
        }
        session_ctx
            .state
            .update_client_status(session_ctx.client_id, ClientStatus::Disconnected)
            .await;
        session_ctx
            .state
            .remove_client_handle(session_ctx.client_id)
            .await;
        let _ = session_ctx.status_tx.send("__UPDATE_UI__".into());
    });
    ctx.state.register_client_task(ctx.client_id, task).await;
    Ok(local)
}

async fn run(
    ctx: &ConnectContext,
    stream: &mut TcpStream,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<Json>,
    events: &mpsc::Sender<Event>,
) -> Result<()> {
    let mut peers = Peers::default();
    let mut reported = 1u64;
    loop {
        let mut scratch = [0u8; 1024];
        let (action, command) = tokio::select! {
            c = external.recv() => match c { Some(c) => (c.action.clone(), Some(c)), None => return Ok(()) },
            a = internal.recv() => match a { Some(a) => (a, None), None => return Ok(()) },
            // A collector sends nothing; reading only notices that it closed.
            n = stream.read(&mut scratch) => {
                if matches!(n, Ok(0) | Err(_)) {
                    events.send(Event::new(&actions::CLOSED_EVENT, json!({"reported": reported}))).await.ok();
                    return Ok(());
                }
                continue;
            }
        };
        let kind = action["type"].as_str().unwrap_or_default().to_owned();
        let outcome = match BmpClientProtocol.execute_action(action.clone()) {
            Err(e) => Ok(ClientSendOutcome::Rejected {
                error: e.to_string(),
            }),
            Ok(ClientActionResult::Disconnect) => {
                if let Some(c) = command {
                    crate::client::command_support::reply(c, Ok(ClientSendOutcome::Disconnected));
                }
                return Ok(());
            }
            Ok(_) => match peers.build(&kind, &action, false) {
                Err(e) => Ok(ClientSendOutcome::Rejected {
                    error: format!("{e:#}"),
                }),
                Ok(bytes) => match stream.write_all(&bytes).await {
                    Ok(()) => {
                        reported += 1;
                        Ok(ClientSendOutcome::Sent {
                            bytes_sent: bytes.len(),
                        })
                    }
                    Err(e) => Err(anyhow::Error::from(e)),
                },
            },
        };
        let failed = outcome.is_err();
        if let Some(c) = command {
            let logged = outcome
                .as_ref()
                .map(|o| serde_json::to_value(o).unwrap_or(Json::Null))
                .unwrap_or_else(|e| json!({"error": e.to_string()}));
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "BMP",
                    None,
                    "injected_action",
                    json!({"type": kind}),
                    vec![logged],
                )
                .await;
            crate::client::command_support::reply(c, outcome);
        } else if let Ok(ClientSendOutcome::Rejected { error }) = &outcome {
            Log::new(Some(&ctx.status_tx)).warn(format!("BMP {kind} refused: {error}"));
        }
        if failed {
            bail!("the collector connection failed");
        }
        if kind == "bmp_termination" {
            stream.shutdown().await.ok();
            return Ok(());
        }
    }
}
