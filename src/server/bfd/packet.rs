//! The BFD Control packet (RFC 5880 §4) and its authentication section (§4.2–4.4, §6.7).
//! Shared by the server and the client (`src/client/bfd/`). Everything here is pure: the
//! session state machine is in `session.rs`.
use anyhow::{bail, ensure, Context, Result};
use md5::{Digest as _, Md5};
use sha1::Sha1;

/// Single-hop BFD (RFC 5881) and multihop BFD (RFC 5883) destination ports.
pub const SINGLE_HOP_PORT: u16 = 3784;
pub const MULTIHOP_PORT: u16 = 4784;
/// RFC 5881 §4: the source port MUST be in this range.
pub const SOURCE_PORTS: std::ops::RangeInclusive<u16> = 49152..=65535;
/// A Control packet with no authentication section.
pub const MANDATORY_LEN: usize = 24;
/// The largest Control packet: the mandatory section plus a keyed SHA1 section (28 bytes).
/// Anything longer is refused before it is parsed.
pub const MAX_PACKET: usize = MANDATORY_LEN + 28;
/// Simple Password: 1 to 16 bytes (§4.2).
pub const MAX_PASSWORD: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum State {
    AdminDown = 0,
    Down = 1,
    Init = 2,
    Up = 3,
}

impl State {
    pub fn from_bits(b: u8) -> Self {
        match b & 3 {
            0 => State::AdminDown,
            1 => State::Down,
            2 => State::Init,
            _ => State::Up,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            State::AdminDown => "AdminDown",
            State::Down => "Down",
            State::Init => "Init",
            State::Up => "Up",
        }
    }
}

/// The diagnostic codes of §4.1.
pub const DIAGNOSTICS: &[(u8, &str)] = &[
    (0, "no_diagnostic"),
    (1, "control_detection_time_expired"),
    (2, "echo_function_failed"),
    (3, "neighbor_signaled_session_down"),
    (4, "forwarding_plane_reset"),
    (5, "path_down"),
    (6, "concatenated_path_down"),
    (7, "administratively_down"),
    (8, "reverse_concatenated_path_down"),
];

pub fn diag_name(code: u8) -> String {
    DIAGNOSTICS
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, n)| n.to_string())
        .unwrap_or_else(|| format!("reserved_{code}"))
}

pub fn diag_code(name: &str) -> Result<u8> {
    DIAGNOSTICS
        .iter()
        .find(|(_, n)| *n == name)
        .map(|(c, _)| *c)
        .with_context(|| {
            format!(
                "diag {name:?} is not one of {}",
                DIAGNOSTICS
                    .iter()
                    .map(|(_, n)| *n)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthType {
    SimplePassword = 1,
    KeyedMd5 = 2,
    MeticulousKeyedMd5 = 3,
    KeyedSha1 = 4,
    MeticulousKeyedSha1 = 5,
}

impl AuthType {
    pub fn from_code(c: u8) -> Result<Self> {
        Ok(match c {
            1 => AuthType::SimplePassword,
            2 => AuthType::KeyedMd5,
            3 => AuthType::MeticulousKeyedMd5,
            4 => AuthType::KeyedSha1,
            5 => AuthType::MeticulousKeyedSha1,
            other => bail!("authentication type {other} is reserved"),
        })
    }
    pub fn parse(name: &str) -> Result<Self> {
        Ok(match name {
            "simple" => AuthType::SimplePassword,
            "keyed_md5" => AuthType::KeyedMd5,
            "meticulous_keyed_md5" => AuthType::MeticulousKeyedMd5,
            "keyed_sha1" => AuthType::KeyedSha1,
            "meticulous_keyed_sha1" => AuthType::MeticulousKeyedSha1,
            other => bail!("auth_type {other:?} is not simple, keyed_md5, meticulous_keyed_md5, keyed_sha1 or meticulous_keyed_sha1"),
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            AuthType::SimplePassword => "simple",
            AuthType::KeyedMd5 => "keyed_md5",
            AuthType::MeticulousKeyedMd5 => "meticulous_keyed_md5",
            AuthType::KeyedSha1 => "keyed_sha1",
            AuthType::MeticulousKeyedSha1 => "meticulous_keyed_sha1",
        }
    }
    fn digest_len(self) -> usize {
        match self {
            AuthType::SimplePassword => 0,
            AuthType::KeyedMd5 | AuthType::MeticulousKeyedMd5 => 16,
            AuthType::KeyedSha1 | AuthType::MeticulousKeyedSha1 => 20,
        }
    }
    /// The sequence number must increase with every packet, not merely not decrease.
    pub fn meticulous(self) -> bool {
        matches!(
            self,
            AuthType::MeticulousKeyedMd5 | AuthType::MeticulousKeyedSha1
        )
    }
}

/// A session's authentication: one key, as BIRD and FRR configure it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthConfig {
    pub kind: AuthType,
    pub key_id: u8,
    pub password: Vec<u8>,
}

impl AuthConfig {
    pub fn new(kind: AuthType, key_id: u8, password: &str) -> Result<Self> {
        let password = password.as_bytes().to_vec();
        ensure!(!password.is_empty(), "the BFD password is empty");
        let max = match kind {
            AuthType::SimplePassword => MAX_PASSWORD,
            other => other.digest_len(),
        };
        ensure!(
            password.len() <= max,
            "a {} password is at most {max} bytes",
            kind.name()
        );
        Ok(Self {
            kind,
            key_id,
            password,
        })
    }
}

/// The authentication section of a received packet, as far as it can be read before the key
/// is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthSection {
    pub kind: AuthType,
    pub key_id: u8,
    /// Keyed types only.
    pub sequence: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlPacket {
    pub diag: u8,
    pub state: State,
    pub poll: bool,
    pub final_: bool,
    pub control_plane_independent: bool,
    pub demand: bool,
    pub detect_mult: u8,
    pub my_discriminator: u32,
    pub your_discriminator: u32,
    pub desired_min_tx_us: u32,
    pub required_min_rx_us: u32,
    pub required_min_echo_rx_us: u32,
    pub auth: Option<AuthSection>,
}

impl ControlPacket {
    /// Encode, signing with `auth` (and `sequence`, for the keyed types) when given.
    pub fn encode(&self, auth: Option<(&AuthConfig, u32)>) -> Vec<u8> {
        let mut out = Vec::with_capacity(MAX_PACKET);
        out.push((1 << 5) | (self.diag & 0x1f));
        out.push(
            ((self.state as u8) << 6)
                | (u8::from(self.poll) << 5)
                | (u8::from(self.final_) << 4)
                | (u8::from(self.control_plane_independent) << 3)
                | (u8::from(auth.is_some()) << 2)
                | (u8::from(self.demand) << 1),
        );
        out.push(self.detect_mult);
        out.push(0); // length, below
        for v in [
            self.my_discriminator,
            self.your_discriminator,
            self.desired_min_tx_us,
            self.required_min_rx_us,
            self.required_min_echo_rx_us,
        ] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        if let Some((a, sequence)) = auth {
            out.push(a.kind as u8);
            match a.kind {
                AuthType::SimplePassword => {
                    out.push(3 + a.password.len() as u8);
                    out.push(a.key_id);
                    out.extend_from_slice(&a.password);
                }
                kind => {
                    let n = kind.digest_len();
                    out.push(8 + n as u8);
                    out.push(a.key_id);
                    out.push(0);
                    out.extend_from_slice(&sequence.to_be_bytes());
                    let start = out.len();
                    out.extend_from_slice(&padded(&a.password, n));
                    out[3] = out.len() as u8;
                    let digest = digest(kind, &out);
                    out[start..].copy_from_slice(&digest);
                }
            }
        }
        out[3] = out.len() as u8;
        out
    }
}

fn padded(key: &[u8], n: usize) -> Vec<u8> {
    let mut k = key.to_vec();
    k.resize(n, 0);
    k
}

fn digest(kind: AuthType, bytes: &[u8]) -> Vec<u8> {
    match kind.digest_len() {
        16 => Md5::digest(bytes).to_vec(),
        _ => Sha1::digest(bytes).to_vec(),
    }
}

/// Decode and check everything §6.8.6 lets a receiver check without session state: version 1,
/// the length against the datagram, a non-zero Detect Mult and My Discriminator, no
/// Multipoint, and a well-formed authentication section. Authentication itself is
/// [`verify`]; discriminator lookups are the session table's.
pub fn decode(bytes: &[u8]) -> Result<ControlPacket> {
    ensure!(
        bytes.len() <= MAX_PACKET,
        "a {}-byte datagram is longer than any BFD Control packet ({MAX_PACKET})",
        bytes.len()
    );
    ensure!(
        bytes.len() >= MANDATORY_LEN,
        "a {}-byte datagram is shorter than a BFD Control packet",
        bytes.len()
    );
    let version = bytes[0] >> 5;
    ensure!(version == 1, "BFD version {version}, not 1");
    let flags = bytes[1];
    let auth_present = flags & 0x04 != 0;
    let length = bytes[3] as usize;
    ensure!(
        length
            >= if auth_present {
                MANDATORY_LEN + 2
            } else {
                MANDATORY_LEN
            },
        "Length {length} is too short"
    );
    ensure!(
        length <= bytes.len(),
        "Length {length} is longer than the {}-byte datagram",
        bytes.len()
    );
    ensure!(flags & 0x01 == 0, "the Multipoint bit is set");
    let detect_mult = bytes[2];
    ensure!(detect_mult != 0, "Detect Mult is zero");
    let word = |i: usize| u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    let my_discriminator = word(4);
    ensure!(my_discriminator != 0, "My Discriminator is zero");
    let state = State::from_bits(flags >> 6);
    let your_discriminator = word(8);
    ensure!(
        your_discriminator != 0 || matches!(state, State::Down | State::AdminDown),
        "Your Discriminator is zero in state {}",
        state.name()
    );
    let auth = if auth_present {
        let kind = AuthType::from_code(bytes[24])?;
        let auth_len = bytes[25] as usize;
        ensure!(
            MANDATORY_LEN + auth_len == length,
            "Auth Len {auth_len} does not end the packet at Length {length}"
        );
        match kind {
            AuthType::SimplePassword => {
                ensure!(
                    (4..=3 + MAX_PASSWORD).contains(&auth_len),
                    "a Simple Password section of {auth_len} bytes"
                );
                Some(AuthSection {
                    kind,
                    key_id: bytes[26],
                    sequence: None,
                })
            }
            kind => {
                ensure!(
                    auth_len == 8 + kind.digest_len(),
                    "a {} section of {auth_len} bytes",
                    kind.name()
                );
                Some(AuthSection {
                    kind,
                    key_id: bytes[26],
                    sequence: Some(word(28)),
                })
            }
        }
    } else {
        ensure!(
            length == MANDATORY_LEN,
            "Length {length} with no authentication section"
        );
        None
    };
    Ok(ControlPacket {
        diag: bytes[0] & 0x1f,
        state,
        poll: flags & 0x20 != 0,
        final_: flags & 0x10 != 0,
        control_plane_independent: flags & 0x08 != 0,
        demand: flags & 0x02 != 0,
        detect_mult,
        my_discriminator,
        your_discriminator,
        desired_min_tx_us: word(12),
        required_min_rx_us: word(16),
        required_min_echo_rx_us: word(20),
        auth,
    })
}

/// Check a received packet's authentication against the session's configuration: the same
/// type and key, the password or the digest. The sequence-number window is the session's.
pub fn verify(bytes: &[u8], packet: &ControlPacket, config: Option<&AuthConfig>) -> Result<()> {
    let (section, config) = match (&packet.auth, config) {
        (None, None) => return Ok(()),
        (Some(_), None) => bail!("the packet is authenticated and this session is not"),
        (None, Some(_)) => bail!("the packet is not authenticated and this session is"),
        (Some(s), Some(c)) => (s, c),
    };
    ensure!(
        section.kind == config.kind,
        "authentication type {} where {} is configured",
        section.kind.name(),
        config.kind.name()
    );
    ensure!(
        section.key_id == config.key_id,
        "key id {} where {} is configured",
        section.key_id,
        config.key_id
    );
    let length = bytes[3] as usize;
    match config.kind {
        AuthType::SimplePassword => {
            ensure!(bytes[27..length] == config.password[..], "wrong password");
        }
        kind => {
            let n = kind.digest_len();
            let start = length - n;
            let mut copy = bytes[..length].to_vec();
            copy[start..].copy_from_slice(&padded(&config.password, n));
            ensure!(
                digest(kind, &copy) == bytes[start..length],
                "the {} digest does not match",
                kind.name()
            );
        }
    }
    Ok(())
}
