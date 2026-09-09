//! Git pkt-line framing and `git-upload-pack` request parsing.
//!
//! A pkt-line is a 4-digit hex length (counting the 4 bytes themselves) followed by that many
//! bytes minus four. `0000` is a flush packet and `0001` a delimiter packet, neither carrying
//! a payload.

/// Frame `payload` as a single pkt-line.
pub fn encode(payload: &[u8]) -> Vec<u8> {
    let len = payload.len() + 4;
    debug_assert!(len <= 65520, "pkt-line payload too large: {len}");
    let mut out = format!("{:04x}", len).into_bytes();
    out.extend_from_slice(payload);
    out
}

/// The flush packet that terminates a section.
pub const FLUSH: &[u8] = b"0000";

/// What a client asked for in a `POST /git-upload-pack` body.
#[derive(Debug, Default, Clone)]
pub struct UploadPackRequest {
    /// Object IDs the client wants, lowercased hex.
    pub wants: Vec<String>,
    /// Object IDs the client already has (empty for a clone).
    pub haves: Vec<String>,
    /// Capabilities the client selected, taken from the first `want` line.
    pub capabilities: Vec<String>,
    /// Whether the client sent `done`.
    pub done: bool,
}

// No side-band helpers here: `BASE_CAPABILITIES` deliberately does not advertise
// `side-band`/`side-band-64k`, so nothing can select it and nothing multiplexes the
// response. `wants_side_band()` and `side_band_chunk_size()` used to sit here with no
// caller at all, which read as support the server does not have.

/// Parse a `git-upload-pack` request body.
///
/// Unknown or malformed lines are skipped rather than rejected: the body comes off the
/// network, and a honeypot that dies on a malformed want line is worse than one that answers
/// what it understood. A truncated length header ends parsing.
pub fn parse_upload_pack_request(body: &[u8]) -> UploadPackRequest {
    let mut request = UploadPackRequest::default();
    let mut offset = 0usize;

    while offset + 4 <= body.len() {
        let header = match std::str::from_utf8(&body[offset..offset + 4]) {
            Ok(h) => h,
            Err(_) => break,
        };
        let len = match usize::from_str_radix(header, 16) {
            Ok(l) => l,
            Err(_) => break,
        };
        offset += 4;

        // 0000 (flush) and 0001 (delimiter) carry no payload.
        if len == 0 || len == 1 {
            continue;
        }
        if len < 4 {
            break;
        }
        let payload_len = len - 4;
        if offset + payload_len > body.len() {
            break;
        }
        let payload = &body[offset..offset + payload_len];
        offset += payload_len;

        let line = String::from_utf8_lossy(payload);
        let line = line.trim_end_matches(['\n', '\r']);

        if let Some(rest) = line.strip_prefix("want ") {
            let mut parts = rest.split(' ');
            if let Some(oid) = parts.next() {
                if is_object_id(oid) {
                    request.wants.push(oid.to_ascii_lowercase());
                }
            }
            // Capabilities ride on the first want line only.
            if request.capabilities.is_empty() {
                request.capabilities = parts.map(|c| c.to_string()).collect();
            }
        } else if let Some(oid) = line.strip_prefix("have ") {
            let oid = oid.split(' ').next().unwrap_or("");
            if is_object_id(oid) {
                request.haves.push(oid.to_ascii_lowercase());
            }
        } else if line == "done" {
            request.done = true;
        }
    }

    request
}

fn is_object_id(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit())
}
