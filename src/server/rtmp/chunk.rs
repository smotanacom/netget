//! RTMP handshake and chunk stream (Adobe RTMP 1.0 §5.2–5.4): the simple version-3 handshake,
//! chunk basic and message headers in all four formats with extended timestamps, message
//! reassembly per chunk stream, and a chunk writer.
use anyhow::{bail, ensure, Context, Result};
use std::collections::HashMap;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const HANDSHAKE_SIZE: usize = 1536;
/// Largest message accepted (a keyframe of a high-bitrate stream fits comfortably).
pub const MAX_MESSAGE: usize = 8 * 1024 * 1024;
/// Largest chunk size either side may set.
pub const MAX_CHUNK_SIZE: usize = 1024 * 1024;
const MAX_CHUNK_STREAMS: usize = 64;

pub const SET_CHUNK_SIZE: u8 = 1;
pub const ABORT: u8 = 2;
pub const ACK: u8 = 3;
pub const USER_CONTROL: u8 = 4;
pub const WINDOW_ACK_SIZE: u8 = 5;
pub const SET_PEER_BANDWIDTH: u8 = 6;
pub const AUDIO: u8 = 8;
pub const VIDEO: u8 = 9;
pub const DATA_AMF3: u8 = 15;
pub const COMMAND_AMF3: u8 = 17;
pub const DATA_AMF0: u8 = 18;
pub const COMMAND_AMF0: u8 = 20;

/// A reassembled RTMP message.
#[derive(Debug, Clone)]
pub struct Message {
    pub type_id: u8,
    pub stream_id: u32,
    pub timestamp: u32,
    pub payload: Vec<u8>,
}

fn random_block() -> Vec<u8> {
    use ring::rand::SecureRandom;
    let mut b = vec![0u8; HANDSHAKE_SIZE];
    let _ = ring::rand::SystemRandom::new().fill(&mut b[8..]);
    b[..8].fill(0);
    b
}

/// Server side: C0+C1 in, S0+S1+S2 out, C2 in.
pub async fn accept<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S) -> Result<()> {
    let mut c0c1 = vec![0u8; 1 + HANDSHAKE_SIZE];
    s.read_exact(&mut c0c1).await?;
    ensure!(c0c1[0] == 3, "RTMP version {} is not 3", c0c1[0]);
    let mut out = vec![3u8];
    out.extend(random_block());
    out.extend_from_slice(&c0c1[1..]);
    s.write_all(&out).await?;
    let mut c2 = vec![0u8; HANDSHAKE_SIZE];
    s.read_exact(&mut c2).await?;
    Ok(())
}

/// Client side: C0+C1 out, S0+S1+S2 in, C2 (an echo of S1) out.
pub async fn connect<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S) -> Result<()> {
    let mut out = vec![3u8];
    out.extend(random_block());
    s.write_all(&out).await?;
    let mut s0s1s2 = vec![0u8; 1 + 2 * HANDSHAKE_SIZE];
    s.read_exact(&mut s0s1s2).await?;
    ensure!(
        s0s1s2[0] == 3,
        "the server answered RTMP version {}",
        s0s1s2[0]
    );
    s.write_all(&s0s1s2[1..1 + HANDSHAKE_SIZE]).await?;
    Ok(())
}

#[derive(Default, Clone)]
struct InStream {
    timestamp: u32,
    delta: u32,
    length: usize,
    type_id: u8,
    stream_id: u32,
    extended: bool,
    buf: Vec<u8>,
}

/// Reassembles messages from chunks.
pub struct Reader {
    pub chunk_size: usize,
    streams: HashMap<u32, InStream>,
    /// Bytes read so far, for acknowledgements.
    pub bytes: u64,
}

impl Default for Reader {
    fn default() -> Self {
        Self {
            chunk_size: 128,
            streams: HashMap::new(),
            bytes: 0,
        }
    }
}

impl Reader {
    async fn u8<R: AsyncRead + Unpin>(&mut self, r: &mut R) -> Result<u8> {
        self.bytes += 1;
        Ok(r.read_u8().await?)
    }
    async fn be(&mut self, r: &mut (impl AsyncRead + Unpin), n: usize) -> Result<u32> {
        let mut b = [0u8; 4];
        r.read_exact(&mut b[4 - n..]).await?;
        self.bytes += n as u64;
        Ok(u32::from_be_bytes(b))
    }

    pub fn abort(&mut self, csid: u32) {
        if let Some(s) = self.streams.get_mut(&csid) {
            s.buf.clear();
        }
    }

    /// The next complete message.
    pub async fn read<R: AsyncRead + Unpin>(&mut self, r: &mut R) -> Result<Message> {
        loop {
            let b0 = self.u8(r).await?;
            let fmt = b0 >> 6;
            let csid = match b0 & 0x3f {
                0 => self.u8(r).await? as u32 + 64,
                1 => {
                    let lo = self.u8(r).await? as u32;
                    let hi = self.u8(r).await? as u32;
                    hi * 256 + lo + 64
                }
                n => n as u32,
            };
            if !self.streams.contains_key(&csid) {
                ensure!(
                    self.streams.len() < MAX_CHUNK_STREAMS,
                    "more than {MAX_CHUNK_STREAMS} chunk streams"
                );
                ensure!(
                    fmt == 0,
                    "chunk stream {csid} starts without a type-0 header"
                );
            }
            let mut st = self.streams.remove(&csid).unwrap_or_default();
            let starting = st.buf.is_empty();
            match fmt {
                0 => {
                    let ts = self.be(r, 3).await?;
                    st.length = self.be(r, 3).await? as usize;
                    st.type_id = self.u8(r).await?;
                    let mut sid = [0u8; 4];
                    r.read_exact(&mut sid).await?;
                    self.bytes += 4;
                    st.stream_id = u32::from_le_bytes(sid);
                    st.extended = ts == 0xFF_FFFF;
                    st.timestamp = if st.extended {
                        self.be(r, 4).await?
                    } else {
                        ts
                    };
                    st.delta = 0;
                    ensure!(
                        starting,
                        "type-0 header in the middle of a message on chunk stream {csid}"
                    );
                }
                1 | 2 => {
                    let d = self.be(r, 3).await?;
                    if fmt == 1 {
                        st.length = self.be(r, 3).await? as usize;
                        st.type_id = self.u8(r).await?;
                    }
                    st.extended = d == 0xFF_FFFF;
                    st.delta = if st.extended { self.be(r, 4).await? } else { d };
                    ensure!(
                        starting,
                        "type-{fmt} header in the middle of a message on chunk stream {csid}"
                    );
                    st.timestamp = st.timestamp.wrapping_add(st.delta);
                }
                _ => {
                    if st.extended {
                        self.be(r, 4).await?;
                    }
                    if starting {
                        st.timestamp = st.timestamp.wrapping_add(st.delta);
                    }
                }
            }
            ensure!(
                st.length <= MAX_MESSAGE,
                "message of {} bytes exceeds {MAX_MESSAGE}",
                st.length
            );
            let n = (st.length - st.buf.len()).min(self.chunk_size);
            let start = st.buf.len();
            st.buf.resize(start + n, 0);
            r.read_exact(&mut st.buf[start..]).await?;
            self.bytes += n as u64;
            if st.buf.len() >= st.length {
                let payload = std::mem::take(&mut st.buf);
                let msg = Message {
                    type_id: st.type_id,
                    stream_id: st.stream_id,
                    timestamp: st.timestamp,
                    payload,
                };
                self.streams.insert(csid, st);
                return Ok(msg);
            }
            self.streams.insert(csid, st);
        }
    }

    /// Apply a peer's Set Chunk Size.
    pub fn set_chunk_size(&mut self, payload: &[u8]) -> Result<()> {
        let v = u32::from_be_bytes(
            payload
                .get(..4)
                .context("short Set Chunk Size")?
                .try_into()?,
        ) & 0x7FFF_FFFF;
        ensure!(
            (1..=MAX_CHUNK_SIZE as u32).contains(&v),
            "chunk size {v} out of bounds"
        );
        self.chunk_size = v as usize;
        Ok(())
    }
}

/// Splits messages into chunks.
pub struct Writer {
    pub chunk_size: usize,
}

impl Default for Writer {
    fn default() -> Self {
        Self { chunk_size: 128 }
    }
}

fn basic_header(fmt: u8, csid: u32, out: &mut Vec<u8>) {
    match csid {
        2..=63 => out.push((fmt << 6) | csid as u8),
        64..=319 => out.extend([fmt << 6, (csid - 64) as u8]),
        _ => out.extend([
            (fmt << 6) | 1,
            ((csid - 64) & 0xff) as u8,
            ((csid - 64) >> 8) as u8,
        ]),
    }
}

impl Writer {
    pub fn encode(&self, csid: u32, m: &Message) -> Result<Vec<u8>> {
        if m.payload.len() > 0xFF_FFFF {
            bail!("message too long for RTMP");
        }
        let extended = m.timestamp >= 0xFF_FFFF;
        let mut out =
            Vec::with_capacity(m.payload.len() + 16 + m.payload.len() / self.chunk_size.max(1) * 5);
        basic_header(0, csid, &mut out);
        let ts = if extended { 0xFF_FFFF } else { m.timestamp };
        out.extend(&ts.to_be_bytes()[1..]);
        out.extend(&(m.payload.len() as u32).to_be_bytes()[1..]);
        out.push(m.type_id);
        out.extend(m.stream_id.to_le_bytes());
        if extended {
            out.extend(m.timestamp.to_be_bytes());
        }
        for (i, chunk) in m.payload.chunks(self.chunk_size.max(1)).enumerate() {
            if i > 0 {
                basic_header(3, csid, &mut out);
                if extended {
                    out.extend(m.timestamp.to_be_bytes());
                }
            }
            out.extend(chunk);
        }
        Ok(out)
    }
}

pub fn control(type_id: u8, payload: Vec<u8>) -> Message {
    Message {
        type_id,
        stream_id: 0,
        timestamp: 0,
        payload,
    }
}

/// User Control event (type 4): StreamBegin = 0, StreamEOF = 1, PingRequest = 6, PingResponse = 7.
pub fn user_control(event: u16, data: u32) -> Message {
    let mut p = event.to_be_bytes().to_vec();
    p.extend(data.to_be_bytes());
    control(USER_CONTROL, p)
}
