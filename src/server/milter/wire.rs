//! The milter protocol (libmilter's wire format, version 6), shared by the filter (server) and
//! the MTA side (client): length-prefixed packets of a command byte and NUL-separated fields.
use anyhow::{bail, ensure, Context, Result};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Largest packet accepted (libmilter's own limit for a body chunk is 64 KiB, a header line
/// is far smaller).
pub const MAX_PACKET: usize = 256 * 1024;
/// Largest message body collected for the end-of-message decision.
pub const MAX_BODY: usize = 1024 * 1024;
/// Most headers collected for one message.
pub const MAX_HEADERS: usize = 512;
pub const IO_TIMEOUT: Duration = Duration::from_secs(30);
pub const VERSION: u32 = 6;

// MTA → filter commands.
pub const C_ABORT: u8 = b'A';
pub const C_BODY: u8 = b'B';
pub const C_CONNECT: u8 = b'C';
pub const C_MACRO: u8 = b'D';
pub const C_BODYEOB: u8 = b'E';
pub const C_HELO: u8 = b'H';
pub const C_QUIT_NC: u8 = b'K';
pub const C_HEADER: u8 = b'L';
pub const C_MAIL: u8 = b'M';
pub const C_EOH: u8 = b'N';
pub const C_OPTNEG: u8 = b'O';
pub const C_QUIT: u8 = b'Q';
pub const C_RCPT: u8 = b'R';
pub const C_DATA: u8 = b'T';
pub const C_UNKNOWN: u8 = b'U';

// Filter → MTA replies.
pub const R_ADDRCPT: u8 = b'+';
pub const R_DELRCPT: u8 = b'-';
pub const R_ACCEPT: u8 = b'a';
pub const R_REPLBODY: u8 = b'b';
pub const R_CONTINUE: u8 = b'c';
pub const R_DISCARD: u8 = b'd';
pub const R_ADDHEADER: u8 = b'h';
pub const R_CHGHEADER: u8 = b'm';
pub const R_PROGRESS: u8 = b'p';
pub const R_QUARANTINE: u8 = b'q';
pub const R_REJECT: u8 = b'r';
pub const R_TEMPFAIL: u8 = b't';
pub const R_REPLYCODE: u8 = b'y';
pub const R_SKIP: u8 = b's';

// Actions a filter may take (SMFIF_*).
pub const F_ADDHDRS: u32 = 0x01;
pub const F_CHGBODY: u32 = 0x02;
pub const F_ADDRCPT: u32 = 0x04;
pub const F_DELRCPT: u32 = 0x08;
pub const F_CHGHDRS: u32 = 0x10;
pub const F_QUARANTINE: u32 = 0x20;
pub const ALL_ACTIONS: u32 =
    F_ADDHDRS | F_CHGBODY | F_ADDRCPT | F_DELRCPT | F_CHGHDRS | F_QUARANTINE;

// Protocol steps a filter may ask the MTA to leave out (SMFIP_NO*) or not to wait for a reply
// to (SMFIP_NR_*), and SMFIP_SKIP: the filter may answer a body chunk with SMFIR_SKIP.
pub const P_NOCONNECT: u32 = 0x01;
pub const P_NOHELO: u32 = 0x02;
pub const P_NOMAIL: u32 = 0x04;
pub const P_NORCPT: u32 = 0x08;
pub const P_NOBODY: u32 = 0x10;
pub const P_NOHDRS: u32 = 0x20;
pub const P_NOEOH: u32 = 0x40;
pub const P_NR_HDR: u32 = 0x80;
pub const P_NODATA: u32 = 0x200;
pub const P_SKIP: u32 = 0x400;
pub const P_NR_CONN: u32 = 0x1000;
pub const P_NR_HELO: u32 = 0x2000;
pub const P_NR_MAIL: u32 = 0x4000;
pub const P_NR_RCPT: u32 = 0x8000;
pub const P_NR_DATA: u32 = 0x10000;
pub const P_NR_EOH: u32 = 0x40000;
pub const P_NR_BODY: u32 = 0x80000;
/// Every protocol step the MTA side can honour, with its name in events.
pub const PROTOCOL_STEPS: [(u32, &str); 17] = [
    (P_NOCONNECT, "no_connect"),
    (P_NOHELO, "no_helo"),
    (P_NOMAIL, "no_mail"),
    (P_NORCPT, "no_rcpt"),
    (P_NOBODY, "no_body"),
    (P_NOHDRS, "no_headers"),
    (P_NOEOH, "no_eoh"),
    (P_NR_HDR, "no_reply_header"),
    (P_NODATA, "no_data"),
    (P_SKIP, "skip"),
    (P_NR_CONN, "no_reply_connect"),
    (P_NR_HELO, "no_reply_helo"),
    (P_NR_MAIL, "no_reply_mail"),
    (P_NR_RCPT, "no_reply_rcpt"),
    (P_NR_DATA, "no_reply_data"),
    (P_NR_EOH, "no_reply_eoh"),
    (P_NR_BODY, "no_reply_body"),
];
pub const OFFERED_PROTOCOL: u32 = P_NOCONNECT
    | P_NOHELO
    | P_NOMAIL
    | P_NORCPT
    | P_NOBODY
    | P_NOHDRS
    | P_NOEOH
    | P_NR_HDR
    | P_NODATA
    | P_SKIP
    | P_NR_CONN
    | P_NR_HELO
    | P_NR_MAIL
    | P_NR_RCPT
    | P_NR_DATA
    | P_NR_EOH
    | P_NR_BODY;

pub async fn read_packet<R: AsyncRead + Unpin>(
    r: &mut R,
    idle: Duration,
) -> Result<Option<(u8, Vec<u8>)>> {
    let mut len = [0u8; 4];
    match tokio::time::timeout(idle, r.read(&mut len[..1])).await {
        Err(_) => bail!("peer idle past its deadline"),
        Ok(Ok(0)) => return Ok(None),
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return Err(e.into()),
    }
    tokio::time::timeout(IO_TIMEOUT, async {
        r.read_exact(&mut len[1..]).await?;
        let n = u32::from_be_bytes(len) as usize;
        ensure!(
            (1..=MAX_PACKET).contains(&n),
            "packet length {n} out of range"
        );
        let mut buf = vec![0u8; n];
        r.read_exact(&mut buf).await?;
        let cmd = buf.remove(0);
        Ok(Some((cmd, buf)))
    })
    .await
    .context("packet not completed in time")?
}

pub fn packet(cmd: u8, data: &[u8]) -> Vec<u8> {
    let mut out = ((data.len() + 1) as u32).to_be_bytes().to_vec();
    out.push(cmd);
    out.extend(data);
    out
}

pub async fn write<W: AsyncWrite + Unpin>(w: &mut W, cmd: u8, data: &[u8]) -> Result<()> {
    tokio::time::timeout(IO_TIMEOUT, w.write_all(&packet(cmd, data)))
        .await
        .context("write deadline")??;
    Ok(())
}

/// NUL-terminated strings, as fields.
pub fn strings(data: &[u8]) -> Vec<String> {
    let mut parts: Vec<String> = data
        .split(|b| *b == 0)
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    if data.last() == Some(&0) {
        parts.pop();
    }
    parts
}

pub fn cstrings(fields: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for f in fields {
        out.extend(f.as_bytes());
        out.push(0);
    }
    out
}

/// A field from the handler: one line, no NUL.
pub fn check_field(s: &str, what: &str) -> Result<()> {
    ensure!(
        s.len() <= 4096 && !s.contains(['\0', '\r', '\n']),
        "{what} must be one line without NUL, at most 4096 bytes"
    );
    Ok(())
}

/// OPTNEG body: version, actions, protocol flags.
pub fn optneg(version: u32, actions: u32, protocol: u32) -> Vec<u8> {
    let mut out = version.to_be_bytes().to_vec();
    out.extend(actions.to_be_bytes());
    out.extend(protocol.to_be_bytes());
    out
}

pub fn parse_optneg(d: &[u8]) -> Result<(u32, u32, u32)> {
    ensure!(d.len() >= 12, "short OPTNEG");
    let n = |i: usize| u32::from_be_bytes(d[i..i + 4].try_into().unwrap());
    Ok((n(0), n(4), n(8)))
}

/// CONNECT body: hostname, family, port, address.
pub fn parse_connect(d: &[u8]) -> Result<(String, char, u16, String)> {
    let nul = d
        .iter()
        .position(|b| *b == 0)
        .context("CONNECT without a hostname")?;
    let host = String::from_utf8_lossy(&d[..nul]).into_owned();
    let rest = &d[nul + 1..];
    let family = *rest.first().context("CONNECT without a family")? as char;
    if family == 'U' || rest.len() < 3 {
        return Ok((host, family, 0, String::new()));
    }
    let port = u16::from_be_bytes([rest[1], rest[2]]);
    let addr = strings(&rest[3..]).into_iter().next().unwrap_or_default();
    Ok((host, family, port, addr))
}

pub fn connect_body(host: &str, family: char, port: u16, addr: &str) -> Vec<u8> {
    let mut out = cstrings(&[host]);
    out.push(family as u8);
    out.extend(port.to_be_bytes());
    out.extend(cstrings(&[addr]));
    out
}

/// A reply's name as events and logs report it.
pub fn reply_name(code: u8) -> &'static str {
    match code {
        R_ACCEPT => "accept",
        R_CONTINUE => "continue",
        R_DISCARD => "discard",
        R_REJECT => "reject",
        R_TEMPFAIL => "tempfail",
        R_REPLYCODE => "replycode",
        R_ADDHEADER => "add_header",
        R_CHGHEADER => "change_header",
        R_ADDRCPT => "add_rcpt",
        R_DELRCPT => "del_rcpt",
        R_REPLBODY => "replace_body",
        R_QUARANTINE => "quarantine",
        R_PROGRESS => "progress",
        R_SKIP => "skip",
        _ => "unknown",
    }
}

/// A REPLYCODE body: "550 5.7.1 text".
pub fn replycode(code: u64, xcode: Option<&str>, text: &str) -> Result<Vec<u8>> {
    ensure!((400..600).contains(&code), "reply code must be 4xx or 5xx");
    check_field(text, "text")?;
    let line = match xcode {
        Some(x) => {
            check_field(x, "xcode")?;
            ensure!(
                x.starts_with(if code < 500 { '4' } else { '5' }),
                "xcode class must match the code"
            );
            format!("{code} {x} {text}")
        }
        None => format!("{code} {text}"),
    };
    Ok(cstrings(&[&line]))
}
