//! ClickHouse native TCP protocol, at protocol revision 54429 (the first with settings
//! serialized as strings, and before the later revision-dependent query fields): hello,
//! query, data blocks with typed columns, compressed frames (LZ4, CityHash128 v1.0.2
//! checksums), exceptions, progress and end of stream. Shared by server and client.
//!
//! The protocol has no frame lengths, so every read is bounded where it happens: strings,
//! column and row counts, each compressed frame, and the total bytes one block may consume.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt};

/// The protocol revision NetGet speaks in both roles; the peer uses the minimum of the two.
pub const REVISION: u64 = 54429;
pub const VERSION_MAJOR: u64 = 24;
pub const VERSION_MINOR: u64 = 8;
pub const VERSION_PATCH: u64 = 0;
/// Longest string field (a query, a password, a String value).
pub const MAX_STRING: usize = 1024 * 1024;
/// Bytes one block may consume, compressed frames included.
pub const MAX_BLOCK_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_COLUMNS: usize = 1000;
pub const MAX_ROWS: usize = 100_000;
/// Settings (or other name/value pairs) in one query.
pub const MAX_SETTINGS: usize = 1000;
/// Deadline for packets that must follow at once (a query's external-tables terminator).
pub const IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub mod client_packet {
    pub const HELLO: u64 = 0;
    pub const QUERY: u64 = 1;
    pub const DATA: u64 = 2;
    pub const CANCEL: u64 = 3;
    pub const PING: u64 = 4;
}

pub mod server_packet {
    pub const HELLO: u64 = 0;
    pub const DATA: u64 = 1;
    pub const EXCEPTION: u64 = 2;
    pub const PROGRESS: u64 = 3;
    pub const PONG: u64 = 4;
    pub const END_OF_STREAM: u64 = 5;
    pub const PROFILE_INFO: u64 = 6;
    pub const TOTALS: u64 = 7;
    pub const EXTREMES: u64 = 8;
    pub const LOG: u64 = 10;
    pub const TABLE_COLUMNS: u64 = 11;
}

pub fn put_varuint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

pub fn put_string(out: &mut Vec<u8>, s: &[u8]) {
    put_varuint(out, s.len() as u64);
    out.extend_from_slice(s);
}

/// Bytes read from the peer, from the socket directly or through compressed frames, against
/// a budget.
pub struct Source<'a, R> {
    inner: &'a mut R,
    compressed: bool,
    buf: Vec<u8>,
    pos: usize,
    budget: usize,
}

impl<'a, R: AsyncRead + Unpin> Source<'a, R> {
    pub fn plain(inner: &'a mut R) -> Self {
        Self::new(inner, false)
    }
    pub fn new(inner: &'a mut R, compressed: bool) -> Self {
        Self {
            inner,
            compressed,
            buf: Vec::new(),
            pos: 0,
            budget: MAX_BLOCK_BYTES,
        }
    }

    fn spend(&mut self, n: usize) -> Result<()> {
        ensure!(
            n <= self.budget,
            "ClickHouse block exceeds {MAX_BLOCK_BYTES} bytes"
        );
        self.budget -= n;
        Ok(())
    }

    async fn refill(&mut self) -> Result<()> {
        let mut header = [0u8; 25];
        self.inner.read_exact(&mut header).await?;
        let method = header[16];
        let compressed_size = u32::from_le_bytes(header[17..21].try_into()?) as usize;
        let size = u32::from_le_bytes(header[21..25].try_into()?) as usize;
        ensure!(
            compressed_size >= 9,
            "compressed frame smaller than its header"
        );
        self.spend(compressed_size + 16)?;
        ensure!(
            size <= self.budget,
            "compressed frame decompresses past the block bound"
        );
        let mut frame = Vec::with_capacity(compressed_size);
        frame.extend_from_slice(&header[16..25]);
        frame.resize(compressed_size, 0);
        self.inner.read_exact(&mut frame[9..]).await?;
        let sum = naive_cityhash::cityhash128(&frame);
        let mut expected = [0u8; 16];
        expected[..8].copy_from_slice(&sum.lo.to_le_bytes());
        expected[8..].copy_from_slice(&sum.hi.to_le_bytes());
        ensure!(
            expected == header[..16],
            "compressed frame checksum mismatch"
        );
        let data = match method {
            0x02 => frame[9..].to_vec(),
            0x82 => lz4_flex::block::decompress(&frame[9..], size)
                .map_err(|e| anyhow::anyhow!("LZ4 frame: {e}"))?,
            other => bail!("compression method 0x{other:02x} is not supported (LZ4 or none)"),
        };
        ensure!(
            data.len() == size,
            "compressed frame decompressed to the wrong size"
        );
        self.buf.drain(..self.pos);
        self.pos = 0;
        self.buf.extend_from_slice(&data);
        Ok(())
    }

    pub async fn bytes(&mut self, n: usize) -> Result<Vec<u8>> {
        if self.compressed {
            while self.buf.len() - self.pos < n {
                self.refill().await?;
            }
            let out = self.buf[self.pos..self.pos + n].to_vec();
            self.pos += n;
            Ok(out)
        } else {
            self.spend(n)?;
            let mut out = vec![0u8; n];
            self.inner.read_exact(&mut out).await?;
            Ok(out)
        }
    }
    pub async fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1).await?[0])
    }
    pub async fn varuint(&mut self) -> Result<u64> {
        let mut v = 0u64;
        for i in 0..10 {
            let b = self.u8().await?;
            v |= u64::from(b & 0x7f) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        bail!("VarUInt longer than 10 bytes")
    }
    pub async fn string(&mut self) -> Result<String> {
        let b = self.raw_string().await?;
        Ok(String::from_utf8_lossy(&b).to_string())
    }
    pub async fn raw_string(&mut self) -> Result<Vec<u8>> {
        let n = self.varuint().await? as usize;
        ensure!(n <= MAX_STRING, "string of {n} bytes exceeds {MAX_STRING}");
        self.bytes(n).await
    }
    /// Whether every compressed byte read so far has been consumed (a block ends on a frame).
    pub fn drained(&self) -> bool {
        self.pos == self.buf.len()
    }
}

/// The first packet type of a message, waiting at most `idle`. `None` on a clean EOF.
pub async fn read_packet_type<R: AsyncRead + Unpin>(
    r: &mut R,
    idle: std::time::Duration,
) -> Result<Option<u64>> {
    let mut first = [0u8; 1];
    match tokio::time::timeout(idle, r.read(&mut first)).await {
        Ok(Ok(0)) => return Ok(None),
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => bail!("ClickHouse peer idle for {}s", idle.as_secs()),
    }
    let mut v = u64::from(first[0] & 0x7f);
    let mut b = first[0];
    let mut shift = 7;
    while b & 0x80 != 0 {
        ensure!(shift < 64, "VarUInt longer than 10 bytes");
        b = r.read_u8().await?;
        v |= u64::from(b & 0x7f) << shift;
        shift += 7;
    }
    Ok(Some(v))
}

/// A column type NetGet can encode and decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColType {
    UInt8,
    UInt16,
    UInt32,
    UInt64,
    Int8,
    Int16,
    Int32,
    Int64,
    Float32,
    Float64,
    Bool,
    String,
    Date,
    DateTime,
    Nullable(Box<ColType>),
}

impl ColType {
    pub fn parse(t: &str) -> Result<Self> {
        let t = t.trim();
        if let Some(inner) = t
            .strip_prefix("Nullable(")
            .and_then(|r| r.strip_suffix(')'))
        {
            let inner = Self::parse(inner)?;
            ensure!(
                !matches!(inner, ColType::Nullable(_)),
                "Nullable cannot nest"
            );
            return Ok(ColType::Nullable(Box::new(inner)));
        }
        if t.starts_with("DateTime(") && !t.starts_with("DateTime64") {
            return Ok(ColType::DateTime);
        }
        Ok(match t {
            "UInt8" => ColType::UInt8,
            "UInt16" => ColType::UInt16,
            "UInt32" => ColType::UInt32,
            "UInt64" => ColType::UInt64,
            "Int8" => ColType::Int8,
            "Int16" => ColType::Int16,
            "Int32" => ColType::Int32,
            "Int64" => ColType::Int64,
            "Float32" => ColType::Float32,
            "Float64" => ColType::Float64,
            "Bool" => ColType::Bool,
            "String" => ColType::String,
            "Date" => ColType::Date,
            "DateTime" => ColType::DateTime,
            other => bail!("column type {other} is not supported (integers, floats, Bool, String, Date, DateTime, Nullable of those)"),
        })
    }

    fn width(&self) -> usize {
        match self {
            ColType::UInt8 | ColType::Int8 | ColType::Bool => 1,
            ColType::UInt16 | ColType::Int16 | ColType::Date => 2,
            ColType::UInt32 | ColType::Int32 | ColType::Float32 | ColType::DateTime => 4,
            ColType::UInt64 | ColType::Int64 | ColType::Float64 => 8,
            ColType::String | ColType::Nullable(_) => 0,
        }
    }

    /// Encode one non-null value of this type from JSON.
    fn encode(&self, v: &Value, out: &mut Vec<u8>) -> Result<()> {
        let int = |v: &Value| -> Result<i128> {
            v.as_i64()
                .map(i128::from)
                .or_else(|| v.as_u64().map(i128::from))
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                .context("expected an integer")
        };
        let ranged = |lo: i128, hi: i128| -> Result<i128> {
            let n = int(v)?;
            ensure!((lo..=hi).contains(&n), "{n} out of range for this column");
            Ok(n)
        };
        match self {
            ColType::UInt8 => out.push(ranged(0, u8::MAX.into())? as u8),
            ColType::UInt16 => out.extend((ranged(0, u16::MAX.into())? as u16).to_le_bytes()),
            ColType::UInt32 => out.extend((ranged(0, u32::MAX.into())? as u32).to_le_bytes()),
            ColType::UInt64 => out.extend((ranged(0, u64::MAX.into())? as u64).to_le_bytes()),
            ColType::Int8 => {
                out.extend((ranged(i8::MIN.into(), i8::MAX.into())? as i8).to_le_bytes())
            }
            ColType::Int16 => {
                out.extend((ranged(i16::MIN.into(), i16::MAX.into())? as i16).to_le_bytes())
            }
            ColType::Int32 => {
                out.extend((ranged(i32::MIN.into(), i32::MAX.into())? as i32).to_le_bytes())
            }
            ColType::Int64 => {
                out.extend((ranged(i64::MIN.into(), i64::MAX.into())? as i64).to_le_bytes())
            }
            ColType::Float32 => {
                out.extend((v.as_f64().context("expected a number")? as f32).to_le_bytes())
            }
            ColType::Float64 => out.extend(v.as_f64().context("expected a number")?.to_le_bytes()),
            ColType::Bool => out.push(u8::from(v.as_bool().context("expected a boolean")?)),
            ColType::String => {
                let s = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                ensure!(s.len() <= MAX_STRING, "string value too long");
                put_string(out, s.as_bytes());
            }
            ColType::Date => {
                let d = chrono::NaiveDate::parse_from_str(
                    v.as_str().context("expected YYYY-MM-DD")?,
                    "%Y-%m-%d",
                )?;
                let days =
                    (d - chrono::NaiveDate::from_ymd_opt(1970, 1, 1).context("epoch")?).num_days();
                ensure!(
                    (0..=i64::from(u16::MAX)).contains(&days),
                    "date out of range"
                );
                out.extend((days as u16).to_le_bytes());
            }
            ColType::DateTime => {
                let t = chrono::NaiveDateTime::parse_from_str(
                    v.as_str().context("expected YYYY-MM-DD hh:mm:ss")?,
                    "%Y-%m-%d %H:%M:%S",
                )?;
                let secs = t.and_utc().timestamp();
                ensure!(
                    (0..=i64::from(u32::MAX)).contains(&secs),
                    "datetime out of range"
                );
                out.extend((secs as u32).to_le_bytes());
            }
            ColType::Nullable(_) => bail!("nested Nullable"),
        }
        Ok(())
    }

    fn default_bytes(&self, out: &mut Vec<u8>) {
        match self {
            ColType::String => out.push(0),
            other => out.extend(std::iter::repeat_n(0, other.width())),
        }
    }

    /// Encode a whole column of `rows` values.
    pub fn encode_column(&self, values: &[&Value]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        match self {
            ColType::Nullable(inner) => {
                for v in values {
                    out.push(u8::from(v.is_null()));
                }
                for v in values {
                    if v.is_null() {
                        inner.default_bytes(&mut out);
                    } else {
                        inner.encode(v, &mut out)?;
                    }
                }
            }
            other => {
                for v in values {
                    ensure!(!v.is_null(), "null in a non-Nullable column");
                    other.encode(v, &mut out)?;
                }
            }
        }
        Ok(out)
    }

    async fn decode_value<R: AsyncRead + Unpin>(&self, s: &mut Source<'_, R>) -> Result<Value> {
        let w = self.width();
        Ok(match self {
            ColType::String => {
                let b = s.raw_string().await?;
                match String::from_utf8(b) {
                    Ok(t) => json!(t),
                    Err(e) => json!(e
                        .into_bytes()
                        .iter()
                        .map(|x| format!("{x:02x}"))
                        .collect::<String>()),
                }
            }
            _ => {
                let b = s.bytes(w).await?;
                match self {
                    ColType::UInt8 => json!(b[0]),
                    ColType::UInt16 => json!(u16::from_le_bytes(b[..2].try_into()?)),
                    ColType::UInt32 => json!(u32::from_le_bytes(b[..4].try_into()?)),
                    ColType::UInt64 => json!(u64::from_le_bytes(b[..8].try_into()?)),
                    ColType::Int8 => json!(b[0] as i8),
                    ColType::Int16 => json!(i16::from_le_bytes(b[..2].try_into()?)),
                    ColType::Int32 => json!(i32::from_le_bytes(b[..4].try_into()?)),
                    ColType::Int64 => json!(i64::from_le_bytes(b[..8].try_into()?)),
                    ColType::Float32 => json!(f32::from_le_bytes(b[..4].try_into()?)),
                    ColType::Float64 => json!(f64::from_le_bytes(b[..8].try_into()?)),
                    ColType::Bool => json!(b[0] != 0),
                    ColType::Date => {
                        let days = u16::from_le_bytes(b[..2].try_into()?);
                        let d = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).context("epoch")?
                            + chrono::Duration::days(i64::from(days));
                        json!(d.format("%Y-%m-%d").to_string())
                    }
                    ColType::DateTime => {
                        let secs = u32::from_le_bytes(b[..4].try_into()?);
                        let t = chrono::DateTime::from_timestamp(i64::from(secs), 0)
                            .context("timestamp")?;
                        json!(t.format("%Y-%m-%d %H:%M:%S").to_string())
                    }
                    _ => unreachable!("handled above"),
                }
            }
        })
    }

    async fn decode_column<R: AsyncRead + Unpin>(
        &self,
        s: &mut Source<'_, R>,
        rows: usize,
    ) -> Result<Vec<Value>> {
        let mut out = Vec::with_capacity(rows.min(1024));
        match self {
            ColType::Nullable(inner) => {
                let nulls = s.bytes(rows).await?;
                for null in nulls {
                    let v = Box::pin(inner.decode_value(s)).await?;
                    out.push(if null != 0 { Value::Null } else { v });
                }
            }
            other => {
                for _ in 0..rows {
                    out.push(Box::pin(other.decode_value(s)).await?);
                }
            }
        }
        Ok(out)
    }
}

/// A block: named, typed columns and rows (row-major, as the handler sees them).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Block {
    pub columns: Vec<(String, String)>,
    pub rows: Vec<Vec<Value>>,
}

impl Block {
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty() && self.rows.is_empty()
    }

    /// Check a block the handler supplied: types NetGet can encode, rows of the right width.
    pub fn from_json(columns: &Value, rows: &Value) -> Result<Self> {
        let cols = columns.as_array().context("columns must be an array")?;
        ensure!(cols.len() <= MAX_COLUMNS, "at most {MAX_COLUMNS} columns");
        let mut columns = Vec::with_capacity(cols.len());
        for c in cols {
            let name = c["name"].as_str().context("each column needs a name")?;
            let t = c["type"].as_str().context("each column needs a type")?;
            ColType::parse(t)?;
            columns.push((name.to_string(), t.to_string()));
        }
        let rows: Vec<Vec<Value>> = match rows {
            Value::Null => Vec::new(),
            Value::Array(rs) => {
                ensure!(rs.len() <= MAX_ROWS, "at most {MAX_ROWS} rows");
                rs.iter()
                    .map(|r| {
                        let r = r.as_array().context("each row must be an array")?;
                        ensure!(
                            r.len() == columns.len(),
                            "a row has {} values for {} columns",
                            r.len(),
                            columns.len()
                        );
                        Ok(r.clone())
                    })
                    .collect::<Result<_>>()?
            }
            _ => bail!("rows must be an array of arrays"),
        };
        let block = Block { columns, rows };
        block.encode_body()?;
        Ok(block)
    }

    /// BlockInfo, counts, then each column's name, type and data.
    pub fn encode_body(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        put_varuint(&mut out, 1);
        out.push(0); // is_overflows
        put_varuint(&mut out, 2);
        out.extend((-1i32).to_le_bytes()); // bucket_num
        put_varuint(&mut out, 0);
        put_varuint(&mut out, self.columns.len() as u64);
        put_varuint(&mut out, self.rows.len() as u64);
        for (i, (name, t)) in self.columns.iter().enumerate() {
            put_string(&mut out, name.as_bytes());
            put_string(&mut out, t.as_bytes());
            let values: Vec<&Value> = self.rows.iter().map(|r| &r[i]).collect();
            out.extend(
                ColType::parse(t)?
                    .encode_column(&values)
                    .with_context(|| format!("column {name}"))?,
            );
        }
        Ok(out)
    }

    pub async fn decode<R: AsyncRead + Unpin>(s: &mut Source<'_, R>) -> Result<Self> {
        loop {
            match s.varuint().await? {
                0 => break,
                1 => {
                    s.u8().await?;
                }
                2 => {
                    s.bytes(4).await?;
                }
                other => bail!("unknown BlockInfo field {other}"),
            }
        }
        let ncols = s.varuint().await? as usize;
        let nrows = s.varuint().await? as usize;
        ensure!(ncols <= MAX_COLUMNS, "block of {ncols} columns");
        ensure!(nrows <= MAX_ROWS, "block of {nrows} rows");
        let mut columns = Vec::with_capacity(ncols);
        let mut data = Vec::with_capacity(ncols);
        for _ in 0..ncols {
            let name = s.string().await?;
            let t = s.string().await?;
            let col = ColType::parse(&t)?;
            data.push(col.decode_column(s, nrows).await?);
            columns.push((name, t));
        }
        let rows = (0..nrows)
            .map(|r| data.iter().map(|c| c[r].clone()).collect())
            .collect();
        Ok(Block { columns, rows })
    }

    pub fn columns_json(&self) -> Value {
        Value::Array(
            self.columns
                .iter()
                .map(|(n, t)| json!({"name": n, "type": t}))
                .collect(),
        )
    }
}

/// One compressed frame (LZ4) around `data`, with its CityHash128 checksum.
pub fn compress(data: &[u8]) -> Vec<u8> {
    let body = lz4_flex::block::compress(data);
    let mut frame = Vec::with_capacity(body.len() + 25);
    frame.push(0x82);
    frame.extend(((body.len() + 9) as u32).to_le_bytes());
    frame.extend((data.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    let sum = naive_cityhash::cityhash128(&frame);
    let mut out = Vec::with_capacity(frame.len() + 16);
    out.extend(sum.lo.to_le_bytes());
    out.extend(sum.hi.to_le_bytes());
    out.extend(frame);
    out
}

/// A Data packet (server 1 / client 2): table name outside, the block inside, compressed when
/// the query asked for it.
pub fn data_packet(kind: u64, block: &Block, compressed: bool) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    put_varuint(&mut out, kind);
    put_string(&mut out, b"");
    let body = block.encode_body()?;
    if compressed {
        out.extend(compress(&body));
    } else {
        out.extend(body);
    }
    Ok(out)
}

/// An Exception packet.
pub fn exception(code: i32, message: &str) -> Vec<u8> {
    let mut out = Vec::new();
    put_varuint(&mut out, server_packet::EXCEPTION);
    out.extend(code.to_le_bytes());
    put_string(&mut out, b"DB::Exception");
    put_string(&mut out, format!("DB::Exception: {message}").as_bytes());
    put_string(&mut out, b"");
    out.push(0);
    out
}

pub fn progress(rows: u64, bytes: u64, written_rows: u64) -> Vec<u8> {
    let mut out = Vec::new();
    put_varuint(&mut out, server_packet::PROGRESS);
    put_varuint(&mut out, rows);
    put_varuint(&mut out, bytes);
    put_varuint(&mut out, rows);
    put_varuint(&mut out, written_rows);
    put_varuint(&mut out, 0);
    out
}

/// The client's Query packet fields at revision 54429.
#[derive(Debug, Clone, Default)]
pub struct Query {
    pub query_id: String,
    pub client_name: String,
    pub settings: Vec<(String, String)>,
    pub stage: u64,
    pub compression: bool,
    pub query: String,
}

/// Read a Query packet (after its type) at `revision` (at most 54429).
pub async fn read_query<R: AsyncRead + Unpin>(
    s: &mut Source<'_, R>,
    revision: u64,
) -> Result<Query> {
    let mut q = Query {
        query_id: s.string().await?,
        ..Default::default()
    };
    if revision >= 54032 {
        let kind = s.u8().await?;
        if kind != 0 {
            for _ in 0..3 {
                s.string().await?; // initial user, query id, address
            }
            let interface = s.u8().await?;
            ensure!(interface == 1, "client interface {interface} is not TCP");
            s.string().await?; // os user
            s.string().await?; // hostname
            q.client_name = s.string().await?;
            s.varuint().await?;
            s.varuint().await?;
            s.varuint().await?;
            if revision >= 54060 {
                s.string().await?; // quota key
            }
            if revision >= 54401 {
                s.varuint().await?; // version patch
            }
        }
    }
    for _ in 0..=MAX_SETTINGS {
        let name = s.string().await?;
        if name.is_empty() {
            q.stage = s.varuint().await?;
            q.compression = s.varuint().await? != 0;
            q.query = s.string().await?;
            return Ok(q);
        }
        s.varuint().await?; // flags
        let value = s.string().await?;
        q.settings.push((name, value));
    }
    bail!("more than {MAX_SETTINGS} settings")
}

/// The client's Query packet, as NetGet's client sends it at revision 54429.
pub fn query_packet(query: &str, compression: bool) -> Vec<u8> {
    let mut out = Vec::new();
    put_varuint(&mut out, client_packet::QUERY);
    put_string(&mut out, b"");
    out.push(1); // initial query
    put_string(&mut out, b"");
    put_string(&mut out, b"");
    put_string(&mut out, b"0.0.0.0:0");
    out.push(1); // TCP
    put_string(&mut out, b"netget");
    put_string(&mut out, b"netget");
    put_string(&mut out, b"NetGet");
    put_varuint(&mut out, VERSION_MAJOR);
    put_varuint(&mut out, VERSION_MINOR);
    put_varuint(&mut out, REVISION);
    put_string(&mut out, b"");
    put_varuint(&mut out, VERSION_PATCH);
    put_string(&mut out, b""); // end of settings
    put_varuint(&mut out, 2); // stage: complete
    put_varuint(&mut out, u64::from(compression));
    put_string(&mut out, query.as_bytes());
    out
}

/// A decoded Exception packet (after its type): code and message, nested ones joined.
pub async fn read_exception<R: AsyncRead + Unpin>(s: &mut Source<'_, R>) -> Result<(i32, String)> {
    let mut first: Option<(i32, String)> = None;
    for _ in 0..16 {
        let code = i32::from_le_bytes(
            s.bytes(4)
                .await?
                .try_into()
                .map_err(|_| anyhow::anyhow!("code"))?,
        );
        s.string().await?; // name
        let message = s.string().await?;
        s.string().await?; // stack trace
        let nested = s.u8().await? != 0;
        if first.is_none() {
            first = Some((code, message));
        }
        if !nested {
            return first.context("exception");
        }
    }
    bail!("exception nested too deeply")
}
