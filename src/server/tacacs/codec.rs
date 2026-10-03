//! Selected RFC8907 legacy packets. Bodies remain internal; events use typed fields.
use anyhow::{bail, ensure, Context, Result};
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
pub const MAX_BODY_BYTES: usize = 16 * 1024;
pub const MAX_ARGUMENTS: usize = 32;
pub const MAX_TEXT_BYTES: usize = 255;
pub const MAX_MESSAGE_BYTES: usize = 1024;
pub const MAX_AUTH_ROUNDS: usize = 8;
pub const DEFAULT_IO_SECONDS: u64 = 30;
pub const DEFAULT_HANDLER_SECONDS: u64 = 30;
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_LLM_FALLBACK: bool = false;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub version: u8,
    pub kind: u8,
    pub sequence: u8,
    pub flags: u8,
    pub session_id: u32,
    pub length: usize,
}
impl Header {
    pub fn parse(b: &[u8]) -> Result<Self> {
        ensure!(b.len() == 12, "header length");
        let h = Self {
            version: b[0],
            kind: b[1],
            sequence: b[2],
            flags: b[3],
            session_id: u32::from_be_bytes(b[4..8].try_into()?),
            length: u32::from_be_bytes(b[8..12].try_into()?) as usize,
        };
        ensure!(h.length <= MAX_BODY_BYTES, "body capacity");
        Ok(h)
    }
    pub fn bytes(self) -> Result<[u8; 12]> {
        ensure!(self.length <= MAX_BODY_BYTES, "body capacity");
        let mut b = [
            self.version,
            self.kind,
            self.sequence,
            self.flags,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        b[4..8].copy_from_slice(&self.session_id.to_be_bytes());
        b[8..12].copy_from_slice(&(self.length as u32).to_be_bytes());
        Ok(b)
    }
    pub fn validate(self) -> Result<()> {
        ensure!(matches!(self.version, 0xc0 | 0xc1), "unsupported version");
        ensure!(matches!(self.kind, 1..=3), "unsupported type");
        ensure!(self.sequence != 0, "sequence zero");
        ensure!(self.flags & 1 == 0, "legacy requires obfuscated body");
        Ok(())
    }
    pub fn reply(self) -> Result<Self> {
        Ok(Self {
            sequence: self
                .sequence
                .checked_add(1)
                .context("sequence must never wrap")?,
            flags: 0,
            length: 0,
            ..self
        })
    }
}
pub fn validate_secret(secret: &str) -> Result<()> {
    ensure!(
        !secret.is_empty() && secret.len() <= MAX_TEXT_BYTES,
        "secret must have1..255 bytes"
    );
    Ok(())
}
pub fn obfuscate(header: Header, body: &[u8], secret: &[u8]) -> Vec<u8> {
    let mut prefix = Vec::with_capacity(secret.len() + 6);
    prefix.extend(header.session_id.to_be_bytes());
    prefix.extend(secret);
    prefix.extend([header.version, header.sequence]);
    let mut previous = Vec::new();
    let mut result = body.to_vec();
    for chunk in result.chunks_mut(16) {
        let mut hash = Md5::new();
        hash.update(&prefix);
        hash.update(&previous);
        previous = hash.finalize().to_vec();
        for (byte, pad) in chunk.iter_mut().zip(&previous) {
            *byte ^= *pad;
        }
    }
    result
}
pub fn packet(mut header: Header, body: &[u8], secret: &[u8]) -> Result<Vec<u8>> {
    ensure!(body.len() <= MAX_BODY_BYTES, "body capacity");
    header.length = body.len();
    let mut bytes = header.bytes()?.to_vec();
    bytes.extend(obfuscate(header, body, secret));
    Ok(bytes)
}
pub async fn read_packet<R: AsyncRead + Unpin>(
    reader: &mut R,
    secret: &[u8],
    timeout: Duration,
) -> Result<(Header, Vec<u8>)> {
    tokio::time::timeout(timeout, async {
        let mut bytes = [0; 12];
        reader.read_exact(&mut bytes).await?;
        let header = Header::parse(&bytes)?;
        let mut body = vec![0; header.length];
        reader.read_exact(&mut body).await?;
        Ok((header, obfuscate(header, &body, secret)))
    })
    .await
    .context("packet deadline")?
}
pub async fn write_packet<W: AsyncWrite + Unpin>(
    writer: &mut W,
    header: Header,
    body: &[u8],
    secret: &[u8],
) -> Result<usize> {
    let bytes = packet(header, body, secret)?;
    tokio::time::timeout(WRITE_TIMEOUT, writer.write_all(&bytes))
        .await
        .context("write deadline")??;
    Ok(bytes.len())
}
macro_rules! enumeration {($name:ident {$($variant:ident=$value:expr),+$(,)?})=>{
 #[derive(Clone,Copy,Debug,Serialize,Deserialize,PartialEq,Eq)]#[serde(rename_all="snake_case")]
 pub enum $name{$($variant),+}
 impl $name {pub fn byte(self)->u8{match self{$(Self::$variant=>$value),+}} pub fn parse(value:u8)->Result<Self>{match value{$($value=>Ok(Self::$variant)),+, _=>bail!(concat!(stringify!($name)," enum"))}}}
}}
enumeration!(AuthType{NotSet=0,Ascii=1,Pap=2,Chap=3,MsChap=5,MsChapV2=6});
enumeration!(Service{None=0,Login=1,Enable=2,Ppp=3,Pt=5,Rcmd=6,X25=7,Nasi=8,FwProxy=9});
enumeration!(AuthMethod{NotSet=0,None=1,Krb5=2,Line=3,Enable=4,Local=5,TacacsPlus=6,Guest=8,Radius=16,Krb4=17,Rcmd=32});
enumeration!(AuthStatus{Pass=1,Fail=2,GetData=3,GetUser=4,GetPass=5,Restart=6,Error=7,Follow=33});
enumeration!(AuthorStatus{PassAdd=1,PassReplace=2,Fail=16,Error=17,Follow=33});
enumeration!(AccountStatus{Success=1,Error=2,Follow=33});
enumeration!(AccountKind{Start=2,Stop=4,Watchdog=8,Update=10});
fn login() -> Service {
    Service::Login
}
fn ascii() -> AuthType {
    AuthType::Ascii
}
fn tacacs() -> AuthMethod {
    AuthMethod::TacacsPlus
}
fn privilege() -> u8 {
    1
}
fn mandatory() -> bool {
    true
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Argument {
    pub name: String,
    pub value: String,
    #[serde(default = "mandatory")]
    pub mandatory: bool,
}
impl Argument {
    pub fn wire(&self) -> Result<String> {
        text(&self.name, MAX_TEXT_BYTES)?;
        ensure!(
            !self.name.is_empty() && !self.name.contains(['=', '*']),
            "argument name"
        );
        text(&self.value, MAX_TEXT_BYTES)?;
        let v = format!(
            "{}{}{}",
            self.name,
            if self.mandatory { '=' } else { '*' },
            self.value
        );
        ensure!(v.len() <= MAX_TEXT_BYTES, "argument byte capacity");
        Ok(v)
    }
    pub fn parse(value: &str) -> Result<Self> {
        text(value, MAX_TEXT_BYTES)?;
        let i = value.find(['=', '*']).context("argument separator")?;
        ensure!(i > 0, "argument name");
        Ok(Self {
            name: value[..i].into(),
            value: value[i + 1..].into(),
            mandatory: value.as_bytes()[i] == b'=',
        })
    }
}
/// Password-bearing commands deliberately have no Debug implementation.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Authentication {
    pub username: String,
    pub password: String,
    #[serde(default = "ascii")]
    pub method: AuthType,
    #[serde(default = "privilege")]
    pub privilege_level: u8,
    #[serde(default)]
    pub port: String,
    #[serde(default)]
    pub remote_address: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(default = "tacacs")]
    pub authentication_method: AuthMethod,
    #[serde(default = "privilege")]
    pub privilege_level: u8,
    #[serde(default = "ascii")]
    pub authentication_type: AuthType,
    #[serde(default = "login")]
    pub service: Service,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub port: String,
    #[serde(default)]
    pub remote_address: String,
    #[serde(default)]
    pub arguments: Vec<Argument>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Accounting {
    pub record_type: AccountKind,
    pub request: Request,
}
#[derive(Clone)]
pub struct Start {
    pub action: u8,
    pub privilege_level: u8,
    pub method: AuthType,
    pub service: Service,
    pub username: String,
    pub port: String,
    pub remote_address: String,
    pub password: Option<String>,
}
#[derive(Clone)]
pub struct Continue {
    pub user_message: String,
    pub abort: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthReply {
    pub status: AuthStatus,
    #[serde(default)]
    pub no_echo: bool,
    #[serde(default)]
    pub server_message: String,
    #[serde(default)]
    pub data: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorReply {
    pub status: AuthorStatus,
    #[serde(default)]
    pub arguments: Vec<Argument>,
    #[serde(default)]
    pub server_message: String,
    #[serde(default)]
    pub data: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountReply {
    pub status: AccountStatus,
    #[serde(default)]
    pub server_message: String,
    #[serde(default)]
    pub data: String,
}
pub fn text(value: &str, maximum: usize) -> Result<()> {
    ensure!(
        value.len() <= maximum && value.bytes().all(|b| matches!(b, 32..=126)),
        "printable ASCII text or capacity"
    );
    Ok(())
}
pub fn username(value: &str) -> Result<()> {
    text(value, MAX_TEXT_BYTES)?;
    ensure!(!value.contains(' '), "selected username has no spaces");
    Ok(())
}
pub fn request_valid(r: &Request) -> Result<()> {
    ensure!(r.privilege_level <= 15, "privilege0..15");
    username(&r.username)?;
    text(&r.port, MAX_TEXT_BYTES)?;
    text(&r.remote_address, MAX_TEXT_BYTES)?;
    ensure!(r.arguments.len() <= MAX_ARGUMENTS, "argument capacity32");
    for a in &r.arguments {
        a.wire()?;
    }
    Ok(())
}
struct Cursor<'a> {
    b: &'a [u8],
    i: usize,
}
impl<'a> Cursor<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, i: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.i.checked_add(n).context("length overflow")?;
        let v = self.b.get(self.i..end).context("truncated body")?;
        self.i = end;
        Ok(v)
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<usize> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into()?) as usize)
    }
    fn string(&mut self, n: usize, max: usize) -> Result<String> {
        ensure!(n <= max, "text capacity");
        let s = std::str::from_utf8(self.take(n)?)?.to_string();
        text(&s, max)?;
        Ok(s)
    }
    fn end(self) -> Result<()> {
        ensure!(self.i == self.b.len(), "trailing body");
        Ok(())
    }
}
pub fn parse_start(body: &[u8]) -> Result<Start> {
    let mut c = Cursor::new(body);
    let action = c.byte()?;
    ensure!(matches!(action, 1 | 2 | 4), "authentication action enum");
    let privilege_level = c.byte()?;
    ensure!(privilege_level <= 15, "privilege0..15");
    let method = AuthType::parse(c.byte()?)?;
    ensure!(method != AuthType::NotSet, "start authentication type");
    let service = Service::parse(c.byte()?)?;
    let lengths = [c.byte()?, c.byte()?, c.byte()?, c.byte()?];
    let username = c.string(lengths[0] as usize, MAX_TEXT_BYTES)?;
    self::username(&username)?;
    let port = c.string(lengths[1] as usize, MAX_TEXT_BYTES)?;
    let remote_address = c.string(lengths[2] as usize, MAX_TEXT_BYTES)?;
    let data = c.take(lengths[3] as usize)?;
    let password = if method == AuthType::Pap {
        let s = std::str::from_utf8(data)?.to_string();
        text(&s, MAX_TEXT_BYTES)?;
        Some(s)
    } else {
        None
    };
    c.end()?;
    Ok(Start {
        action,
        privilege_level,
        method,
        service,
        username,
        port,
        remote_address,
        password,
    })
}
pub fn authentication_body(a: &Authentication) -> Result<(u8, Vec<u8>)> {
    ensure!(
        matches!(a.method, AuthType::Ascii | AuthType::Pap),
        "only ASCII/PAP login"
    );
    username(&a.username)?;
    ensure!(!a.username.is_empty(), "username required");
    text(&a.password, MAX_TEXT_BYTES)?;
    text(&a.port, MAX_TEXT_BYTES)?;
    text(&a.remote_address, MAX_TEXT_BYTES)?;
    ensure!(a.privilege_level <= 15, "privilege0..15");
    let data = if a.method == AuthType::Pap {
        a.password.as_bytes()
    } else {
        &[]
    };
    let mut b = vec![
        1,
        a.privilege_level,
        a.method.byte(),
        1,
        a.username.len() as u8,
        a.port.len() as u8,
        a.remote_address.len() as u8,
        data.len() as u8,
    ];
    b.extend(a.username.as_bytes());
    b.extend(a.port.as_bytes());
    b.extend(a.remote_address.as_bytes());
    b.extend(data);
    Ok((
        if a.method == AuthType::Pap {
            0xc1
        } else {
            0xc0
        },
        b,
    ))
}
pub fn parse_continue(body: &[u8]) -> Result<Continue> {
    let mut c = Cursor::new(body);
    let u = c.u16()?;
    let d = c.u16()?;
    let flags = c.byte()?;
    let user_message = c.string(u, MAX_TEXT_BYTES)?;
    c.take(d)?;
    c.end()?;
    Ok(Continue {
        user_message,
        abort: flags & 1 != 0,
    })
}
pub fn continue_body(value: &str, abort: bool) -> Result<Vec<u8>> {
    text(value, MAX_TEXT_BYTES)?;
    let mut b = if abort {
        vec![0, 0]
    } else {
        (value.len() as u16).to_be_bytes().to_vec()
    };
    b.extend(if abort {
        (value.len() as u16).to_be_bytes()
    } else {
        [0, 0]
    });
    b.push(u8::from(abort));
    b.extend(value.as_bytes());
    Ok(b)
}
pub fn request_body(r: &Request, account: Option<AccountKind>) -> Result<Vec<u8>> {
    request_valid(r)?;
    let mut b = Vec::new();
    if let Some(k) = account {
        b.push(k.byte());
    }
    b.extend([
        r.authentication_method.byte(),
        r.privilege_level,
        r.authentication_type.byte(),
        r.service.byte(),
        r.username.len() as u8,
        r.port.len() as u8,
        r.remote_address.len() as u8,
        r.arguments.len() as u8,
    ]);
    let args = r
        .arguments
        .iter()
        .map(Argument::wire)
        .collect::<Result<Vec<_>>>()?;
    b.extend(args.iter().map(|s| s.len() as u8));
    b.extend(r.username.as_bytes());
    b.extend(r.port.as_bytes());
    b.extend(r.remote_address.as_bytes());
    for a in args {
        b.extend(a.as_bytes());
    }
    ensure!(b.len() <= MAX_BODY_BYTES, "body capacity");
    Ok(b)
}
pub fn parse_request(body: &[u8], account: bool) -> Result<(Request, Option<AccountKind>)> {
    let mut c = Cursor::new(body);
    let kind = if account {
        Some(AccountKind::parse(c.byte()? & 0x0e)?)
    } else {
        None
    };
    let authentication_method = AuthMethod::parse(c.byte()?)?;
    let privilege_level = c.byte()?;
    let authentication_type = AuthType::parse(c.byte()?)?;
    let service = Service::parse(c.byte()?)?;
    let u = c.byte()? as usize;
    let p = c.byte()? as usize;
    let r = c.byte()? as usize;
    let count = c.byte()? as usize;
    ensure!(count <= MAX_ARGUMENTS, "argument capacity32");
    let lengths = c.take(count)?.to_vec();
    let username = c.string(u, MAX_TEXT_BYTES)?;
    let port = c.string(p, MAX_TEXT_BYTES)?;
    let remote_address = c.string(r, MAX_TEXT_BYTES)?;
    let mut arguments = Vec::with_capacity(count);
    for length in lengths {
        arguments.push(Argument::parse(
            &c.string(length as usize, MAX_TEXT_BYTES)?,
        )?);
    }
    c.end()?;
    let request = Request {
        authentication_method,
        privilege_level,
        authentication_type,
        service,
        username,
        port,
        remote_address,
        arguments,
    };
    request_valid(&request)?;
    Ok((request, kind))
}
fn reply_text(message: &str, data: &str) -> Result<()> {
    text(message, MAX_MESSAGE_BYTES)?;
    text(data, MAX_MESSAGE_BYTES)
}
pub fn auth_reply_body(r: &AuthReply) -> Result<Vec<u8>> {
    reply_text(&r.server_message, &r.data)?;
    let mut b = vec![r.status.byte(), u8::from(r.no_echo)];
    b.extend((r.server_message.len() as u16).to_be_bytes());
    b.extend((r.data.len() as u16).to_be_bytes());
    b.extend(r.server_message.as_bytes());
    b.extend(r.data.as_bytes());
    Ok(b)
}
pub fn parse_auth_reply(body: &[u8]) -> Result<AuthReply> {
    let mut c = Cursor::new(body);
    let status = AuthStatus::parse(c.byte()?)?;
    let no_echo = c.byte()? & 1 != 0;
    let m = c.u16()?;
    let d = c.u16()?;
    let server_message = c.string(m, MAX_MESSAGE_BYTES)?;
    let data = c.string(d, MAX_MESSAGE_BYTES)?;
    c.end()?;
    Ok(AuthReply {
        status,
        no_echo,
        server_message,
        data,
    })
}
pub fn author_reply_body(r: &AuthorReply) -> Result<Vec<u8>> {
    reply_text(&r.server_message, &r.data)?;
    ensure!(r.arguments.len() <= MAX_ARGUMENTS, "argument capacity32");
    ensure!(
        r.status != AuthorStatus::Follow || r.arguments.is_empty(),
        "FOLLOW arguments"
    );
    let args = r
        .arguments
        .iter()
        .map(Argument::wire)
        .collect::<Result<Vec<_>>>()?;
    let mut b = vec![r.status.byte(), r.arguments.len() as u8];
    b.extend((r.server_message.len() as u16).to_be_bytes());
    b.extend((r.data.len() as u16).to_be_bytes());
    b.extend(args.iter().map(|a| a.len() as u8));
    b.extend(r.server_message.as_bytes());
    b.extend(r.data.as_bytes());
    for a in args {
        b.extend(a.as_bytes());
    }
    Ok(b)
}
pub fn parse_author_reply(body: &[u8]) -> Result<AuthorReply> {
    let mut c = Cursor::new(body);
    let status = AuthorStatus::parse(c.byte()?)?;
    let count = c.byte()? as usize;
    ensure!(count <= MAX_ARGUMENTS, "argument capacity32");
    let m = c.u16()?;
    let d = c.u16()?;
    let lengths = c.take(count)?.to_vec();
    let server_message = c.string(m, MAX_MESSAGE_BYTES)?;
    let data = c.string(d, MAX_MESSAGE_BYTES)?;
    let mut arguments = Vec::with_capacity(count);
    for n in lengths {
        arguments.push(Argument::parse(&c.string(n as usize, MAX_TEXT_BYTES)?)?);
    }
    c.end()?;
    ensure!(
        status != AuthorStatus::Follow || arguments.is_empty(),
        "FOLLOW arguments"
    );
    Ok(AuthorReply {
        status,
        arguments,
        server_message,
        data,
    })
}
pub fn account_reply_body(r: &AccountReply) -> Result<Vec<u8>> {
    reply_text(&r.server_message, &r.data)?;
    let mut b = (r.server_message.len() as u16).to_be_bytes().to_vec();
    b.extend((r.data.len() as u16).to_be_bytes());
    b.push(r.status.byte());
    b.extend(r.server_message.as_bytes());
    b.extend(r.data.as_bytes());
    Ok(b)
}
pub fn parse_account_reply(body: &[u8]) -> Result<AccountReply> {
    let mut c = Cursor::new(body);
    let m = c.u16()?;
    let d = c.u16()?;
    let status = AccountStatus::parse(c.byte()?)?;
    let server_message = c.string(m, MAX_MESSAGE_BYTES)?;
    let data = c.string(d, MAX_MESSAGE_BYTES)?;
    c.end()?;
    Ok(AccountReply {
        status,
        server_message,
        data,
    })
}

pub const MAX_JSON_BYTES: usize = 128 * 1024;
pub const MAX_JSON_NODES: usize = 1024;
pub const MAX_JSON_DEPTH: usize = 8;
pub fn within_json_budget(value: &serde_json::Value) -> bool {
    crate::utils::json_budget::within_budget(value, MAX_JSON_BYTES, MAX_JSON_NODES, MAX_JSON_DEPTH)
}
pub fn owned_json(value: serde_json::Value) -> Result<serde_json::Value> {
    if within_json_budget(&value) {
        Ok(value)
    } else {
        crate::utils::json_budget::drop_iteratively(value);
        bail!("TACACS JSON depth/node/retained-content budget")
    }
}
