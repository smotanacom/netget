//! Owned bounded SFTP v3 requests. No local filesystem or detached session tasks.
//! Wire reference: draft-ietf-secsh-filexfer-02, the version used by OpenSSH.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_PACKET: usize = 64 * 1024;
pub const MAX_PATH: usize = 4096;
pub const MAX_TEXT: usize = 1024 * 1024;
pub const MAX_ENTRIES: usize = 1024;
pub const MAX_CHUNK: usize = 32768;
pub const DEFAULT_READ: usize = 65536;

pub fn validate_action(action: &Value) -> Result<()> {
    let path = action["path"].as_str().context("SFTP needs a path")?;
    ensure!(
        !path.is_empty() && path.len() <= MAX_PATH && !path.contains('\0'),
        "SFTP path must contain 1..4096 bytes without NUL"
    );
    if let Some(offset) = action.get("offset") {
        offset
            .as_u64()
            .context("offset must be an unsigned integer")?;
    }
    if let Some(length) = action.get("length") {
        ensure!(
            length
                .as_u64()
                .is_some_and(|n| n > 0 && n <= MAX_TEXT as u64),
            "length must be 1..1048576"
        );
    }
    if let Some(follow) = action.get("follow_symlinks") {
        ensure!(follow.is_boolean(), "follow_symlinks must be a boolean");
    }
    ensure!(
        action["offset"]
            .as_u64()
            .unwrap_or(0)
            .checked_add(action["length"].as_u64().unwrap_or(DEFAULT_READ as u64))
            .is_some(),
        "SFTP offset overflow"
    );
    Ok(())
}

struct Cursor<'a>(&'a [u8]);
impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        ensure!(n <= self.0.len(), "Truncated SFTP field");
        let (value, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(value)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into()?))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into()?))
    }
    fn string(&mut self, cap: usize) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        ensure!(n <= cap, "SFTP string exceeds bound");
        self.take(n)
    }
    fn text(&mut self) -> Result<String> {
        Ok(std::str::from_utf8(self.string(MAX_PATH)?)?.into())
    }
    fn attrs(&mut self) -> Result<Value> {
        let flags = self.u32()?;
        ensure!(flags & !0x8000000f == 0, "Unknown SFTP attribute flags");
        let mut a = serde_json::Map::new();
        if flags & 1 != 0 {
            a.insert("size".into(), json!(self.u64()?));
        }
        if flags & 2 != 0 {
            a.insert("uid".into(), json!(self.u32()?));
            a.insert("gid".into(), json!(self.u32()?));
        }
        if flags & 4 != 0 {
            let mode = self.u32()?;
            a.insert("permissions".into(), json!(mode));
            a.insert("is_dir".into(), json!(mode & 0o170000 == 0o040000));
            a.insert("is_symlink".into(), json!(mode & 0o170000 == 0o120000));
        }
        if flags & 8 != 0 {
            a.insert("atime".into(), json!(self.u32()?));
            a.insert("mtime".into(), json!(self.u32()?));
        }
        if flags & 0x80000000 != 0 {
            let n = self.u32()? as usize;
            ensure!(
                n <= 64 && n <= self.0.len() / 8,
                "SFTP extended attribute count exceeds bound"
            );
            // Attribute extensions are opaque protocol metadata, never model bytes.
            for _ in 0..n {
                self.string(MAX_PATH)?;
                self.string(MAX_PATH)?;
            }
        }
        Ok(Value::Object(a))
    }
    fn finish(self) -> Result<()> {
        ensure!(self.0.is_empty(), "Trailing SFTP packet bytes");
        Ok(())
    }
}

#[derive(Debug)]
pub enum Reply {
    Status { code: u32, message: String },
    Handle(Vec<u8>),
    Data(Vec<u8>),
    Names(Vec<Value>),
    Attributes(Value),
}
/// Counts and strings are validated before allocating collections or copying data.
pub fn decode_reply(bytes: &[u8], expected_id: u32) -> Result<Reply> {
    ensure!(bytes.len() <= MAX_PACKET, "SFTP packet exceeds 64 KiB");
    let mut c = Cursor(bytes);
    let kind = c.take(1)?[0];
    ensure!(c.u32()? == expected_id, "Mismatched SFTP response ID");
    let reply = match kind {
        101 => {
            let code = c.u32()?;
            ensure!(code <= 8, "Unknown SFTP status");
            let message = c.text()?;
            c.string(MAX_PATH)?;
            Reply::Status { code, message }
        }
        102 => {
            let handle = c.string(256)?;
            ensure!(!handle.is_empty(), "Empty SFTP handle");
            Reply::Handle(handle.to_vec())
        }
        103 => Reply::Data(c.string(MAX_CHUNK)?.to_vec()),
        104 => {
            let n = c.u32()? as usize;
            ensure!(
                n <= MAX_ENTRIES && n <= c.0.len() / 12,
                "SFTP directory entry count exceeds bound"
            );
            let mut entries = Vec::with_capacity(n);
            for _ in 0..n {
                let name = c.text()?;
                c.string(MAX_PATH)?; // Human-readable longname is not a typed attribute.
                entries.push(json!({"name":name,"attributes":c.attrs()?}));
            }
            Reply::Names(entries)
        }
        105 => Reply::Attributes(c.attrs()?),
        _ => bail!("Unexpected SFTP packet type"),
    };
    c.finish()?;
    Ok(reply)
}

pub async fn read_packet<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>> {
    let length = reader.read_u32().await? as usize;
    ensure!(
        (1..=MAX_PACKET).contains(&length),
        "SFTP packet length exceeds 64 KiB"
    );
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    Ok(bytes)
}
fn string(bytes: &[u8]) -> Vec<u8> {
    let mut out = (bytes.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(bytes);
    out
}
struct RawSession<S> {
    stream: S,
    id: u32,
    received: usize,
}
impl<S: AsyncRead + AsyncWrite + Unpin> RawSession<S> {
    async fn receive(&mut self) -> Result<Vec<u8>> {
        let bytes = read_packet(&mut self.stream).await?;
        self.received += bytes.len();
        ensure!(
            self.received <= 2 * MAX_TEXT,
            "SFTP operation packet budget exceeded"
        );
        Ok(bytes)
    }
    async fn send(&mut self, bytes: &[u8]) -> Result<()> {
        ensure!(
            bytes.len() <= MAX_PACKET,
            "SFTP outgoing packet exceeds bound"
        );
        self.stream.write_u32(bytes.len() as u32).await?;
        self.stream.write_all(bytes).await?;
        self.stream.flush().await?;
        Ok(())
    }
    async fn init(&mut self) -> Result<()> {
        self.send(&[1, 0, 0, 0, 3]).await?;
        let bytes = self.receive().await?;
        let mut c = Cursor(&bytes);
        ensure!(
            c.take(1)?[0] == 2 && c.u32()? == 3,
            "SFTP peer must negotiate version 3"
        );
        let mut extensions = 0;
        while !c.0.is_empty() {
            extensions += 1;
            ensure!(extensions <= 64, "Too many SFTP extensions");
            c.string(MAX_PATH)?;
            c.string(MAX_PATH)?;
        }
        Ok(())
    }
    async fn request(&mut self, kind: u8, payload: &[u8]) -> Result<Reply> {
        self.id = self
            .id
            .checked_add(1)
            .context("SFTP request ID exhausted")?;
        let mut out = vec![kind];
        out.extend_from_slice(&self.id.to_be_bytes());
        out.extend_from_slice(payload);
        self.send(&out).await?;
        decode_reply(&self.receive().await?, self.id)
    }
    async fn close(&mut self, handle: &[u8]) -> Result<()> {
        match self.request(4, &string(handle)).await? {
            Reply::Status { code: 0, .. } => Ok(()),
            _ => bail!("SFTP handle close failed"),
        }
    }
}
fn status(reply: Reply) -> Result<Reply> {
    match reply {
        Reply::Status { code, message } => bail!("SFTP status {code}: {message}"),
        reply => Ok(reply),
    }
}
/// Each operation gets its own channel; handles stay inside the exchange and are
/// closed on success. The owner closes the channel on all other outcomes.
pub async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    action: &Value,
) -> Result<Value> {
    validate_action(action)?;
    let mut s = RawSession {
        stream,
        id: 0,
        received: 0,
    };
    s.init().await?;
    let path = action["path"].as_str().context("Missing path")?;
    match action["type"].as_str().unwrap_or("") {
        "sftp_stat" => {
            let kind = if action["follow_symlinks"].as_bool().unwrap_or(false) {
                17
            } else {
                7
            };
            match status(s.request(kind, &string(path.as_bytes())).await?)? {
                Reply::Attributes(attrs) => {
                    Ok(json!({"operation":"stat","path":path,"attributes":attrs}))
                }
                _ => bail!("SFTP stat expected attributes"),
            }
        }
        "sftp_list_directory" => {
            let handle = match status(s.request(11, &string(path.as_bytes())).await?)? {
                Reply::Handle(h) => h,
                _ => bail!("SFTP opendir expected handle"),
            };
            let mut entries = Vec::new();
            let mut empty_batches = 0;
            loop {
                match s.request(12, &string(&handle)).await? {
                    Reply::Status { code: 1, .. } => break,
                    Reply::Names(batch) => {
                        // A peer can return an empty NAME before the final EOF,
                        // including NetGet's semantic empty-directory response.
                        if batch.is_empty() {
                            empty_batches += 1;
                            ensure!(empty_batches <= 16, "SFTP directory made no progress");
                        }
                        ensure!(
                            batch.len() <= MAX_ENTRIES.saturating_sub(entries.len()),
                            "SFTP directory exceeds 1024 entries"
                        );
                        entries.extend(batch);
                    }
                    reply => {
                        status(reply)?;
                        bail!("SFTP readdir expected names");
                    }
                }
            }
            s.close(&handle).await?;
            Ok(json!({"operation":"list_directory","path":path,"entries":entries}))
        }
        "sftp_read_file" => {
            let mut payload = string(path.as_bytes());
            payload.extend_from_slice(&1u32.to_be_bytes()); // SSH_FXF_READ
            payload.extend_from_slice(&0u32.to_be_bytes()); // no invented attributes
            let handle = match status(s.request(3, &payload).await?)? {
                Reply::Handle(h) => h,
                _ => bail!("SFTP open expected handle"),
            };
            let offset = action["offset"].as_u64().unwrap_or(0);
            let length = action["length"].as_u64().unwrap_or(DEFAULT_READ as u64) as usize;
            let mut data = Vec::new();
            let mut eof = false;
            while data.len() < length {
                let count = MAX_CHUNK.min(length - data.len());
                let mut payload = string(&handle);
                payload.extend_from_slice(&(offset + data.len() as u64).to_be_bytes());
                payload.extend_from_slice(&(count as u32).to_be_bytes());
                match s.request(5, &payload).await? {
                    Reply::Status { code: 1, .. } => {
                        eof = true;
                        break;
                    }
                    Reply::Data(chunk) => {
                        ensure!(
                            !chunk.is_empty() && chunk.len() <= count,
                            "SFTP read returned an invalid data length"
                        );
                        data.extend(chunk);
                    }
                    reply => {
                        status(reply)?;
                        bail!("SFTP read expected data");
                    }
                }
            }
            s.close(&handle).await?;
            let bytes = data.len();
            let text = String::from_utf8(data).context("SFTP file window is not UTF-8 text")?;
            Ok(
                json!({"operation":"read_file","path":path,"offset":offset,"bytes_read":bytes,"eof":eof,"text":text}),
            )
        }
        _ => bail!("Unknown SFTP operation"),
    }
}
