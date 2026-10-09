//! Bounded selected RFC6733 TCP messages and RFC7155 stateless NASREQ.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_FRAME_BYTES: usize = 16 * 1024;
pub const MAX_AVPS: usize = 64;
pub const DEFAULT_IO_SECONDS: u64 = 90;
pub const DEFAULT_HANDLER_SECONDS: u64 = 30;
pub const DEFAULT_WATCHDOG_SECONDS: u64 = 30;
pub const DEFAULT_LLM_FALLBACK: bool = false;
pub const CER: u32 = 257;
pub const AA: u32 = 265;
pub const DWR: u32 = 280;
pub const DPR: u32 = 282;
pub const SESSION: u32 = 263;
pub const HOST: u32 = 264;
pub const REALM: u32 = 296;
pub const DEST_REALM: u32 = 283;
pub const DEST_HOST: u32 = 293;
pub const AUTH_APP: u32 = 258;
pub const ACCT_APP: u32 = 259;
pub const VENDOR_APP: u32 = 260;
pub const AUTH_TYPE: u32 = 274;
pub const AUTH_STATE: u32 = 277;
pub const RESULT: u32 = 268;
pub const USER: u32 = 1;
pub const PASSWORD: u32 = 2;
pub const HOST_IP: u32 = 257;
pub const VENDOR: u32 = 266;
pub const PRODUCT: u32 = 269;
pub const ORIGIN_STATE: u32 = 278;
pub const SECURITY: u32 = 299;
pub const SUPPORTED_VENDOR: u32 = 265;
pub const FIRMWARE: u32 = 267;
pub const DISCONNECT_CAUSE: u32 = 273;
pub const ERROR_MESSAGE: u32 = 281;
pub const FAILED_AVP: u32 = 279;
pub const NAS_ID: u32 = 32;
pub const NAS_PORT: u32 = 5;
pub const SERVICE: u32 = 6;
pub const REPLY_MESSAGE: u32 = 18;
pub const FILTER_ID: u32 = 11;
pub const SESSION_TIMEOUT: u32 = 27;
pub const MAX_PASSWORD_BYTES: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Avp {
    pub code: u32,
    pub flags: u8,
    pub vendor: Option<u32>,
    pub data: Vec<u8>,
}
impl Avp {
    pub fn bytes(code: u32, data: impl Into<Vec<u8>>) -> Self {
        Self {
            code,
            flags: if [PRODUCT, FIRMWARE, ERROR_MESSAGE].contains(&code) {
                0
            } else {
                0x40
            },
            vendor: None,
            data: data.into(),
        }
    }
    pub fn number(code: u32, value: u32) -> Self {
        Self::bytes(code, value.to_be_bytes())
    }
    pub fn text(code: u32, value: &str) -> Self {
        Self::bytes(code, value.as_bytes())
    }
    pub fn integer(&self) -> Result<u32> {
        ensure!(self.data.len() == 4, "Diameter integer AVP length");
        Ok(u32::from_be_bytes(self.data[..].try_into()?))
    }
    pub fn string(&self, max: usize) -> Result<String> {
        ensure!(self.data.len() <= max, "Diameter string AVP bound");
        let s = std::str::from_utf8(&self.data).context("Diameter UTF8 AVP")?;
        ensure!(!s.contains('\0'), "Diameter NUL excluded");
        Ok(s.into())
    }
    fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        ensure!(
            self.flags & 0x1f == 0 && (self.flags & 0x80 != 0) == self.vendor.is_some(),
            "Diameter AVP flags"
        );
        let length = 8 + self.vendor.is_some() as usize * 4 + self.data.len();
        ensure!(length <= MAX_FRAME_BYTES, "Diameter AVP size");
        ensure!(
            out.len() + ((length + 3) & !3) <= MAX_FRAME_BYTES,
            "Diameter aggregate message size before copy"
        );
        out.extend(self.code.to_be_bytes());
        out.push(self.flags);
        put24(out, length as u32);
        if let Some(vendor) = self.vendor {
            out.extend(vendor.to_be_bytes());
        }
        out.extend(&self.data);
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    pub flags: u8,
    pub command: u32,
    pub application: u32,
    pub hop: u32,
    pub end: u32,
    pub avps: Vec<Avp>,
}
impl Packet {
    pub fn request(command: u32, application: u32) -> Result<Self> {
        let mut ids = [0; 8];
        rand::RngCore::try_fill_bytes(&mut rand::rngs::OsRng, &mut ids)
            .map_err(|_| anyhow::anyhow!("Diameter identifier source unavailable"))?;
        Ok(Self {
            flags: 0x80 | if command == AA { 0x40 } else { 0 },
            command,
            application,
            hop: u32::from_be_bytes(ids[..4].try_into()?),
            end: u32::from_be_bytes(ids[4..].try_into()?),
            avps: vec![],
        })
    }
    pub fn is_request(&self) -> bool {
        self.flags & 0x80 != 0
    }
    pub fn answer(&self, result: u32) -> Self {
        Self {
            flags: self.flags & 0x40
                | if (3000..4000).contains(&result) {
                    0x20
                } else {
                    0
                },
            command: self.command,
            application: self.application,
            hop: self.hop,
            end: self.end,
            avps: vec![Avp::number(RESULT, result)],
        }
    }
    pub fn matches(&self, request: &Self) -> bool {
        !self.is_request()
            && self.command == request.command
            && self.application == request.application
            && self.hop == request.hop
            && self.end == request.end
            && (self.flags & 0x40) == (request.flags & 0x40)
    }
    pub fn one(&self, code: u32) -> Result<&Avp> {
        let mut v = self
            .avps
            .iter()
            .filter(|a| a.code == code && a.vendor.is_none());
        let first = v.next().context("Diameter required AVP missing")?;
        ensure!(v.next().is_none(), "Diameter singleton AVP repeated");
        Ok(first)
    }
    pub fn optional(&self, code: u32) -> Result<Option<&Avp>> {
        let mut v = self
            .avps
            .iter()
            .filter(|a| a.code == code && a.vendor.is_none());
        let first = v.next();
        ensure!(v.next().is_none(), "Diameter singleton AVP repeated");
        Ok(first)
    }
    pub fn num(&self, code: u32) -> Result<u32> {
        self.one(code)?.integer()
    }
    pub fn text(&self, code: u32, max: usize) -> Result<String> {
        self.one(code)?.string(max)
    }
    pub fn origin(&mut self, identity: &Identity) {
        self.avps.push(Avp::text(HOST, &identity.host));
        self.avps.push(Avp::text(REALM, &identity.realm));
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        ensure!(
            self.command <= 0xffffff
                && self.flags & 15 == 0
                && !(self.is_request() && self.flags & 0x20 != 0)
                && !(!self.is_request() && self.flags & 0x10 != 0)
                && self.avps.len() <= MAX_AVPS,
            "Diameter header/AVP bounds"
        );
        let mut out = vec![1, 0, 0, 0, self.flags];
        put24(&mut out, self.command);
        out.extend(self.application.to_be_bytes());
        out.extend(self.hop.to_be_bytes());
        out.extend(self.end.to_be_bytes());
        for avp in &self.avps {
            avp.encode(&mut out)?;
        }
        ensure!(out.len() <= MAX_FRAME_BYTES, "Diameter message size");
        let length = out.len() as u32;
        out[1..4].copy_from_slice(&length.to_be_bytes()[1..]);
        Ok(out)
    }
    pub fn decode(data: &[u8]) -> Result<Self> {
        ensure!(
            data.len() >= 20
                && data.len() <= MAX_FRAME_BYTES
                && data[0] == 1
                && get24(&data[1..4]) as usize == data.len()
                && data.len().is_multiple_of(4),
            "Diameter frame header/size"
        );
        let flags = data[4] & 0xf0;
        ensure!(
            flags & 15 == 0
                && !(flags & 0x80 != 0 && flags & 0x20 != 0)
                && !(flags & 0x80 == 0 && flags & 0x10 != 0),
            "Diameter header flags"
        );
        let mut avps = vec![];
        let mut offset = 20;
        while offset < data.len() {
            ensure!(
                data.len() - offset >= 8 && avps.len() < MAX_AVPS,
                "Diameter AVP header/count"
            );
            let code = u32::from_be_bytes(data[offset..offset + 4].try_into()?);
            let flags = data[offset + 4] & 0xe0;
            ensure!(flags & 0x1f == 0, "Diameter AVP reserved flags");
            let length = get24(&data[offset + 5..offset + 8]) as usize;
            let head = if flags & 0x80 != 0 { 12 } else { 8 };
            let padded = (length + 3) & !3;
            ensure!(
                length >= head && offset + padded <= data.len(),
                "Diameter AVP length/padding"
            );
            let vendor = if head == 12 {
                Some(u32::from_be_bytes(
                    data[offset + 8..offset + 12].try_into()?,
                ))
            } else {
                None
            };
            avps.push(Avp {
                code,
                flags,
                vendor,
                data: data[offset + head..offset + length].to_vec(),
            });
            offset += padded;
        }
        Ok(Self {
            flags,
            command: get24(&data[5..8]),
            application: u32::from_be_bytes(data[8..12].try_into()?),
            hop: u32::from_be_bytes(data[12..16].try_into()?),
            end: u32::from_be_bytes(data[16..20].try_into()?),
            avps,
        })
    }
    pub fn unsupported_mandatory(&self, allowed: &[u32]) -> Option<&Avp> {
        self.avps
            .iter()
            .find(|a| a.flags & 0x40 != 0 && (a.vendor.is_some() || !allowed.contains(&a.code)))
    }
}
fn put24(out: &mut Vec<u8>, v: u32) {
    out.extend(&v.to_be_bytes()[1..]);
}
fn get24(v: &[u8]) -> u32 {
    u32::from_be_bytes([0, v[0], v[1], v[2]])
}
pub async fn read_packet<R: AsyncRead + Unpin>(r: &mut R, deadline: Duration) -> Result<Packet> {
    tokio::time::timeout(deadline, async {
        let mut h = [0; 20];
        r.read_exact(&mut h).await?;
        let size = get24(&h[1..4]) as usize;
        ensure!(
            h[0] == 1 && (20..=MAX_FRAME_BYTES).contains(&size) && size.is_multiple_of(4),
            "Diameter length/version before allocation"
        );
        let mut bytes = vec![0; size];
        bytes[..20].copy_from_slice(&h);
        r.read_exact(&mut bytes[20..]).await?;
        Packet::decode(&bytes)
    })
    .await
    .context("Diameter complete-frame deadline")?
}
pub async fn write_packet<W: AsyncWrite + Unpin>(w: &mut W, p: &Packet) -> Result<usize> {
    let b = p.encode()?;
    tokio::time::timeout(Duration::from_secs(10), w.write_all(&b))
        .await
        .context("Diameter write deadline")??;
    Ok(b.len())
}
pub fn within_json_budget(v: &Value) -> bool {
    crate::utils::json_budget::within_budget(v, MAX_FRAME_BYTES, 4096, 16)
}
pub fn owned_json(v: Value) -> Result<Value> {
    if !within_json_budget(&v) {
        crate::utils::json_budget::drop_iteratively(v);
        bail!("Diameter JSON size/node/depth budget");
    }
    Ok(v)
}
pub fn timeout(value: Option<u64>, default: u64) -> Result<Duration> {
    let v = value.unwrap_or(default);
    ensure!((1..=300).contains(&v), "Diameter timeout1..300seconds");
    Ok(Duration::from_secs(v))
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub host: String,
    pub realm: String,
}
pub fn identity_text(v: &str) -> Result<()> {
    ensure!(
        !v.is_empty()
            && v.len() <= 255
            && v.is_ascii()
            && v.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-'),
        "Diameter selected ASCII identity"
    );
    Ok(())
}
impl Identity {
    pub fn validate(&self) -> Result<()> {
        identity_text(&self.host)?;
        identity_text(&self.realm)
    }
    pub fn from_packet(p: &Packet) -> Result<Self> {
        let id = Self {
            host: p.text(HOST, 255)?,
            realm: p.text(REALM, 255)?,
        };
        id.validate()?;
        Ok(id)
    }
    pub fn same_peer(&self, p: &Packet) -> Result<()> {
        ensure!(
            {
                let other = Self::from_packet(p)?;
                self.host.eq_ignore_ascii_case(&other.host)
                    && self.realm.eq_ignore_ascii_case(&other.realm)
            },
            "Diameter peer identity changed"
        );
        Ok(())
    }
}
pub const BASE_ALLOWED: &[u32] = &[
    HOST,
    REALM,
    HOST_IP,
    VENDOR,
    PRODUCT,
    AUTH_APP,
    ACCT_APP,
    VENDOR_APP,
    ORIGIN_STATE,
    SECURITY,
    SUPPORTED_VENDOR,
    FIRMWARE,
    RESULT,
    ERROR_MESSAGE,
    FAILED_AVP,
    DISCONNECT_CAUSE,
];
pub fn capabilities(p: &Packet) -> Result<Identity> {
    ensure!(
        p.command == CER && p.application == 0 && p.flags & 0x40 == 0,
        "Diameter capabilities header"
    );
    ensure!(
        p.unsupported_mandatory(BASE_ALLOWED).is_none(),
        "Diameter unsupported capability AVP"
    );
    let id = Identity::from_packet(p)?;
    p.num(VENDOR)?;
    ensure!(
        !p.text(PRODUCT, 255)?.is_empty(),
        "Diameter product required"
    );
    let ips: Vec<_> = p
        .avps
        .iter()
        .filter(|a| a.code == HOST_IP && a.vendor.is_none())
        .collect();
    ensure!(
        !ips.is_empty() && ips.len() <= 8,
        "Diameter capability address count"
    );
    for ip in ips {
        decode_address(&ip.data)?;
    }
    ensure!(
        p.avps
            .iter()
            .filter(|a| a.code == AUTH_APP && a.vendor.is_none())
            .any(|a| a.integer().ok() == Some(1)),
        "Diameter NASREQ not advertised"
    );
    for a in p
        .avps
        .iter()
        .filter(|a| a.code == SECURITY && a.vendor.is_none())
    {
        ensure!(a.integer()? == 0, "Diameter inband TLS excluded");
    }
    for avp in p.avps.iter().filter(|a| a.vendor.is_none()) {
        match avp.code {
            AUTH_APP | ACCT_APP | SUPPORTED_VENDOR | FIRMWARE | ORIGIN_STATE => {
                avp.integer()?;
            }
            VENDOR_APP => {
                // Base capability advertisement, not a supported vendor AAA
                // application or policy group. Parse only its flat typed IDs.
                let grouped = decode_capability_group(&avp.data)?;
                grouped.num(VENDOR)?;
                let auth = grouped.optional(AUTH_APP)?.map(Avp::integer).transpose()?;
                let acct = grouped.optional(ACCT_APP)?.map(Avp::integer).transpose()?;
                ensure!(
                    auth.is_some() ^ acct.is_some(),
                    "Diameter capability group app type"
                );
            }
            _ => {}
        }
    }
    if !p.is_request() {
        ensure!(
            p.num(RESULT)? == 2001 && p.flags & 0x20 == 0,
            "Diameter capability rejected"
        );
    }
    Ok(id)
}
pub fn decode_capability_group(data: &[u8]) -> Result<Packet> {
    ensure!(
        data.len() + 20 <= MAX_FRAME_BYTES,
        "Diameter capability group bound"
    );
    let mut frame = vec![1, 0, 0, 0];
    frame.resize(20, 0);
    frame.extend(data);
    let size = frame.len() as u32;
    frame[1..4].copy_from_slice(&size.to_be_bytes()[1..]);
    let group = Packet::decode(&frame)?;
    ensure!(
        group.avps.len() <= 3
            && group
                .avps
                .iter()
                .all(|a| a.vendor.is_none() && [VENDOR, AUTH_APP, ACCT_APP].contains(&a.code)),
        "Diameter flat capability group"
    );
    Ok(group)
}
pub fn capability_fields(p: &mut Packet, id: &Identity, ip: IpAddr) {
    p.origin(id);
    p.avps.push(Avp::bytes(HOST_IP, encode_address(ip)));
    p.avps.push(Avp::number(VENDOR, 0));
    p.avps
        .push(Avp::text(PRODUCT, "NetGet experimental NASREQ"));
    p.avps.push(Avp::number(AUTH_APP, 1));
}
fn encode_address(ip: IpAddr) -> Vec<u8> {
    match ip {
        IpAddr::V4(a) => [&[0, 1][..], &a.octets()].concat(),
        IpAddr::V6(a) => [&[0, 2][..], &a.octets()].concat(),
    }
}
pub fn decode_address(v: &[u8]) -> Result<IpAddr> {
    match v {
        [0, 1, rest @ ..] if rest.len() == 4 => {
            Ok(Ipv4Addr::from(<[u8; 4]>::try_from(rest)?).into())
        }
        [0, 2, rest @ ..] if rest.len() == 16 => {
            Ok(Ipv6Addr::from(<[u8; 16]>::try_from(rest)?).into())
        }
        _ => bail!("Diameter selected IP address AVP"),
    }
}
fn default_auth_type() -> u32 {
    3
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub username: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default = "default_auth_type")]
    pub auth_request_type: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nas_identifier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nas_port: Option<u32>,
}
impl Request {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.username.is_empty()
                && self.username.len() <= 255
                && !self.username.contains('\0'),
            "Diameter username UTF8 bound"
        );
        ensure!(
            (1..=3).contains(&self.auth_request_type),
            "Diameter Auth-Request-Type1..3"
        );
        ensure!(
            (self.auth_request_type == 2 && self.password.is_none())
                || (self.auth_request_type != 2 && self.password.is_some()),
            "Diameter selected PAP password required for authentication only"
        );
        if let Some(v) = &self.password {
            ensure!(
                v.len() <= MAX_PASSWORD_BYTES && !v.contains('\0'),
                "Diameter PAP UTF8 password bound"
            );
        }
        if let Some(v) = &self.nas_identifier {
            ensure!(
                v.len() <= 255 && !v.contains('\0'),
                "Diameter NAS-Identifier bound"
            );
        }
        Ok(())
    }
    pub fn credential_free(&self) -> Value {
        serde_json::json!({"username":self.username,"auth_request_type":self.auth_request_type,"nas_identifier":self.nas_identifier,"nas_port":self.nas_port})
    }
    pub fn packet(&self, id: &Identity, destination: &Identity, session: &str) -> Result<Packet> {
        self.validate()?;
        session_text(session)?;
        let mut p = Packet::request(AA, 1)?;
        p.avps.push(Avp::text(SESSION, session));
        p.origin(id);
        p.avps.push(Avp::text(DEST_REALM, &destination.realm));
        p.avps.push(Avp::text(DEST_HOST, &destination.host));
        p.avps.push(Avp::number(AUTH_APP, 1));
        p.avps.push(Avp::number(AUTH_TYPE, self.auth_request_type));
        p.avps.push(Avp::number(AUTH_STATE, 1));
        p.avps.push(Avp::text(USER, &self.username));
        if let Some(v) = &self.password {
            p.avps.push(Avp::text(PASSWORD, v));
        }
        if let Some(v) = &self.nas_identifier {
            p.avps.push(Avp::text(NAS_ID, v));
        }
        if let Some(v) = self.nas_port {
            p.avps.push(Avp::number(NAS_PORT, v));
        }
        Ok(p)
    }
    pub fn from_packet(p: &Packet, id: &Identity, peer: &Identity) -> Result<(Self, String)> {
        ensure!(
            p.is_request() && p.command == AA && p.application == 1 && p.flags & 0x40 != 0,
            "Diameter NASREQ request header"
        );
        peer.same_peer(p)?;
        ensure!(
            p.avps
                .first()
                .is_some_and(|a| a.code == SESSION && a.vendor.is_none()),
            "Diameter Session-Id fixed first AVP"
        );
        ensure!(
            p.text(DEST_REALM, 255)?.eq_ignore_ascii_case(&id.realm),
            "Diameter destination realm"
        );
        if let Some(a) = p.optional(DEST_HOST)? {
            ensure!(
                a.string(255)?.eq_ignore_ascii_case(&id.host),
                "Diameter destination host"
            );
        }
        ensure!(
            p.num(AUTH_APP)? == 1 && p.num(AUTH_STATE)? == 1,
            "Diameter stateless NASREQ required"
        );
        let session = p.text(SESSION, 512)?;
        session_text(&session)?;
        let r = Self {
            username: p.text(USER, 255)?,
            password: p
                .optional(PASSWORD)?
                .map(|a| a.string(MAX_PASSWORD_BYTES))
                .transpose()?,
            auth_request_type: p.num(AUTH_TYPE)?,
            nas_identifier: p.optional(NAS_ID)?.map(|a| a.string(255)).transpose()?,
            nas_port: p.optional(NAS_PORT)?.map(Avp::integer).transpose()?,
        };
        r.validate()?;
        Ok((r, session))
    }
}
pub const REQUEST_ALLOWED: &[u32] = &[
    SESSION,
    HOST,
    REALM,
    DEST_HOST,
    DEST_REALM,
    AUTH_APP,
    AUTH_TYPE,
    AUTH_STATE,
    USER,
    PASSWORD,
    NAS_ID,
    NAS_PORT,
    ORIGIN_STATE,
];
pub const ANSWER_ALLOWED: &[u32] = &[
    SESSION,
    HOST,
    REALM,
    AUTH_APP,
    AUTH_TYPE,
    AUTH_STATE,
    RESULT,
    USER,
    SERVICE,
    REPLY_MESSAGE,
    FILTER_ID,
    SESSION_TIMEOUT,
    ERROR_MESSAGE,
    FAILED_AVP,
    ORIGIN_STATE,
];
pub fn session_text(v: &str) -> Result<()> {
    ensure!(
        !v.is_empty() && v.len() <= 512 && !v.contains('\0'),
        "Diameter Session-Id bound"
    );
    Ok(())
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Accept,
    Reject,
    Error,
}
impl Default for Verdict {
    fn default() -> Self {
        Self::Reject
    }
}
impl Verdict {
    pub fn code(self) -> u32 {
        match self {
            Self::Accept => 2001,
            Self::Reject => 4001,
            Self::Error => 5012,
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    #[serde(default)]
    pub verdict: Verdict,
    #[serde(default)]
    pub reply_messages: Vec<String>,
    #[serde(default)]
    pub filter_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_type: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_timeout: Option<u32>,
}
impl Reply {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.reply_messages.len() <= 8 && self.filter_ids.len() <= 8,
            "Diameter reply AVP count"
        );
        for s in self.reply_messages.iter().chain(&self.filter_ids) {
            ensure!(
                s.len() <= 1024 && !s.contains('\0'),
                "Diameter reply UTF8 bound"
            );
        }
        if let Some(v) = self.service_type {
            ensure!((1..=19).contains(&v), "Diameter selected Service-Type1..19");
        }
        Ok(())
    }
    pub fn packet(&self, request: &Packet, id: &Identity) -> Result<Packet> {
        self.validate()?;
        let mut a = request.answer(self.verdict.code());
        a.origin(id);
        a.avps
            .insert(0, Avp::text(SESSION, &request.text(SESSION, 512)?));
        a.avps.push(Avp::number(AUTH_APP, 1));
        a.avps.push(Avp::number(AUTH_TYPE, request.num(AUTH_TYPE)?));
        a.avps.push(Avp::number(AUTH_STATE, 1));
        for v in &self.reply_messages {
            a.avps.push(Avp::text(REPLY_MESSAGE, v));
        }
        for v in &self.filter_ids {
            a.avps.push(Avp::text(FILTER_ID, v));
        }
        if let Some(v) = self.service_type {
            a.avps.push(Avp::number(SERVICE, v));
        }
        if let Some(v) = self.session_timeout {
            a.avps.push(Avp::number(SESSION_TIMEOUT, v));
        }
        a.encode()?;
        Ok(a)
    }
}
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Response {
    pub result_code: u32,
    pub accepted: bool,
    pub stateless: bool,
    pub reply_messages: Vec<String>,
    pub filter_ids: Vec<String>,
    pub service_type: Option<u32>,
    pub session_timeout: Option<u32>,
}
impl Response {
    pub fn from_packet(p: &Packet, request: &Packet, peer: &Identity) -> Result<Self> {
        ensure!(p.matches(request), "Diameter answer correlation");
        peer.same_peer(p)?;
        let result = p.num(RESULT)?;
        ensure!((1000..6000).contains(&result), "Diameter Result-Code class");
        ensure!(
            (p.flags & 0x20 != 0) == (3000..4000).contains(&result),
            "Diameter error flag/result category"
        );
        ensure!(
            p.unsupported_mandatory(ANSWER_ALLOWED).is_none(),
            "Diameter unhandled mandatory answer AVP"
        );
        let successful = result == 2001;
        ensure!(
            !successful || p.optional(FAILED_AVP)?.is_none(),
            "Diameter success cannot report a failed AVP"
        );
        if p.optional(SESSION)?.is_some() {
            ensure!(
                p.avps
                    .first()
                    .is_some_and(|a| a.code == SESSION && a.vendor.is_none()),
                "Diameter answer Session-Id fixed first AVP"
            );
        }
        // Protocol error answers (E-bit) use the RFC6733 generic grammar. Other
        // answers must meet the NASREQ grammar even when they deny the request.
        if p.flags & 0x20 == 0 {
            ensure!(
                p.text(SESSION, 512)? == request.text(SESSION, 512)?
                    && p.num(AUTH_APP)? == 1
                    && p.num(AUTH_TYPE)? == request.num(AUTH_TYPE)?,
                "Diameter NASREQ answer fields"
            );
        }
        let stateless = p.optional(AUTH_STATE)?.map(Avp::integer).transpose()? == Some(1);
        ensure!(
            !successful || stateless,
            "Diameter successful answer must agree to stateless scope"
        );
        let strings = |code| -> Result<Vec<String>> {
            let values: Vec<_> = p
                .avps
                .iter()
                .filter(|a| a.code == code && a.vendor.is_none())
                .collect();
            ensure!(values.len() <= 8, "Diameter answer repeated AVP count");
            values.into_iter().map(|a| a.string(1024)).collect()
        };
        let service_type = p.optional(SERVICE)?.map(Avp::integer).transpose()?;
        if let Some(v) = service_type {
            ensure!((1..=19).contains(&v), "Diameter unsupported Service-Type");
        }
        let session_timeout = p.optional(SESSION_TIMEOUT)?.map(Avp::integer).transpose()?;
        Ok(Self {
            result_code: result,
            accepted: successful && stateless,
            stateless,
            reply_messages: strings(REPLY_MESSAGE)?,
            filter_ids: strings(FILTER_ID)?,
            service_type,
            session_timeout,
        })
    }
}
pub fn error_answer(
    request: &Packet,
    id: &Identity,
    result: u32,
    failed: Option<&Avp>,
) -> Result<Packet> {
    let mut p = request.answer(result);
    p.origin(id);
    if request.command == AA {
        if let Ok(v) = request.text(SESSION, 512) {
            p.avps.insert(0, Avp::text(SESSION, &v));
        }
        if !(3000..4000).contains(&result) {
            p.avps.push(Avp::number(AUTH_APP, 1));
            if let Ok(v) = request.num(AUTH_TYPE) {
                p.avps.push(Avp::number(AUTH_TYPE, v));
            }
            p.avps.push(Avp::number(AUTH_STATE, 1));
        }
    }
    if let Some(avp) = failed {
        let mut data = vec![];
        avp.encode(&mut data)?;
        p.avps.push(Avp::bytes(FAILED_AVP, data));
    }
    p.encode()?;
    Ok(p)
}
