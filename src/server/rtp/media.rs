//! RTP media synthesis and packetization (RFC 3550).
//!
//! This is the VNC-of-audio: the model never produces samples or bytes. It answers an event
//! with a *structured description* of what a stream should carry — a tone at a frequency, a
//! run of DTMF digits, silence — and this module turns that description into G.711 (PCMU/PCMA)
//! samples and frames them into correct RTP packets. The one escape hatch for genuinely
//! arbitrary payloads is an explicit `encoding: "hex"` field the caller decodes here, mirroring
//! the `send_tcp_data` reference fix — never sniffed, always declared.
//!
//! Shared by both the bare `rtp` server and the `rtsp` control server (whose PLAY streams RTP).

use anyhow::{bail, Context, Result};

/// G.711 sample rate. Both PCMU (PT 0) and PCMA (PT 8) are 8 kHz, 8 bits/sample, mono.
pub const G711_CLOCK_HZ: u32 = 8000;

/// Samples per 20 ms RTP frame at 8 kHz — the near-universal ptime for G.711.
pub const G711_SAMPLES_PER_FRAME: usize = 160;

/// Upper bound on synthesized duration, so a single action cannot enqueue an unbounded burst
/// of datagrams onto the (unbounded, backpressure-free) socket.
pub const MAX_DURATION_MS: u64 = 30_000;

/// A static RTP payload type from the RTP/AVP profile (RFC 3551) that this engine can synthesize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    /// PCMU / G.711 µ-law, payload type 0.
    Pcmu,
    /// PCMA / G.711 A-law, payload type 8.
    Pcma,
}

impl AudioCodec {
    /// Parse the `payload_type` field. Accepts the codec name or its numeric PT.
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "pcmu" | "mulaw" | "ulaw" | "g711u" | "0" => Ok(Self::Pcmu),
            "pcma" | "alaw" | "g711a" | "8" => Ok(Self::Pcma),
            other => bail!(
                "unsupported payload_type {other:?}; this engine synthesizes only \"pcmu\" (PT 0) \
                 or \"pcma\" (PT 8)"
            ),
        }
    }

    /// The static RTP/AVP payload type number.
    pub fn payload_type(self) -> u8 {
        match self {
            Self::Pcmu => 0,
            Self::Pcma => 8,
        }
    }

    /// The `a=rtpmap` encoding name used in SDP.
    pub fn rtpmap_name(self) -> &'static str {
        match self {
            Self::Pcmu => "PCMU",
            Self::Pcma => "PCMA",
        }
    }

    /// Encode one 16-bit linear PCM sample to this codec's single byte.
    pub fn encode_sample(self, pcm: i16) -> u8 {
        match self {
            Self::Pcmu => linear_to_ulaw(pcm),
            Self::Pcma => linear_to_alaw(pcm),
        }
    }
}

/// What a stream should carry, as decided by the model. This is the structured description that
/// stands in for raw bytes.
#[derive(Debug, Clone)]
pub enum AudioContent {
    /// A pure sine tone at the given frequency (Hz).
    Tone { hz: f64 },
    /// A run of DTMF digits (0-9, *, #, A-D), 150 ms tone + 50 ms gap each, classic dual-tone.
    Dtmf { digits: String },
    /// Digital silence (all-zero PCM).
    Silence,
    /// Caller-supplied, already-encoded codec bytes. `encoding` was `"hex"`; decoded here.
    /// Used only when the content is genuinely not describable as tone/dtmf/silence.
    Raw { encoded: Vec<u8> },
}

/// Parse an `AudioContent` from an action object's fields.
///
/// Recognised shapes (checked in order):
/// - `{"content":"tone","tone_hz":440}`
/// - `{"content":"dtmf","digits":"123#"}`
/// - `{"content":"silence"}`
/// - `{"content":"raw","encoding":"hex","samples":"ff7e..."}`
///
/// A bare `{"tone_hz":440}` is also accepted as a tone for convenience.
pub fn parse_audio_content(action: &serde_json::Value) -> Result<AudioContent> {
    let kind = action
        .get("content")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_ascii_lowercase());

    match kind.as_deref() {
        Some("tone") | None => {
            let hz = action
                .get("tone_hz")
                .and_then(|v| v.as_f64())
                .unwrap_or(440.0);
            if !(20.0..=3800.0).contains(&hz) {
                bail!("tone_hz {hz} is outside the 20-3800 Hz range representable at 8 kHz");
            }
            Ok(AudioContent::Tone { hz })
        }
        Some("dtmf") => {
            let digits = action
                .get("digits")
                .and_then(|v| v.as_str())
                .context("content \"dtmf\" requires a \"digits\" string")?
                .to_string();
            if digits.is_empty() {
                bail!("\"digits\" must be non-empty for dtmf content");
            }
            Ok(AudioContent::Dtmf { digits })
        }
        Some("silence") => Ok(AudioContent::Silence),
        Some("raw") => {
            let encoding = action
                .get("encoding")
                .and_then(|v| v.as_str())
                .unwrap_or("hex");
            if encoding != "hex" {
                bail!(
                    "raw content encoding must be \"hex\" (got {encoding:?}); raw bytes are only \
                     accepted hex-encoded and are decoded before framing"
                );
            }
            let hexstr = action
                .get("samples")
                .and_then(|v| v.as_str())
                .context("raw content requires a hex \"samples\" string")?;
            let encoded = hex::decode(hexstr.trim())
                .context("raw \"samples\" is not valid hex; the engine decodes it for real")?;
            if encoded.is_empty() {
                bail!("decoded raw \"samples\" is empty");
            }
            Ok(AudioContent::Raw { encoded })
        }
        Some(other) => {
            bail!("unknown content {other:?}; valid: \"tone\", \"dtmf\", \"silence\", \"raw\"")
        }
    }
}

/// Synthesize codec bytes (one per sample) for a description and duration.
///
/// For `Raw`, the bytes are already codec-encoded and returned verbatim (duration ignored).
pub fn synthesize(codec: AudioCodec, content: &AudioContent, duration_ms: u64) -> Result<Vec<u8>> {
    if duration_ms == 0 {
        bail!("duration_ms must be greater than zero");
    }
    if duration_ms > MAX_DURATION_MS {
        bail!("duration_ms {duration_ms} exceeds the {MAX_DURATION_MS} ms cap");
    }
    let total = (G711_CLOCK_HZ as u64 * duration_ms / 1000) as usize;

    let pcm: Vec<i16> = match content {
        AudioContent::Raw { encoded } => return Ok(encoded.clone()),
        AudioContent::Silence => vec![0i16; total],
        AudioContent::Tone { hz } => (0..total)
            .map(|n| {
                let t = n as f64 / G711_CLOCK_HZ as f64;
                (8000.0 * (2.0 * std::f64::consts::PI * hz * t).sin()) as i16
            })
            .collect(),
        AudioContent::Dtmf { digits } => synthesize_dtmf(digits),
    };
    // DTMF length comes from the digit count (200 ms each), not from `duration_ms`, so the cap
    // above does not constrain it. Without this a `digits` string a handler or a script can
    // make arbitrarily long allocates without bound — 30 s is 150 digits, and the same 30 s
    // ceiling should mean the same thing however the content was described.
    let max_samples = (G711_CLOCK_HZ as u64 * MAX_DURATION_MS / 1000) as usize;
    if pcm.len() > max_samples {
        bail!(
            "dtmf digits synthesize to {} ms, past the {} ms cap ({} digits at {} ms each is the \
             most one action may stream)",
            pcm.len() * 1000 / G711_CLOCK_HZ as usize,
            MAX_DURATION_MS,
            max_samples / ((G711_CLOCK_HZ as usize * DTMF_DIGIT_MS) / 1000),
            DTMF_DIGIT_MS
        );
    }

    Ok(pcm.into_iter().map(|s| codec.encode_sample(s)).collect())
}

/// Wall-clock length of one DTMF digit: 150 ms of tone plus a 50 ms gap.
pub const DTMF_DIGIT_MS: usize = 200;

/// DTMF: each digit is the sum of a low and a high frequency, 150 ms on + 50 ms off.
fn synthesize_dtmf(digits: &str) -> Vec<i16> {
    let mut out = Vec::new();
    let on = (G711_CLOCK_HZ as usize * 150) / 1000;
    let off = (G711_CLOCK_HZ as usize * 50) / 1000;
    for ch in digits.chars() {
        if let Some((low, high)) = dtmf_freqs(ch) {
            for n in 0..on {
                let t = n as f64 / G711_CLOCK_HZ as f64;
                let s = 4000.0
                    * ((2.0 * std::f64::consts::PI * low * t).sin()
                        + (2.0 * std::f64::consts::PI * high * t).sin());
                out.push(s as i16);
            }
            out.extend(std::iter::repeat(0i16).take(off));
        }
    }
    out
}

fn dtmf_freqs(ch: char) -> Option<(f64, f64)> {
    let (row, col) = match ch.to_ascii_uppercase() {
        '1' => (697.0, 1209.0),
        '2' => (697.0, 1336.0),
        '3' => (697.0, 1477.0),
        'A' => (697.0, 1633.0),
        '4' => (770.0, 1209.0),
        '5' => (770.0, 1336.0),
        '6' => (770.0, 1477.0),
        'B' => (770.0, 1633.0),
        '7' => (852.0, 1209.0),
        '8' => (852.0, 1336.0),
        '9' => (852.0, 1477.0),
        'C' => (852.0, 1633.0),
        '*' => (941.0, 1209.0),
        '0' => (941.0, 1336.0),
        '#' => (941.0, 1477.0),
        'D' => (941.0, 1633.0),
        _ => return None,
    };
    Some((row, col))
}

/// Stateful RTP packetizer (RFC 3550 §5.1). Holds the running sequence number and timestamp so
/// that successive bursts on one stream stay monotonic, as a real sender must.
#[derive(Debug, Clone)]
pub struct RtpPacketizer {
    ssrc: u32,
    payload_type: u8,
    sequence: u16,
    timestamp: u32,
}

impl RtpPacketizer {
    /// New packetizer. Initial sequence and timestamp are randomized per RFC 3550 §5.1 unless a
    /// value is supplied (tests pin them for determinism).
    pub fn new(
        ssrc: u32,
        payload_type: u8,
        initial_seq: Option<u16>,
        initial_ts: Option<u32>,
    ) -> Self {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        Self {
            ssrc,
            payload_type,
            sequence: initial_seq.unwrap_or_else(|| rng.gen()),
            timestamp: initial_ts.unwrap_or_else(|| rng.gen()),
        }
    }

    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }
    pub fn next_sequence(&self) -> u16 {
        self.sequence
    }
    pub fn next_timestamp(&self) -> u32 {
        self.timestamp
    }

    /// Split codec `payload` into `samples_per_frame`-sized RTP packets. The first packet of the
    /// burst carries the marker bit (RFC 3551: start of a talkspurt). Advances internal seq/ts.
    pub fn packetize(&mut self, payload: &[u8], samples_per_frame: usize) -> Vec<Vec<u8>> {
        let mut packets = Vec::new();
        let mut first = true;
        for chunk in payload.chunks(samples_per_frame.max(1)) {
            let pkt = build_rtp_packet(
                self.payload_type,
                first,
                self.sequence,
                self.timestamp,
                self.ssrc,
                chunk,
            );
            packets.push(pkt);
            self.sequence = self.sequence.wrapping_add(1);
            self.timestamp = self.timestamp.wrapping_add(chunk.len() as u32);
            first = false;
        }
        packets
    }
}

/// Build one RTP packet: 12-byte fixed header (RFC 3550 §5.1) followed by the payload.
pub fn build_rtp_packet(
    payload_type: u8,
    marker: bool,
    sequence: u16,
    timestamp: u32,
    ssrc: u32,
    payload: &[u8],
) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(12 + payload.len());
    // V=2, P=0, X=0, CC=0
    pkt.push(0x80);
    // M | PT
    pkt.push(((marker as u8) << 7) | (payload_type & 0x7F));
    pkt.extend_from_slice(&sequence.to_be_bytes());
    pkt.extend_from_slice(&timestamp.to_be_bytes());
    pkt.extend_from_slice(&ssrc.to_be_bytes());
    pkt.extend_from_slice(payload);
    pkt
}

/// Parsed view of an inbound RTP packet header.
#[derive(Debug, Clone)]
pub struct ParsedRtp {
    pub version: u8,
    pub marker: bool,
    pub payload_type: u8,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub payload_len: usize,
}

/// Parse the fixed RTP header. Returns None if the buffer is too short or not version 2.
pub fn parse_rtp(data: &[u8]) -> Option<ParsedRtp> {
    if data.len() < 12 {
        return None;
    }
    let version = data[0] >> 6;
    if version != 2 {
        return None;
    }
    let cc = (data[0] & 0x0F) as usize;
    let header_len = 12 + cc * 4;
    if data.len() < header_len {
        return None;
    }
    Some(ParsedRtp {
        version,
        marker: data[1] & 0x80 != 0,
        payload_type: data[1] & 0x7F,
        sequence: u16::from_be_bytes([data[2], data[3]]),
        timestamp: u32::from_be_bytes([data[4], data[5], data[6], data[7]]),
        ssrc: u32::from_be_bytes([data[8], data[9], data[10], data[11]]),
        payload_len: data.len() - header_len,
    })
}

/// True if a datagram is an RTCP packet rather than RTP. RTCP packet types occupy 200-204,
/// which the RTP profile reserves from the payload-type space so the two never collide on a
/// muxed port (RFC 5761 §4).
pub fn is_rtcp(data: &[u8]) -> bool {
    data.len() >= 2 && data[0] >> 6 == 2 && (200..=204).contains(&data[1])
}

/// Build a minimal RTCP Sender Report (RFC 3550 §6.4.1) with no reception report blocks.
pub fn build_rtcp_sender_report(
    ssrc: u32,
    rtp_timestamp: u32,
    packet_count: u32,
    octet_count: u32,
) -> Vec<u8> {
    // NTP timestamp: seconds since 1900 + fraction. 1900->1970 is 2_208_988_800 s.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let ntp_secs = now.as_secs() + 2_208_988_800;
    let ntp_frac = ((now.subsec_nanos() as u64) << 32) / 1_000_000_000;

    let mut pkt = Vec::with_capacity(28);
    pkt.push(0x80); // V=2, P=0, RC=0
    pkt.push(200); // PT = SR
    pkt.extend_from_slice(&6u16.to_be_bytes()); // length in 32-bit words minus one (28 bytes)
    pkt.extend_from_slice(&ssrc.to_be_bytes());
    pkt.extend_from_slice(&(ntp_secs as u32).to_be_bytes());
    pkt.extend_from_slice(&(ntp_frac as u32).to_be_bytes());
    pkt.extend_from_slice(&rtp_timestamp.to_be_bytes());
    pkt.extend_from_slice(&packet_count.to_be_bytes());
    pkt.extend_from_slice(&octet_count.to_be_bytes());
    pkt
}

// --- G.711 encoders (ITU-T G.711, standard reference implementation) ---

/// Linear 16-bit PCM to G.711 µ-law.
pub fn linear_to_ulaw(sample: i16) -> u8 {
    const BIAS: i32 = 0x84;
    const CLIP: i32 = 32635;
    let mut pcm = sample as i32;
    let sign = if pcm < 0 {
        pcm = -pcm;
        0x80
    } else {
        0
    };
    if pcm > CLIP {
        pcm = CLIP;
    }
    pcm += BIAS;
    let mut exponent = 7;
    let mut mask = 0x4000;
    while exponent > 0 && (pcm & mask) == 0 {
        exponent -= 1;
        mask >>= 1;
    }
    let mantissa = (pcm >> (exponent + 3)) & 0x0F;
    let ulaw = !(sign | (exponent << 4) | mantissa);
    (ulaw & 0xFF) as u8
}

/// Linear 16-bit PCM to G.711 A-law.
pub fn linear_to_alaw(sample: i16) -> u8 {
    let mut pcm = sample as i32;
    let sign = if pcm >= 0 { 0x80 } else { 0 };
    if sign == 0 {
        pcm = -pcm;
    }
    if pcm > 32635 {
        pcm = 32635;
    }
    let (exponent, mantissa) = if pcm >= 256 {
        let mut exponent = 7;
        let mut mask = 0x4000;
        while exponent > 1 && (pcm & mask) == 0 {
            exponent -= 1;
            mask >>= 1;
        }
        (exponent, (pcm >> (exponent + 3)) & 0x0F)
    } else {
        (0, (pcm >> 4) & 0x0F)
    };
    let alaw = (sign | (exponent << 4) | mantissa) as u8;
    alaw ^ 0x55
}
