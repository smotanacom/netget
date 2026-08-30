//! OpenVPN "key method 2": the first message each side sends inside the TLS
//! control channel.
//!
//! Once the TLS handshake finishes, both peers write a plaintext record into the
//! TLS session laid out like this (`ssl.c`, `key_method_2_write` /
//! `key_method_2_read`):
//!
//! ```text
//! u32       0                        -- literal, historical
//! u8        key method               -- 2, in the low bits
//! [48]      pre-master secret        -- CLIENT ONLY; the server omits it
//! [32]      random1
//! [32]      random2
//! string    options string           -- the sender's OCC options
//! string    username                 -- empty when not doing user/pass auth
//! string    password
//! string    peer info                -- IV_* variables; empty from a server
//! ```
//!
//! A `string` is a `u16` length followed by that many bytes, and the length
//! **includes a trailing NUL**, which is part of the payload. `write_empty_string`
//! emits a length of zero and no bytes at all, which is what a server sends for
//! username, password and peer info — so "empty" and "one NUL byte" are two
//! different encodings and only the first is what the peer expects from us.
//!
//! # What this module does and does not do
//!
//! It parses the client's message and builds the server's answer. It does **not**
//! derive data-channel keys from the exchanged material: that needs the TLS
//! PRF over the combined key sources, and the data channel it would key does
//! not exist. `pre_master` is deliberately never copied out of the parsed
//! struct into anything persistent.

use anyhow::{bail, Result};

/// The only key method this server understands.
pub const KEY_METHOD_2: u8 = 2;

/// `key_source2` field sizes, from `ssl_common.h`.
const PRE_MASTER_LEN: usize = 48;
const RANDOM_LEN: usize = 32;

/// Refuse absurd string lengths early. OpenVPN's own cap on the options string
/// is `TLS_OPTIONS_LEN` (512); usernames and peer info are smaller still, and
/// the whole message arrives inside one TLS record.
const MAX_STRING_LEN: usize = 4096;

/// The client's key-method-2 message, as parsed off the control channel.
#[derive(Debug, Clone)]
pub struct ClientKeyMethod2 {
    /// Client entropy. Half of the data-channel key material, and a secret:
    /// never logged, never put in event data.
    pub pre_master: Vec<u8>,
    pub random1: Vec<u8>,
    pub random2: Vec<u8>,
    /// The client's OCC options string, e.g.
    /// `V4,dev-type tun,link-mtu 1549,...,tls-client`.
    pub options: String,
    /// `--auth-user-pass` username, or empty.
    pub username: String,
    /// `--auth-user-pass` password, or empty.
    pub password: String,
    /// `IV_*` variables the client advertises: version, platform, protocol
    /// flags, supported data ciphers.
    pub peer_info: String,
}

/// Read a `u16`-prefixed, NUL-terminated string.
///
/// Returns `Ok(None)` when the buffer does not yet hold the whole string, which
/// is not an error: the control channel is a stream and the rest may still be in
/// flight.
fn read_string(buf: &[u8], pos: &mut usize) -> Result<Option<String>> {
    if buf.len() < *pos + 2 {
        return Ok(None);
    }
    let len = u16::from_be_bytes([buf[*pos], buf[*pos + 1]]) as usize;
    if len > MAX_STRING_LEN {
        bail!("control-channel string length {} is implausible", len);
    }
    if buf.len() < *pos + 2 + len {
        return Ok(None);
    }
    let start = *pos + 2;
    let bytes = &buf[start..start + len];
    *pos = start + len;
    // The length counts the NUL terminator; strip it rather than carrying it
    // into a Rust string.
    let bytes = match bytes.last() {
        Some(0) => &bytes[..bytes.len() - 1],
        _ => bytes,
    };
    Ok(Some(String::from_utf8_lossy(bytes).into_owned()))
}

fn write_string(out: &mut Vec<u8>, s: &str) {
    let len = s.len() + 1;
    out.extend_from_slice(&(len as u16).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
    out.push(0);
}

/// `write_empty_string`: a zero length and no bytes. Not the same as a string
/// containing only a NUL.
fn write_empty_string(out: &mut Vec<u8>) {
    out.extend_from_slice(&0u16.to_be_bytes());
}

/// Parse a client key-method-2 message.
///
/// * `Ok(Some((msg, consumed)))` — a whole message was read.
/// * `Ok(None)` — the buffer is a prefix of one; wait for more control packets.
/// * `Err(_)` — the bytes cannot be a key-method-2 message at all.
pub fn parse_client_key_method_2(buf: &[u8]) -> Result<Option<(ClientKeyMethod2, usize)>> {
    let fixed = 4 + 1 + PRE_MASTER_LEN + RANDOM_LEN + RANDOM_LEN;
    if buf.len() < fixed {
        return Ok(None);
    }

    // The leading u32 is a literal zero in every version that speaks key
    // method 2. Anything else means we are not looking at one.
    let leading = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if leading != 0 {
        bail!(
            "control channel does not start with a key-method-2 message (leading u32 is {:#010x})",
            leading
        );
    }

    // OpenVPN masks off the high bits, which carry flags rather than the method.
    let key_method = buf[4] & 0x07;
    if key_method != KEY_METHOD_2 {
        bail!(
            "unsupported OpenVPN key method {} (only key method 2 exists in any supported release)",
            key_method
        );
    }

    let mut pos = 5;
    let pre_master = buf[pos..pos + PRE_MASTER_LEN].to_vec();
    pos += PRE_MASTER_LEN;
    let random1 = buf[pos..pos + RANDOM_LEN].to_vec();
    pos += RANDOM_LEN;
    let random2 = buf[pos..pos + RANDOM_LEN].to_vec();
    pos += RANDOM_LEN;

    // All four strings are always written, so a missing one means the message is
    // still arriving rather than that the client omitted it.
    let options = match read_string(buf, &mut pos)? {
        Some(s) => s,
        None => return Ok(None),
    };
    let username = match read_string(buf, &mut pos)? {
        Some(s) => s,
        None => return Ok(None),
    };
    let password = match read_string(buf, &mut pos)? {
        Some(s) => s,
        None => return Ok(None),
    };
    let peer_info = match read_string(buf, &mut pos)? {
        Some(s) => s,
        None => return Ok(None),
    };

    Ok(Some((
        ClientKeyMethod2 {
            pre_master,
            random1,
            random2,
            options,
            username,
            password,
            peer_info,
        },
        pos,
    )))
}

/// Build the server's key-method-2 answer.
///
/// The server sends `random1` and `random2` but **no pre-master secret** —
/// `key_source2_randomize_write` writes one only when it is the client — and
/// then an empty username, password and peer info, exactly as a real server
/// with no `--auth-user-pass-verify` does.
pub fn build_server_key_method_2(random1: &[u8; 32], random2: &[u8; 32], options: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(128 + options.len());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.push(KEY_METHOD_2);
    out.extend_from_slice(random1);
    out.extend_from_slice(random2);
    write_string(&mut out, options);
    write_empty_string(&mut out); // username
    write_empty_string(&mut out); // password
    write_empty_string(&mut out); // peer info
    out
}

/// Derive the options string to answer with from the one the client sent.
///
/// The peer compares the two and warns about every field that differs, so
/// mirroring the client's string and flipping only `tls-client` to `tls-server`
/// produces a clean log. A client that sent nothing usable gets a minimal but
/// well-formed V4 string instead of an empty one, which OpenVPN treats as a
/// protocol error rather than as "no opinion".
pub fn server_options_from_client(client_options: &str) -> String {
    const FALLBACK: &str = "V4,dev-type tun,link-mtu 1549,tun-mtu 1500,proto UDPv4,\
                            cipher AES-256-GCM,auth SHA1,keysize 256,key-method 2,tls-server";

    if client_options.is_empty() {
        return FALLBACK.to_string();
    }
    match client_options.strip_suffix("tls-client") {
        Some(head) => format!("{}tls-server", head),
        None => client_options.to_string(),
    }
}

/// Peer-info lines (`IV_VER=...`) as a JSON-friendly map, for event data.
pub fn peer_info_map(peer_info: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    for line in peer_info.split(['\n', '\0']) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match line.split_once('=') {
            Some((k, v)) => {
                map.insert(k.to_string(), serde_json::Value::String(v.to_string()));
            }
            None => {
                map.insert(line.to_string(), serde_json::Value::Bool(true));
            }
        }
    }
    map
}
