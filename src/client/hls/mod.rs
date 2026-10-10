//! HLS client (RFC 8216): playlists fetched over HTTP and parsed for the model, segments
//! fetched and analysed (MPEG-TS PAT/PMT, PES timestamps, continuity counters), and whole
//! play runs — a variant chosen, a live playlist reloaded as it slides — summarised in one
//! event. Every URL a playlist names is resolved against it and must stay on the stream's
//! origin.
pub mod actions;
use crate::client::http_fetch::FetchClient;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::HlsClientProtocol;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::net::SocketAddr;
use std::time::Duration;

/// A handler chain (answer → event → answer …) stops after this many follow-ups.
pub const MAX_FOLLOWUP_DEPTH: usize = 8;
pub const TIMEOUT: Duration = Duration::from_secs(15);
/// A playlist at most.
pub const MAX_PLAYLIST_BYTES: usize = 1024 * 1024;
/// A media segment at most.
pub const MAX_SEGMENT_BYTES: usize = 32 * 1024 * 1024;
/// Segments one `hls_play` may fetch.
pub const MAX_SEGMENTS_PER_PLAY: usize = 100;
pub const DEFAULT_SEGMENTS_PER_PLAY: usize = 10;
/// Segments listed in an `hls_playlist` event; the count says how many there are.
pub const MAX_LISTED_SEGMENTS: usize = 50;
/// Lines a playlist may have.
pub const MAX_PLAYLIST_LINES: usize = 20_000;
/// A live playlist that has not grown after this many reloads ends the run.
pub const MAX_IDLE_RELOADS: usize = 6;
const TS: usize = 188;

/// One `#EXT-X-STREAM-INF` entry.
#[derive(Debug, Clone)]
pub struct Variant {
    pub index: usize,
    pub uri: String,
    pub bandwidth: Option<u64>,
    pub attributes: BTreeMap<String, String>,
}

/// One media segment.
#[derive(Debug, Clone)]
pub struct Segment {
    pub sequence: u64,
    pub uri: String,
    pub duration: f64,
    pub title: String,
}

#[derive(Debug, Clone)]
pub enum Playlist {
    Master {
        variants: Vec<Variant>,
        renditions: Vec<BTreeMap<String, String>>,
    },
    Media {
        target_duration: Option<f64>,
        media_sequence: u64,
        playlist_type: Option<String>,
        endlist: bool,
        segments: Vec<Segment>,
        encryption: Option<String>,
        map: Option<String>,
    },
}

/// `KEY=VALUE,KEY="quoted, value"` as RFC 8216 §4.2 writes attribute lists.
pub fn attributes(list: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut rest = list.trim();
    while !rest.is_empty() {
        let Some((key, after)) = rest.split_once('=') else {
            break;
        };
        let (value, after) = if let Some(quoted) = after.strip_prefix('"') {
            match quoted.split_once('"') {
                Some((v, a)) => (v.to_string(), a),
                None => (quoted.to_string(), ""),
            }
        } else {
            match after.split_once(',') {
                Some((v, a)) => (v.trim().to_string(), a),
                None => (after.trim().to_string(), ""),
            }
        };
        out.insert(key.trim().to_ascii_uppercase(), value);
        rest = after.trim_start_matches(',').trim_start();
    }
    out
}

/// Parse a playlist; URIs are left as written (resolve them with [`resolve`]).
pub fn parse_playlist(text: &str) -> Result<Playlist> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    ensure!(
        lines.next().map(|l| l.trim_start_matches('\u{feff}')) == Some("#EXTM3U"),
        "not a playlist: the first line is not #EXTM3U"
    );
    let mut variants = Vec::new();
    let mut renditions = Vec::new();
    let mut pending_variant: Option<BTreeMap<String, String>> = None;
    let mut target_duration = None;
    let mut media_sequence = 0u64;
    let mut playlist_type = None;
    let mut endlist = false;
    let mut segments = Vec::new();
    let mut encryption = None;
    let mut map = None;
    let mut pending_inf: Option<(f64, String)> = None;
    for (n, line) in lines.enumerate() {
        ensure!(
            n < MAX_PLAYLIST_LINES,
            "playlist longer than {MAX_PLAYLIST_LINES} lines"
        );
        if let Some(tag) = line.strip_prefix('#') {
            let (name, value) = tag.split_once(':').unwrap_or((tag, ""));
            match name {
                "EXT-X-STREAM-INF" => pending_variant = Some(attributes(value)),
                "EXT-X-MEDIA" => renditions.push(attributes(value)),
                "EXT-X-TARGETDURATION" => target_duration = value.trim().parse().ok(),
                "EXT-X-MEDIA-SEQUENCE" => media_sequence = value.trim().parse().unwrap_or(0),
                "EXT-X-PLAYLIST-TYPE" => playlist_type = Some(value.trim().to_string()),
                "EXT-X-ENDLIST" => endlist = true,
                "EXT-X-KEY" => {
                    let method = attributes(value).remove("METHOD").unwrap_or_default();
                    encryption = (method != "NONE" && !method.is_empty()).then_some(method);
                }
                "EXT-X-MAP" => map = attributes(value).remove("URI"),
                "EXTINF" => {
                    let (d, t) = value.split_once(',').unwrap_or((value, ""));
                    let d: f64 = d.trim().parse().context("EXTINF without a duration")?;
                    ensure!(d.is_finite() && d >= 0.0, "EXTINF duration {d}");
                    pending_inf = Some((d, t.trim().to_string()));
                }
                _ => {}
            }
            continue;
        }
        if let Some(attrs) = pending_variant.take() {
            variants.push(Variant {
                index: variants.len(),
                uri: line.to_string(),
                bandwidth: attrs.get("BANDWIDTH").and_then(|b| b.parse().ok()),
                attributes: attrs,
            });
        } else if let Some((duration, title)) = pending_inf.take() {
            segments.push(Segment {
                sequence: media_sequence + segments.len() as u64,
                uri: line.to_string(),
                duration,
                title,
            });
        }
    }
    if !variants.is_empty() {
        ensure!(
            segments.is_empty(),
            "a playlist cannot list both variants and segments"
        );
        return Ok(Playlist::Master {
            variants,
            renditions,
        });
    }
    Ok(Playlist::Media {
        target_duration,
        media_sequence,
        playlist_type,
        endlist,
        segments,
        encryption,
        map,
    })
}

/// The playlist as the `hls_playlist` event reports it.
pub fn describe(playlist: &Playlist) -> Value {
    match playlist {
        Playlist::Master {
            variants,
            renditions,
        } => json!({
            "kind": "master",
            "variants": variants.iter().map(variant_json).collect::<Vec<_>>(),
            "renditions": renditions.iter().map(|r| json!({
                "type": r.get("TYPE"), "group_id": r.get("GROUP-ID"), "name": r.get("NAME"),
                "language": r.get("LANGUAGE"), "uri": r.get("URI"),
            })).collect::<Vec<_>>(),
        }),
        Playlist::Media {
            target_duration,
            media_sequence,
            playlist_type,
            endlist,
            segments,
            encryption,
            map,
        } => json!({
            "kind": "media",
            "target_duration": target_duration,
            "media_sequence": media_sequence,
            "playlist_type": playlist_type,
            "endlist": endlist,
            "segments": segments.iter().take(MAX_LISTED_SEGMENTS).map(|s| json!({
                "sequence": s.sequence, "uri": s.uri, "duration": s.duration, "title": s.title,
            })).collect::<Vec<_>>(),
            "segment_count": segments.len(),
            "total_duration": (segments.iter().map(|s| s.duration).sum::<f64>() * 1000.0).round() / 1000.0,
            "encryption": encryption,
            "map": map,
        }),
    }
}

fn variant_json(v: &Variant) -> Value {
    json!({
        "index": v.index, "uri": v.uri, "bandwidth": v.bandwidth,
        "average_bandwidth": v.attributes.get("AVERAGE-BANDWIDTH").and_then(|b| b.parse::<u64>().ok()),
        "resolution": v.attributes.get("RESOLUTION"), "codecs": v.attributes.get("CODECS"),
        "frame_rate": v.attributes.get("FRAME-RATE").and_then(|f| f.parse::<f64>().ok()),
    })
}

/// Resolve `uri` against `base` (RFC 3986), refusing any other origin.
pub fn resolve(base: &str, uri: &str) -> Result<String> {
    let base_url = url::Url::parse(base).with_context(|| format!("{base:?} is not a URL"))?;
    let joined = base_url
        .join(uri)
        .with_context(|| format!("{uri:?} is not a URI"))?;
    ensure!(
        joined.origin() == base_url.origin(),
        "{uri:?} is on {}, but this client is bound to {}",
        joined.origin().ascii_serialization(),
        base_url.origin().ascii_serialization()
    );
    ensure!(
        joined.username().is_empty() && joined.password().is_none(),
        "{uri:?} carries credentials"
    );
    Ok(joined.into())
}

/// What one segment holds.
#[derive(Debug, Default, Clone)]
pub struct SegmentReport {
    pub container: &'static str,
    pub ts_packets: u64,
    pub sync_errors: u64,
    pub continuity_errors: u64,
    /// PID → (stream type, codec name)
    pub streams: BTreeMap<u16, (u8, String)>,
    /// Media length by PTS: the widest span any elementary stream covers, in 90 kHz ticks.
    pub pts_span: Option<u64>,
}

impl SegmentReport {
    pub fn duration_ms(&self) -> Option<u64> {
        self.pts_span.map(|t| t / 90)
    }
    pub fn codecs(&self) -> BTreeSet<String> {
        self.streams.values().map(|(_, c)| c.clone()).collect()
    }
    pub fn to_json(&self) -> Value {
        let mut v = json!({"container": self.container});
        if self.container == "mpegts" {
            v["ts_packets"] = json!(self.ts_packets);
            v["sync_errors"] = json!(self.sync_errors);
            v["continuity_errors"] = json!(self.continuity_errors);
            v["streams"] = json!(self
                .streams
                .iter()
                .map(|(pid, (t, c))| json!({"pid": pid, "stream_type": t, "codec": c}))
                .collect::<Vec<_>>());
            v["duration_ms"] = json!(self.duration_ms());
        }
        v
    }
}

/// ISO/IEC 13818-1 stream types the model is likely to meet.
pub fn codec_name(stream_type: u8) -> String {
    match stream_type {
        0x01 => "mpeg1-video".into(),
        0x02 => "mpeg2-video".into(),
        0x03 | 0x04 => "mp3".into(),
        0x0f => "aac".into(),
        0x11 => "aac-latm".into(),
        0x15 => "id3".into(),
        0x1b => "h264".into(),
        0x24 => "hevc".into(),
        0x81 => "ac3".into(),
        0x87 => "eac3".into(),
        other => format!("0x{other:02x}"),
    }
}

/// The 33-bit timestamp of a PES header's PTS field (5 bytes, marker bits interleaved).
fn pts(b: &[u8]) -> u64 {
    (u64::from(b[0] >> 1 & 0x07) << 30)
        | (u64::from(b[1]) << 22)
        | (u64::from(b[2] >> 1) << 15)
        | (u64::from(b[3]) << 7)
        | u64::from(b[4] >> 1)
}

/// Analyse one segment's bytes.
pub fn analyse_segment(data: &[u8]) -> SegmentReport {
    let mut r = SegmentReport::default();
    if data.len() >= 8 && matches!(&data[4..8], b"ftyp" | b"styp" | b"moof" | b"sidx") {
        r.container = "fmp4";
        return r;
    }
    if data.first() != Some(&0x47) {
        r.container = if data.starts_with(b"ID3") {
            "packed"
        } else {
            "unknown"
        };
        return r;
    }
    r.container = "mpegts";
    let mut pmt_pids = BTreeSet::new();
    let mut last_cc: BTreeMap<u16, u8> = BTreeMap::new();
    let mut spans: BTreeMap<u16, (u64, u64)> = BTreeMap::new();
    let chunks = data.chunks(TS);
    for p in chunks {
        if p.len() < TS {
            r.sync_errors += 1;
            continue;
        }
        if p[0] != 0x47 {
            r.sync_errors += 1;
            continue;
        }
        r.ts_packets += 1;
        let pid = (u16::from(p[1] & 0x1f) << 8) | u16::from(p[2]);
        let start = p[1] & 0x40 != 0;
        let afc = (p[3] >> 4) & 0x03;
        let has_payload = afc & 0x01 != 0;
        let cc = p[3] & 0x0f;
        let discontinuity = afc & 0x02 != 0 && p[4] > 0 && p[5] & 0x80 != 0;
        if pid != 0x1fff && has_payload {
            if let Some(prev) = last_cc.insert(pid, cc) {
                if !discontinuity && cc != (prev + 1) & 0x0f && cc != prev {
                    r.continuity_errors += 1;
                }
            }
        }
        if !start || !has_payload {
            continue;
        }
        let mut i = 4;
        if afc & 0x02 != 0 {
            i += 1 + p[4] as usize;
        }
        let Some(payload) = p.get(i..) else { continue };
        if pid == 0 || pmt_pids.contains(&pid) {
            let Some(&pointer) = payload.first() else {
                continue;
            };
            let Some(section) = payload.get(1 + pointer as usize..) else {
                continue;
            };
            if section.len() < 12 {
                continue;
            }
            let length = (((section[1] & 0x0f) as usize) << 8) | section[2] as usize;
            let end = (3 + length).min(section.len()).saturating_sub(4);
            if pid == 0 && section[0] == 0x00 {
                let mut j = 8;
                while j + 4 <= end {
                    let program = u16::from_be_bytes([section[j], section[j + 1]]);
                    let pmt = (u16::from(section[j + 2] & 0x1f) << 8) | u16::from(section[j + 3]);
                    if program != 0 {
                        pmt_pids.insert(pmt);
                    }
                    j += 4;
                }
            } else if section[0] == 0x02 {
                let info = (((section[10] & 0x0f) as usize) << 8) | section[11] as usize;
                let mut j = 12 + info;
                while j + 5 <= end {
                    let kind = section[j];
                    let es_pid =
                        (u16::from(section[j + 1] & 0x1f) << 8) | u16::from(section[j + 2]);
                    r.streams.insert(es_pid, (kind, codec_name(kind)));
                    let es_info =
                        (((section[j + 3] & 0x0f) as usize) << 8) | section[j + 4] as usize;
                    j += 5 + es_info;
                }
            }
        } else if payload.len() >= 14
            && payload[..3] == [0, 0, 1]
            && payload[7] & 0x80 != 0
            && r.streams.contains_key(&pid)
        {
            let t = pts(&payload[9..14]);
            let e = spans.entry(pid).or_insert((t, t));
            e.0 = e.0.min(t);
            e.1 = e.1.max(t);
        }
    }
    r.pts_span = spans.values().map(|(a, b)| b - a).max();
    r
}

/// The run an `hls_play` makes, accumulated.
#[derive(Default)]
struct Run {
    segments: u64,
    first: Option<u64>,
    last: Option<u64>,
    gaps: u64,
    reloads: u64,
    endlist: bool,
    bytes: u64,
    duration_ms: u64,
    playlist_duration: f64,
    containers: BTreeSet<&'static str>,
    codecs: BTreeSet<String>,
    continuity_errors: u64,
    failed: Vec<Value>,
}

struct Http {
    fetch: FetchClient,
}

impl Http {
    async fn get(&self, url: &str, max: usize) -> Result<(u16, Vec<u8>)> {
        let response = self.fetch.get(url).send().await?;
        let status = response.status().as_u16();
        if let Some(len) = response.content_length() {
            ensure!(len as usize <= max, "{url} is {len} bytes (at most {max})");
        }
        let body = response.bytes().await?;
        ensure!(body.len() <= max, "{url} is longer than {max} bytes");
        Ok((status, body.to_vec()))
    }

    async fn playlist(&self, url: &str) -> Result<(u16, Option<Playlist>)> {
        let (status, body) = self.get(url, MAX_PLAYLIST_BYTES).await?;
        if status != 200 {
            return Ok((status, None));
        }
        let text = String::from_utf8(body).context("the playlist is not UTF-8")?;
        Ok((status, Some(parse_playlist(&text)?)))
    }
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let url = if ctx.remote_addr.contains("://") {
        ctx.remote_addr.clone()
    } else {
        format!("http://{}/index.m3u8", ctx.remote_addr)
    };
    let parsed =
        url::Url::parse(&url).context("remote_addr must be a playlist URL or host:port")?;
    ensure!(
        matches!(parsed.scheme(), "http" | "https"),
        "an HLS stream is an http(s) URL"
    );
    let host = parsed
        .host_str()
        .context("the URL names no host")?
        .to_string();
    let port = parsed.port_or_known_default().unwrap_or(80);
    let timeout = Duration::from_secs(
        ctx.startup_params
            .as_ref()
            .map(|p| p.get_optional_u64("timeout_secs"))
            .transpose()?
            .flatten()
            .unwrap_or(TIMEOUT.as_secs()),
    );
    ensure!(
        (1..=120).contains(&timeout.as_secs()),
        "timeout_secs must be 1..=120"
    );
    crate::client::http_fetch::check_url(&url)?;
    #[cfg(not(target_arch = "wasm32"))]
    let fetch = FetchClient::from_reqwest(
        crate::llm::ollama_client::configured_for_endpoint(
            reqwest::Client::builder()
                .timeout(timeout)
                .redirect(crate::client::http_fetch::same_origin_redirects()),
            &url,
        )
        .build()?,
    );
    #[cfg(target_arch = "wasm32")]
    let fetch = FetchClient::transport(timeout);
    let fetch = fetch
        .with_max_body(MAX_SEGMENT_BYTES)
        .with_user_agent(concat!("NetGet/", env!("CARGO_PKG_VERSION")));
    let local: SocketAddr = match tokio::net::lookup_host((host.as_str(), port)).await?.next() {
        Some(addr) if addr.is_ipv4() => "0.0.0.0:0".parse()?,
        Some(_) => "[::]:0".parse()?,
        None => bail!("HLS server address {host} did not resolve"),
    };
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let result = session(&session_ctx, Http { fetch }, url, external).await;
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("HLS client ended: {e}"));
                ClientStatus::Error(e.to_string())
            }
        };
        session_ctx
            .state
            .update_client_status(session_ctx.client_id, status)
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

/// Ask the handler about an event; its actions come back for the queue at `depth`.
async fn ask(ctx: &ConnectContext, event: &Event) -> Vec<Value> {
    let state = &ctx.state;
    let instruction = state
        .get_instruction_for_client(ctx.client_id)
        .await
        .unwrap_or_default();
    let memory = state
        .get_memory_for_client(ctx.client_id)
        .await
        .unwrap_or_default();
    match call_llm_for_client(
        &ctx.llm_client,
        state,
        ctx.client_id.to_string(),
        &instruction,
        &memory,
        Some(event),
        &HlsClientProtocol,
        &ctx.status_tx,
    )
    .await
    {
        Ok(result) => {
            if let Some(memory) = result.memory_updates {
                state.set_memory_for_client(ctx.client_id, memory).await;
            }
            result.actions
        }
        Err(e) => {
            Log::new(Some(&ctx.status_tx)).warn(format!("HLS client handler: {e}"));
            Vec::new()
        }
    }
}

struct Session {
    http: Http,
    url: String,
    /// The last media playlist read; segment URIs resolve against it.
    media_base: String,
}

async fn session(
    ctx: &ConnectContext,
    http: Http,
    url: String,
    mut external: tokio::sync::mpsc::Receiver<ClientCommand>,
) -> Result<()> {
    let mut s = Session {
        http,
        media_base: url.clone(),
        url,
    };
    let mut queue: VecDeque<(Value, usize)> = VecDeque::new();
    let ready = Event::new(&actions::READY_EVENT, json!({"url": s.url}));
    queue.extend(ask(ctx, &ready).await.into_iter().map(|a| (a, 1)));
    loop {
        let (action, depth, command) = match queue.pop_front() {
            Some((a, d)) => (a, d, None),
            None => match external.recv().await {
                Some(c) => (c.action.clone(), 0, Some(c)),
                None => return Ok(()),
            },
        };
        let checked = HlsClientProtocol.execute_action(action.clone());
        let data = match checked {
            Ok(ClientActionResult::Disconnect) => {
                if let Some(c) = command {
                    crate::client::command_support::reply(c, Ok(ClientSendOutcome::Disconnected));
                }
                return Ok(());
            }
            Ok(ClientActionResult::Custom { data, .. }) => data,
            Ok(_) => continue,
            Err(e) => {
                match command {
                    Some(c) => crate::client::command_support::reply(
                        c,
                        Ok(ClientSendOutcome::Rejected {
                            error: e.to_string(),
                        }),
                    ),
                    None => Log::new(Some(&ctx.status_tx)).warn(format!("HLS action refused: {e}")),
                }
                continue;
            }
        };
        let event = perform(&mut s, &data).await;
        if let Some(c) = command {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "HLS",
                    None,
                    "injected_action",
                    action.clone(),
                    vec![json!({"event": event.event_type.id})],
                )
                .await;
            crate::client::command_support::reply(
                c,
                Ok(ClientSendOutcome::Executed {
                    detail: event.data.to_string(),
                }),
            );
        }
        if depth >= MAX_FOLLOWUP_DEPTH {
            Log::new(Some(&ctx.status_tx)).warn(format!(
                "HLS handler chain stopped after {MAX_FOLLOWUP_DEPTH} follow-ups"
            ));
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "HLS",
                    None,
                    event.event_type.id.as_str(),
                    event.data.clone(),
                    vec![json!({"handled": false, "reason": "follow-up depth"})],
                )
                .await;
            continue;
        }
        let next = ask(ctx, &event).await;
        for a in next.into_iter().rev() {
            queue.push_front((a, depth + 1));
        }
    }
}

fn uri_or<'a>(data: &'a Value, default: &'a str) -> &'a str {
    data.get("uri").and_then(Value::as_str).unwrap_or(default)
}

async fn perform(s: &mut Session, data: &Value) -> Event {
    match data["type"].as_str().unwrap_or_default() {
        actions::GET_PLAYLIST => {
            let target = match resolve(&s.url, uri_or(data, &s.url)) {
                Ok(t) => t,
                Err(e) => {
                    return Event::new(
                        &actions::PLAYLIST_EVENT,
                        json!({"url": uri_or(data, &s.url), "error": e.to_string()}),
                    )
                }
            };
            let mut ev = json!({"url": target});
            match s.http.playlist(&target).await {
                Ok((status, playlist)) => {
                    ev["status"] = json!(status);
                    match playlist {
                        Some(p) => {
                            if matches!(p, Playlist::Media { .. }) {
                                s.media_base = target.clone();
                            }
                            merge(&mut ev, describe(&p));
                        }
                        None => ev["error"] = json!(format!("HTTP {status}")),
                    }
                }
                Err(e) => ev["error"] = json!(format!("{e:#}")),
            }
            Event::new(&actions::PLAYLIST_EVENT, ev)
        }
        actions::GET_SEGMENT => {
            let ev = match resolve(&s.media_base, uri_or(data, "")) {
                Ok(target) => segment_event(&s.http, &target).await.0,
                Err(e) => json!({"url": uri_or(data, ""), "error": e.to_string()}),
            };
            Event::new(&actions::SEGMENT_EVENT, ev)
        }
        _ => {
            let ev = play(s, data).await;
            Event::new(&actions::PLAYED_EVENT, ev)
        }
    }
}

fn merge(into: &mut Value, from: Value) {
    if let (Some(a), Value::Object(b)) = (into.as_object_mut(), from) {
        a.extend(b);
    }
}

async fn segment_event(http: &Http, url: &str) -> (Value, Option<(usize, SegmentReport)>) {
    let mut ev = json!({"url": url});
    match http.get(url, MAX_SEGMENT_BYTES).await {
        Ok((status, body)) => {
            ev["status"] = json!(status);
            if status == 200 {
                let report = analyse_segment(&body);
                ev["bytes"] = json!(body.len());
                merge(&mut ev, report.to_json());
                return (ev, Some((body.len(), report)));
            }
            ev["error"] = json!(format!("HTTP {status}"));
        }
        Err(e) => ev["error"] = json!(format!("{e:#}")),
    }
    (ev, None)
}

async fn play(s: &mut Session, data: &Value) -> Value {
    let start = uri_or(data, &s.url).to_string();
    let max = data
        .get("max_segments")
        .and_then(Value::as_u64)
        .map_or(DEFAULT_SEGMENTS_PER_PLAY, |n| n as usize)
        .min(MAX_SEGMENTS_PER_PLAY);
    let mut out = json!({"url": start});
    let mut run = Run::default();
    let result = play_run(s, data, &start, max, &mut out, &mut run).await;
    if let Err(e) = result {
        out["error"] = json!(format!("{e:#}"));
    }
    merge(
        &mut out,
        json!({
            "segments": run.segments, "first_sequence": run.first, "last_sequence": run.last,
            "gaps": run.gaps, "reloads": run.reloads, "endlist": run.endlist, "bytes": run.bytes,
            "duration_ms": run.duration_ms,
            "playlist_duration": (run.playlist_duration * 1000.0).round() / 1000.0,
            "containers": run.containers, "codecs": run.codecs,
            "continuity_errors": run.continuity_errors, "failed": run.failed,
        }),
    );
    out
}

async fn play_run(
    s: &mut Session,
    data: &Value,
    start: &str,
    max: usize,
    out: &mut Value,
    run: &mut Run,
) -> Result<()> {
    let first_url = resolve(&s.url, start)?;
    out["url"] = json!(first_url);
    let (status, playlist) = s.http.playlist(&first_url).await?;
    let mut playlist = playlist.with_context(|| format!("{first_url}: HTTP {status}"))?;
    let mut media_url = first_url.clone();
    if let Playlist::Master { variants, .. } = &playlist {
        ensure!(
            !variants.is_empty(),
            "the master playlist lists no variants"
        );
        let wanted = data.get("variant").filter(|v| !v.is_null());
        let chosen = match wanted {
            Some(Value::String(w)) if w == "lowest" => {
                variants.iter().min_by_key(|v| v.bandwidth.unwrap_or(0))
            }
            None => variants.iter().max_by_key(|v| v.bandwidth.unwrap_or(0)),
            Some(Value::String(w)) if w == "highest" => {
                variants.iter().max_by_key(|v| v.bandwidth.unwrap_or(0))
            }
            Some(w) => {
                let i = w
                    .as_u64()
                    .or_else(|| w.as_str().and_then(|s| s.parse().ok()))
                    .unwrap_or(u64::MAX) as usize;
                variants.get(i)
            }
        }
        .context("no such variant")?
        .clone();
        out["variant"] = variant_json(&chosen);
        media_url = resolve(&first_url, &chosen.uri)?;
        let (status, media) = s.http.playlist(&media_url).await?;
        playlist = media.with_context(|| format!("{media_url}: HTTP {status}"))?;
        ensure!(
            matches!(playlist, Playlist::Media { .. }),
            "the variant's playlist is another master playlist"
        );
    }
    out["media_playlist"] = json!(media_url);
    s.media_base = media_url.clone();
    let mut next_seq: Option<u64> = None;
    let mut idle = 0usize;
    loop {
        let Playlist::Media {
            target_duration,
            segments,
            endlist,
            encryption,
            ..
        } = &playlist
        else {
            bail!("expected a media playlist");
        };
        run.endlist = *endlist;
        let mut fetched_any = false;
        for seg in segments {
            if next_seq.is_some_and(|n| seg.sequence < n) {
                continue;
            }
            if run.segments as usize + run.failed.len() >= max {
                return Ok(());
            }
            if let Some(n) = next_seq {
                run.gaps += seg.sequence - n;
            }
            next_seq = Some(seg.sequence + 1);
            fetched_any = true;
            run.first.get_or_insert(seg.sequence);
            run.last = Some(seg.sequence);
            run.playlist_duration += seg.duration;
            let url = match resolve(&media_url, &seg.uri) {
                Ok(u) => u,
                Err(e) => {
                    run.failed
                        .push(json!({"uri": seg.uri, "error": e.to_string()}));
                    continue;
                }
            };
            let (ev, report) = segment_event(&s.http, &url).await;
            match report {
                Some((bytes, r)) => {
                    run.segments += 1;
                    run.bytes += bytes as u64;
                    run.containers.insert(r.container);
                    if encryption.is_none() {
                        run.codecs.extend(r.codecs());
                        run.duration_ms += r.duration_ms().unwrap_or(0);
                        run.continuity_errors += r.continuity_errors;
                    }
                }
                None => run
                    .failed
                    .push(json!({"uri": seg.uri, "error": ev["error"]})),
            }
        }
        if *endlist || run.segments as usize + run.failed.len() >= max {
            return Ok(());
        }
        idle = if fetched_any { 0 } else { idle + 1 };
        if idle >= MAX_IDLE_RELOADS {
            bail!("the live playlist stopped growing");
        }
        // RFC 8216 §6.3.4: reload after the target duration (half of it when nothing was new).
        let wait = target_duration.unwrap_or(2.0).clamp(0.5, 30.0);
        let wait = if fetched_any { wait } else { wait / 2.0 };
        tokio::time::sleep(Duration::from_secs_f64(wait)).await;
        run.reloads += 1;
        let (status, again) = s.http.playlist(&media_url).await?;
        playlist = again.with_context(|| format!("{media_url}: HTTP {status} on reload"))?;
    }
}
