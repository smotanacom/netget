//! 9P2000 message codec (Plan 9's file protocol, as plan9port and the Go 9P libraries speak
//! it): little-endian `size[4] type[1] tag[2]` framing, strings with a u16 length, 13-byte
//! qids and the variable-length stat. Shared by the server and the client; every length the
//! peer announces is checked before anything is allocated for it.
use anyhow::{bail, ensure, Context, Result};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};

pub const VERSION: &str = "9P2000";
/// Largest message either side accepts, and the msize NetGet offers and accepts at most.
pub const MAX_MSIZE: u32 = 65536;
/// Smallest msize worth negotiating: room for a header and a little data.
pub const MIN_MSIZE: u32 = 256;
/// Header bytes in an Rread/Twrite before the data: size, type, tag, count (and fid+offset).
pub const IOHDRSZ: u32 = 24;
/// Path elements in one Twalk, as Plan 9's MAXWELEM.
pub const MAX_WELEM: usize = 16;
/// Bytes in one path element.
pub const MAX_NAME: usize = 255;
/// Fids one connection may hold.
pub const MAX_FIDS: usize = 256;
/// Largest file content a handler may supply, and the client accepts.
pub const MAX_CONTENT: usize = 1024 * 1024;
/// Entries in one directory listing.
pub const MAX_ENTRIES: usize = 1024;
/// Per-message read deadline once a message has started, and per-request write deadline.
pub const IO_TIMEOUT: Duration = Duration::from_secs(30);
pub const NOFID: u32 = u32::MAX;
pub const NOTAG: u16 = u16::MAX;

pub const DMDIR: u32 = 0x8000_0000;
pub const QTDIR: u8 = 0x80;
pub const QTFILE: u8 = 0x00;
pub const OREAD: u8 = 0;
pub const OWRITE: u8 = 1;
pub const ORDWR: u8 = 2;
pub const OEXEC: u8 = 3;
pub const OTRUNC: u8 = 0x10;
pub const ORCLOSE: u8 = 0x40;

pub const TVERSION: u8 = 100;
pub const RVERSION: u8 = 101;
pub const TAUTH: u8 = 102;
pub const RAUTH: u8 = 103;
pub const TATTACH: u8 = 104;
pub const RATTACH: u8 = 105;
pub const RERROR: u8 = 107;
pub const TFLUSH: u8 = 108;
pub const RFLUSH: u8 = 109;
pub const TWALK: u8 = 110;
pub const RWALK: u8 = 111;
pub const TOPEN: u8 = 112;
pub const ROPEN: u8 = 113;
pub const TCREATE: u8 = 114;
pub const RCREATE: u8 = 115;
pub const TREAD: u8 = 116;
pub const RREAD: u8 = 117;
pub const TWRITE: u8 = 118;
pub const RWRITE: u8 = 119;
pub const TCLUNK: u8 = 120;
pub const RCLUNK: u8 = 121;
pub const TREMOVE: u8 = 122;
pub const RREMOVE: u8 = 123;
pub const TSTAT: u8 = 124;
pub const RSTAT: u8 = 125;
pub const TWSTAT: u8 = 126;
pub const RWSTAT: u8 = 127;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Qid {
    pub kind: u8,
    pub version: u32,
    pub path: u64,
}

impl Qid {
    pub fn is_dir(&self) -> bool {
        self.kind & QTDIR != 0
    }
}

/// One directory entry, as Tstat/Rstat and directory reads carry it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stat {
    pub kind: u16,
    pub dev: u32,
    pub qid: Qid,
    pub mode: u32,
    pub atime: u32,
    pub mtime: u32,
    pub length: u64,
    pub name: String,
    pub uid: String,
    pub gid: String,
    pub muid: String,
}

impl Stat {
    /// A wstat entry that changes nothing: every field set to its "don't touch" value.
    pub fn dont_touch() -> Self {
        Self {
            kind: u16::MAX,
            dev: u32::MAX,
            qid: Qid {
                kind: u8::MAX,
                version: u32::MAX,
                path: u64::MAX,
            },
            mode: u32::MAX,
            atime: u32::MAX,
            mtime: u32::MAX,
            length: u64::MAX,
            name: String::new(),
            uid: String::new(),
            gid: String::new(),
            muid: String::new(),
        }
    }
}

/// Builds one message body; `finish` prepends size, type and tag.
#[derive(Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn u8(mut self, v: u8) -> Self {
        self.buf.push(v);
        self
    }
    pub fn u16(mut self, v: u16) -> Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u32(mut self, v: u32) -> Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u64(mut self, v: u64) -> Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn string(self, s: &str) -> Self {
        let len = s.len().min(u16::MAX as usize);
        let mut w = self.u16(len as u16);
        w.buf.extend_from_slice(&s.as_bytes()[..len]);
        w
    }
    pub fn bytes(mut self, b: &[u8]) -> Self {
        self.buf.extend_from_slice(b);
        self
    }
    pub fn qid(self, q: &Qid) -> Self {
        self.u8(q.kind).u32(q.version).u64(q.path)
    }
    pub fn stat(self, s: &Stat) -> Self {
        self.bytes(&encode_stat(s))
    }
    pub fn finish(self, kind: u8, tag: u16) -> Vec<u8> {
        let size = (7 + self.buf.len()) as u32;
        let mut out = Vec::with_capacity(size as usize);
        out.extend_from_slice(&size.to_le_bytes());
        out.push(kind);
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&self.buf);
        out
    }
}

/// A stat entry with its own leading size field.
pub fn encode_stat(s: &Stat) -> Vec<u8> {
    let body = Writer::new()
        .u16(s.kind)
        .u32(s.dev)
        .qid(&s.qid)
        .u32(s.mode)
        .u32(s.atime)
        .u32(s.mtime)
        .u64(s.length)
        .string(&s.name)
        .string(&s.uid)
        .string(&s.gid)
        .string(&s.muid)
        .buf;
    let mut out = Vec::with_capacity(body.len() + 2);
    out.extend_from_slice(&(body.len() as u16).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// A cursor over one message body.
pub struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        ensure!(self.buf.len() >= n, "9P message ends inside a field");
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into()?))
    }
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into()?))
    }
    pub fn string(&mut self) -> Result<String> {
        let len = self.u16()? as usize;
        Ok(std::str::from_utf8(self.take(len)?)
            .context("9P string is not UTF-8")?
            .to_string())
    }
    pub fn qid(&mut self) -> Result<Qid> {
        Ok(Qid {
            kind: self.u8()?,
            version: self.u32()?,
            path: self.u64()?,
        })
    }
    pub fn stat(&mut self) -> Result<Stat> {
        let size = self.u16()? as usize;
        let mut r = Reader::new(self.take(size)?);
        Ok(Stat {
            kind: r.u16()?,
            dev: r.u32()?,
            qid: r.qid()?,
            mode: r.u32()?,
            atime: r.u32()?,
            mtime: r.u32()?,
            length: r.u64()?,
            name: r.string()?,
            uid: r.string()?,
            gid: r.string()?,
            muid: r.string()?,
        })
    }
}

/// One message: (type, tag, body). `None` on a clean EOF between messages. The size field is
/// checked against `max` before the body is read; the first byte may wait `idle`, the rest
/// must follow within `IO_TIMEOUT`.
pub async fn read_message<R: AsyncRead + Unpin>(
    reader: &mut R,
    max: u32,
    idle: Duration,
) -> Result<Option<(u8, u16, Vec<u8>)>> {
    let mut size = [0u8; 4];
    match tokio::time::timeout(idle, reader.read(&mut size[..1])).await {
        Ok(Ok(0)) => return Ok(None),
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => bail!("9P peer idle for {}s", idle.as_secs()),
    }
    tokio::time::timeout(IO_TIMEOUT, async {
        reader.read_exact(&mut size[1..]).await?;
        let size = u32::from_le_bytes(size);
        ensure!(size >= 7, "9P message size {size} is below the header");
        ensure!(
            size <= max,
            "9P message of {size} bytes exceeds msize {max}"
        );
        let mut rest = vec![0u8; size as usize - 4];
        reader.read_exact(&mut rest).await?;
        let kind = rest[0];
        let tag = u16::from_le_bytes([rest[1], rest[2]]);
        Ok(Some((kind, tag, rest[3..].to_vec())))
    })
    .await
    .context("9P read deadline")?
}

/// A path element a server may create or walk to: not empty, no '/', not "." and at most
/// `MAX_NAME` bytes. ".." is handled by the walk itself.
pub fn check_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= MAX_NAME
            && !name.contains('/')
            && name != "."
            && !name.contains('\0'),
        "invalid 9P name {name:?}"
    );
    Ok(())
}

/// Stable qid path for a file path: FNV-1a 64.
pub fn qid_path(path: &str) -> u64 {
    path.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Join a parent path and a child name ("/" + "a" = "/a").
pub fn join(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{parent}/{name}")
    }
}

/// The parent of a path ("/a/b" -> "/a", "/a" -> "/", "/" -> "/").
pub fn parent(path: &str) -> String {
    match path.rsplit_once('/') {
        Some(("", _)) | None => "/".into(),
        Some((p, _)) => p.into(),
    }
}

/// The last element of a path ("/" for the root).
pub fn base(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((_, "")) | None => "/".into(),
        Some((_, name)) => name.into(),
    }
}

/// Split "/a/b" into elements, refusing anything `check_name` would.
pub fn elements(path: &str) -> Result<Vec<String>> {
    ensure!(path.starts_with('/'), "9P paths are absolute: {path:?}");
    let parts: Vec<String> = path
        .split('/')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    for p in &parts {
        if p != ".." {
            check_name(p)?;
        }
    }
    Ok(parts)
}

/// Bytes as event text: UTF-8 when it is, hex otherwise.
pub fn to_text(bytes: &[u8]) -> (String, &'static str) {
    match std::str::from_utf8(bytes) {
        Ok(s) => (s.to_string(), "utf8"),
        Err(_) => (bytes.iter().map(|b| format!("{b:02x}")).collect(), "hex"),
    }
}

/// Text from an action back to bytes, by its declared encoding.
pub fn from_text(text: &str, encoding: Option<&str>) -> Result<Vec<u8>> {
    match encoding.unwrap_or("utf8") {
        "utf8" => Ok(text.as_bytes().to_vec()),
        "hex" => {
            ensure!(
                text.is_ascii() && text.len().is_multiple_of(2),
                "hex data must be an even number of hex digits"
            );
            (0..text.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&text[i..i + 2], 16).context("invalid hex data"))
                .collect()
        }
        other => bail!("encoding must be utf8 or hex, not {other}"),
    }
}
