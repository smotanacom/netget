//! RTP endpoint (client role): sends synthesized G.711 to `remote_addr` in paced 20 ms frames,
//! receives on `listen`, and summarises what arrives per SSRC — one event when a stream
//! starts, one when it ends — rather than per packet, which at 50 packets a second would be
//! a model call every 20 ms.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::rtp::media::{self, AudioCodec};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::RtpClientProtocol;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

/// A handler chain (answer → event → answer …) stops after this many follow-ups.
pub const MAX_FOLLOWUP_DEPTH: usize = 8;
/// A stream with nothing for this long has ended.
pub const STREAM_IDLE: Duration = Duration::from_secs(1);
/// Streams tracked at once; past it new SSRCs are ignored until one ends.
pub const MAX_STREAMS: usize = 64;
/// Decoded samples kept per stream for the tone estimate (ten seconds at 8 kHz).
pub const MAX_ANALYSED_SAMPLES: usize = 80_000;
const FRAME: Duration = Duration::from_millis(20);

/// G.711 µ-law to linear PCM (ITU-T G.711).
pub fn ulaw_to_linear(u: u8) -> i16 {
    let u = !u;
    let exponent = (u >> 4) & 0x07;
    let mantissa = i32::from(u & 0x0F);
    let magnitude = (((mantissa << 3) + 0x84) << exponent) - 0x84;
    (if u & 0x80 != 0 { -magnitude } else { magnitude }) as i16
}

/// G.711 A-law to linear PCM (ITU-T G.711).
pub fn alaw_to_linear(a: u8) -> i16 {
    let a = a ^ 0x55;
    let exponent = (a >> 4) & 0x07;
    let mantissa = i32::from(a & 0x0F);
    let magnitude = if exponent == 0 {
        (mantissa << 4) + 8
    } else {
        ((mantissa << 4) + 0x108) << (exponent - 1)
    };
    (if a & 0x80 != 0 { magnitude } else { -magnitude }) as i16
}

/// The dominant frequency of a signal from its zero crossings (with a small hysteresis so
/// quantisation noise around zero does not count), and its RMS level in dBFS.
pub fn analyse(samples: &[i16], rate: f64) -> (Option<f64>, f64) {
    if samples.is_empty() {
        return (None, f64::NEG_INFINITY);
    }
    let rms =
        (samples.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / samples.len() as f64).sqrt();
    let level = 20.0 * (rms / 32768.0).max(1e-9).log10();
    let threshold = (rms * 0.1).max(64.0);
    let (mut crossings, mut state) = (0u64, 0i8);
    for s in samples {
        let v = f64::from(*s);
        let now = if v > threshold {
            1
        } else if v < -threshold {
            -1
        } else {
            state
        };
        if state != 0 && now != state {
            crossings += 1;
        }
        state = now;
    }
    let seconds = samples.len() as f64 / rate;
    let tone = (crossings > 4).then(|| crossings as f64 / 2.0 / seconds);
    (tone, level)
}

struct Stream {
    payload_type: u8,
    first_seq: u16,
    last_seq: u16,
    first_ts: u32,
    last_ts: u32,
    last_frame_len: usize,
    packets: u64,
    octets: u64,
    samples: Vec<i16>,
    last_seen: crate::utils::clock::Instant,
}

/// Inbound RTP accounting per SSRC, shared with the RTSP client: a start event for a new
/// stream, and an end event (loss, length, decoded tone and level) once it goes quiet.
#[derive(Default)]
pub struct Tracker {
    streams: HashMap<u32, Stream>,
}

impl Tracker {
    /// Account one RTP datagram; the `rtp_stream_started` data when it opens a stream.
    pub fn on_rtp(&mut self, data: &[u8], from: SocketAddr) -> Option<Value> {
        let h = media::parse_rtp(data)?;
        let payload = &data[data.len() - h.payload_len..];
        let now = crate::utils::clock::Instant::now();
        let mut started = None;
        if !self.streams.contains_key(&h.ssrc) {
            if self.streams.len() >= MAX_STREAMS {
                return None;
            }
            let codec = match h.payload_type {
                0 => "pcmu",
                8 => "pcma",
                _ => "unknown",
            };
            started = Some(
                json!({"ssrc": h.ssrc, "payload_type": h.payload_type, "codec": codec, "from": from.to_string()}),
            );
            self.streams.insert(
                h.ssrc,
                Stream {
                    payload_type: h.payload_type,
                    first_seq: h.sequence,
                    last_seq: h.sequence,
                    first_ts: h.timestamp,
                    last_ts: h.timestamp,
                    last_frame_len: 0,
                    packets: 0,
                    octets: 0,
                    samples: Vec::new(),
                    last_seen: now,
                },
            );
        }
        if let Some(s) = self.streams.get_mut(&h.ssrc) {
            s.packets += 1;
            s.octets += h.payload_len as u64;
            s.last_seen = now;
            // Sequence numbers wrap; a packet "after" the last within half the space is newer.
            if h.sequence.wrapping_sub(s.last_seq) < 0x8000 {
                s.last_seq = h.sequence;
                s.last_ts = h.timestamp;
                s.last_frame_len = h.payload_len;
            }
            if s.samples.len() < MAX_ANALYSED_SAMPLES {
                let decode: Option<fn(u8) -> i16> = match s.payload_type {
                    0 => Some(ulaw_to_linear),
                    8 => Some(alaw_to_linear),
                    _ => None,
                };
                if let Some(d) = decode {
                    s.samples.extend(payload.iter().map(|b| d(*b)));
                }
            }
        }
        started
    }

    /// The `rtp_stream_ended` data of every stream quiet for `STREAM_IDLE`, which are dropped.
    pub fn sweep(&mut self) -> Vec<Value> {
        let now = crate::utils::clock::Instant::now();
        let done: Vec<u32> = self
            .streams
            .iter()
            .filter(|(_, s)| now.duration_since(s.last_seen) >= STREAM_IDLE)
            .map(|(k, _)| *k)
            .collect();
        done.into_iter()
            .filter_map(|ssrc| self.streams.remove(&ssrc).map(|s| ended(ssrc, &s)))
            .collect()
    }
}

fn ended(ssrc: u32, s: &Stream) -> Value {
    let expected = u64::from(s.last_seq.wrapping_sub(s.first_seq)) + 1;
    let samples = u64::from(s.last_ts.wrapping_sub(s.first_ts)) + s.last_frame_len as u64;
    let mut data = json!({"ssrc": ssrc, "packets": s.packets, "lost": expected.saturating_sub(s.packets),
                          "octets": s.octets, "duration_ms": samples * 1000 / u64::from(media::G711_CLOCK_HZ)});
    if !s.samples.is_empty() {
        let (tone, level) = analyse(&s.samples, f64::from(media::G711_CLOCK_HZ));
        data["tone_hz"] = json!(tone.map(|t| t.round()));
        data["level_dbfs"] = json!((level * 10.0).round() / 10.0);
    }
    data
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let listen = ctx
        .startup_params
        .as_ref()
        .map(|p| p.get_optional_string("listen"))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| actions::DEFAULT_LISTEN.to_string());
    let remote: SocketAddr = tokio::net::lookup_host(&ctx.remote_addr)
        .await
        .with_context(|| format!("cannot resolve {}", ctx.remote_addr))?
        .next()
        .context("remote_addr resolves to nothing")?;
    let socket = Arc::new(
        UdpSocket::bind(&listen)
            .await
            .with_context(|| format!("cannot bind {listen}"))?,
    );
    let local = socket.local_addr()?;
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<(Value, usize)>(32);
    let (event_tx, mut event_rx) = mpsc::channel::<(Event, usize)>(64);
    event_tx.try_send((
        Event::new(
            &actions::READY_EVENT,
            json!({"local_addr": local.to_string(), "remote_addr": remote.to_string()}),
        ),
        0,
    ))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        while let Some((event, depth)) = event_rx.recv().await {
            events_ctx
                .state
                .record_access_log(
                    AccessLogOwner::Client(events_ctx.client_id.as_u32()),
                    "RTP",
                    None,
                    event.id(),
                    event.data.clone(),
                    vec![],
                )
                .await;
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
                &RtpClientProtocol,
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
                        if internal_tx.send((action, depth + 1)).await.is_err() {
                            return;
                        }
                    }
                }
                Err(e) => {
                    Log::new(Some(&events_ctx.status_tx)).warn(format!("RTP client handler: {e}"))
                }
            }
        }
    });
    let dispatcher_abort = dispatcher.abort_handle();
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        session(
            &session_ctx,
            socket,
            remote,
            external,
            internal_rx,
            event_tx,
        )
        .await;
        dispatcher_abort.abort();
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

/// Send one stream, paced at its frame rate; report it when done or cut short.
async fn send_stream(
    socket: Arc<UdpSocket>,
    remote: SocketAddr,
    action: Value,
    depth: usize,
    events: mpsc::Sender<(Event, usize)>,
    cut: tokio::sync::watch::Receiver<u64>,
    generation: u64,
) {
    let codec = action
        .get("payload_type")
        .and_then(Value::as_str)
        .map(AudioCodec::parse)
        .unwrap_or(Ok(AudioCodec::Pcmu))
        .unwrap_or(AudioCodec::Pcmu);
    let Ok(content) = media::parse_audio_content(&action) else {
        return;
    };
    let Ok(bytes) = media::synthesize(
        codec,
        &content,
        action["duration_ms"].as_u64().unwrap_or(1000),
    ) else {
        return;
    };
    let ssrc = action["ssrc"]
        .as_u64()
        .map(|s| s as u32)
        .unwrap_or_else(rand::random);
    let mut packetizer = media::RtpPacketizer::new(ssrc, codec.payload_type(), None, None);
    let packets = packetizer.packetize(&bytes, media::G711_SAMPLES_PER_FRAME);
    let mut interval = tokio::time::interval(FRAME);
    let mut sent = 0u64;
    let mut complete = true;
    for p in &packets {
        interval.tick().await;
        if *cut.borrow() != generation {
            complete = false;
            break;
        }
        if socket.send_to(p, remote).await.is_err() {
            complete = false;
            break;
        }
        sent += 1;
    }
    let data =
        json!({"ssrc": ssrc, "packets": sent, "duration_ms": sent * 20, "complete": complete});
    let _ = events.try_send((Event::new(&actions::SENT_EVENT, data), depth));
}

async fn session(
    ctx: &ConnectContext,
    socket: Arc<UdpSocket>,
    remote: SocketAddr,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<(Value, usize)>,
    events: mpsc::Sender<(Event, usize)>,
) {
    let log = Log::new(Some(&ctx.status_tx));
    let mut tracker = Tracker::default();
    let mut buf = vec![0u8; 2048];
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let (cut_tx, cut_rx) = tokio::sync::watch::channel(0u64);
    let mut generation = 0u64;
    loop {
        let (action, depth, mut injected) = tokio::select! {
            r = socket.recv_from(&mut buf) => {
                let Ok((n, from)) = r else { return };
                let data = &buf[..n];
                if media::is_rtcp(data) {
                    let ssrc = data.get(4..8).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]])).unwrap_or(0);
                    let _ = events.try_send((Event::new(&actions::RTCP_EVENT, json!({"packet_type": data[1], "ssrc": ssrc, "from": from.to_string()})), 0));
                    continue;
                }
                if let Some(started) = tracker.on_rtp(data, from) {
                    let _ = events.try_send((Event::new(&actions::STREAM_STARTED_EVENT, started), 0));
                }
                continue;
            }
            _ = tick.tick() => {
                for data in tracker.sweep() {
                    let _ = events.try_send((Event::new(&actions::STREAM_ENDED_EVENT, data), 0));
                }
                continue;
            }
            command = external.recv() => match command {
                Some(c) => (c.action.clone(), 0, Some(c)),
                None => return,
            },
            action = internal.recv() => match action {
                Some((a, depth)) => (a, depth, None),
                None => return,
            },
        };
        let reply = |injected: &mut Option<ClientCommand>, outcome: ClientSendOutcome| {
            if let Some(command) = injected.take() {
                crate::client::command_support::reply(command, Ok(outcome));
            }
        };
        match RtpClientProtocol.execute_action(action.clone()) {
            Ok(ClientActionResult::Disconnect) => {
                let _ = cut_tx.send(u64::MAX);
                reply(&mut injected, ClientSendOutcome::Disconnected);
                return;
            }
            Ok(_) => {}
            Err(e) => {
                log.warn(format!("RTP client action refused: {e}"));
                reply(
                    &mut injected,
                    ClientSendOutcome::Rejected {
                        error: e.to_string(),
                    },
                );
                continue;
            }
        }
        if depth > MAX_FOLLOWUP_DEPTH {
            log.warn(format!(
                "RTP client: handler chain stopped after {MAX_FOLLOWUP_DEPTH} follow-ups"
            ));
            continue;
        }
        if injected.is_some() {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "RTP",
                    None,
                    "injected_action",
                    action.clone(),
                    vec![],
                )
                .await;
        }
        match action["type"].as_str().unwrap_or_default() {
            actions::SEND_AUDIO => {
                generation += 1;
                let _ = cut_tx.send(generation);
                let task = tokio::spawn(send_stream(
                    socket.clone(),
                    remote,
                    action,
                    depth,
                    events.clone(),
                    cut_rx.clone(),
                    generation,
                ));
                ctx.state.register_client_task(ctx.client_id, task).await;
                reply(
                    &mut injected,
                    ClientSendOutcome::Executed {
                        detail: "streaming".into(),
                    },
                );
            }
            _ => {
                let sr = media::build_rtcp_sender_report(
                    action["ssrc"]
                        .as_u64()
                        .map(|s| s as u32)
                        .unwrap_or_else(rand::random),
                    action["rtp_timestamp"].as_u64().unwrap_or(0) as u32,
                    action["packet_count"].as_u64().unwrap_or(0) as u32,
                    action["octet_count"].as_u64().unwrap_or(0) as u32,
                );
                let outcome = match socket.send_to(&sr, remote).await {
                    Ok(n) => ClientSendOutcome::Sent { bytes_sent: n },
                    Err(e) => ClientSendOutcome::Rejected {
                        error: e.to_string(),
                    },
                };
                reply(&mut injected, outcome);
            }
        }
    }
}
