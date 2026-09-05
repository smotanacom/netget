//! Pure VRRP (v2/v3) and CARP packet codec. **No I/O, no async, no `AppState`.**
//!
//! This file is the part of the protocol that can actually be proved in this environment.
//! Nothing here opens a socket, so every function is exercised by
//! `tests/server/vrrp/codec_test.rs` against literal specification bytes in both directions.
//! The transport in [`super`] is a thin layer over it.
//!
//! # Three packet formats, one IP protocol number
//!
//! IP protocol **112** carries three different things:
//!
//! * **VRRPv2** — RFC 3768 §5.1. Advertisement interval in **whole seconds**, one octet. An
//!   authentication-type octet (kept, but RFC 3768 removed every method, so it is 0) and
//!   eight octets of zeroed authentication data at the end.
//! * **VRRPv3** — RFC 5798 §5.2. The authentication-type octet becomes four reserved bits,
//!   and the interval becomes a **12-bit centisecond** field. The trailing authentication
//!   data is gone. The checksum additionally covers an IP pseudo-header.
//! * **CARP** — OpenBSD's replacement, which squats the same protocol number with a
//!   completely different 36-octet layout. It is **not** VRRP.
//!
//! ## The first octet does not tell them apart
//!
//! VRRPv2's version/type octet is `0x21` (version 2, type 1 = advertisement). CARP's is
//! *also* `0x21` (`CARP_VERSION` 2, `CARP_ADVERTISEMENT` 1). A dispatcher that switches on
//! the first byte will decode a CARP advertisement as a VRRPv2 one carrying seven virtual
//! addresses, because CARP's authentication-length octet sits exactly where VRRP's
//! count-IP-addresses octet does. That is why the server takes a `variant` startup parameter
//! rather than sniffing: only the operator knows which protocol the segment is running.
//! [`Advertisement::decode`] takes the variant explicitly for the same reason.
//!
//! ## Two things implementations get wrong, both pinned to literal bytes
//!
//! 1. **Seconds versus centiseconds.** A one-second interval is `0x01` in VRRPv2 and
//!    `0x064` (100) in VRRPv3. Writing seconds into the v3 field yields an advertisement
//!    claiming a 10-millisecond interval, and a real peer's master-down timer acts on it.
//! 2. **The v3 checksum is not the v2 checksum.** VRRPv2 (RFC 3768 §5.3.8) sums the VRRP
//!    message alone. VRRPv3 (RFC 5798 §5.2.8) sums an IP pseudo-header *and* the message,
//!    with next-header 112. The same bytes therefore carry different checksums under the two
//!    versions, and [`VrrpAdvertisement::encode`] refuses to guess: a v3 encode without a
//!    [`PseudoHeader`] is an error, never a silently-wrong packet.

use anyhow::{bail, ensure, Context, Result};
use sha1::{Digest, Sha1};
use std::net::Ipv4Addr;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// IANA protocol number shared by VRRP and CARP.
pub const IP_PROTOCOL_VRRP: u8 = 112;

/// The IPv4 VRRP multicast group (RFC 5798 §5.1.1.2).
pub const VRRP_MULTICAST_IPV4: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 18);

/// The IPv6 VRRP multicast group, `FF02::12` (RFC 5798 §5.1.2.2).
///
/// Declared for completeness and to name the thing that is missing: **no IPv6 transport is
/// implemented**, and [`VrrpAdvertisement`] carries IPv4 virtual addresses only.
pub const VRRP_MULTICAST_IPV6: std::net::Ipv6Addr =
    std::net::Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0x12);

/// The only VRRP packet type there is (RFC 3768 §5.3.2, RFC 5798 §5.2.2).
pub const VRRP_TYPE_ADVERTISEMENT: u8 = 1;

pub const VRRP_VERSION_2: u8 = 2;
pub const VRRP_VERSION_3: u8 = 3;

/// Priority 255 is reserved for the router that **owns** the virtual addresses
/// (RFC 3768 §5.3.4). Claiming it asserts the addresses are configured on this interface.
pub const VRRP_PRIORITY_ADDRESS_OWNER: u8 = 255;

/// Priority 0 means the current master is **resigning**, and it exists so a backup can take
/// over immediately instead of waiting out its master-down interval (RFC 3768 §6.4.3).
pub const VRRP_PRIORITY_RESIGN: u8 = 0;

/// RFC 3768 §5.3.5: no authentication method is defined; the field is sent as zero.
pub const VRRP_AUTH_TYPE_NONE: u8 = 0;

/// VRRPv2 fixed header, before the virtual addresses.
pub const VRRP_V2_HEADER_LEN: usize = 8;
/// VRRPv2's two zeroed authentication-data words, after the virtual addresses.
pub const VRRP_V2_AUTH_DATA_LEN: usize = 8;
/// VRRPv3 fixed header, before the virtual addresses. No trailing authentication data.
pub const VRRP_V3_HEADER_LEN: usize = 8;

/// Largest value the VRRPv3 12-bit Max Advertisement Interval field can hold, in
/// centiseconds — 40.95 seconds.
pub const VRRP_V3_MAX_INTERVAL_CENTISECONDS: u16 = 0x0fff;

/// `CARP_VERSION` in OpenBSD's `ip_carp.h`.
pub const CARP_VERSION: u8 = 2;
/// `CARP_ADVERTISEMENT` in OpenBSD's `ip_carp.h`.
pub const CARP_TYPE_ADVERTISEMENT: u8 = 1;
/// A CARP advertisement is always exactly this long: 8 octets of header, an 8-octet counter
/// and a 20-octet SHA-1 HMAC.
pub const CARP_HEADER_LEN: usize = 36;
/// `carp_authlen` counts 32-bit words of counter + HMAC: (8 + 20) / 4 = 7.
pub const CARP_AUTH_LEN_WORDS: u8 = 7;
/// OpenBSD's `CARP_KEY_LEN` — the passphrase is zero-padded (or truncated) to this length.
pub const CARP_KEY_LEN: usize = 20;

// ---------------------------------------------------------------------------
// Checksum
// ---------------------------------------------------------------------------

/// The standard 16-bit one's-complement checksum of RFC 1071.
///
/// Big-endian 16-bit words, an odd trailing octet padded with zero, carries folded in, result
/// complemented. The defining property a receiver relies on is that running this over a buffer
/// that already contains its own correct checksum yields 0 — which is exactly what
/// [`checksum_is_valid`] tests.
pub fn internet_checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut chunks = data.chunks_exact(2);
    for chunk in &mut chunks {
        sum += u16::from_be_bytes([chunk[0], chunk[1]]) as u32;
    }
    if let Some(&last) = chunks.remainder().first() {
        sum += u16::from_be_bytes([last, 0]) as u32;
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// The IPv4 pseudo-header VRRPv3 folds into its checksum (RFC 5798 §5.2.8).
///
/// Twelve octets: source address, destination address, a zero octet, the protocol number
/// (112) and the VRRP message length. This is the same shape TCP and UDP use, and it is the
/// reason a VRRPv3 advertisement's checksum depends on *where it is going* — the identical
/// message body sent to `224.0.0.18` and to a unicast peer carries different checksums.
///
/// VRRPv2 has no pseudo-header at all (RFC 3768 §5.3.8 sums the message alone).
///
/// IPv6 is not implemented: RFC 5798 points at the 40-octet RFC 2460 §8.1 pseudo-header for
/// that case, and this server has no IPv6 transport to exercise it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PseudoHeader {
    pub source: Ipv4Addr,
    pub destination: Ipv4Addr,
}

impl PseudoHeader {
    pub fn new(source: Ipv4Addr, destination: Ipv4Addr) -> Self {
        Self {
            source,
            destination,
        }
    }

    /// The twelve octets, for a VRRP message of `message_len` octets.
    pub fn bytes(&self, message_len: usize) -> Result<[u8; 12]> {
        let length = u16::try_from(message_len)
            .with_context(|| format!("VRRP message of {message_len} octets exceeds 65535"))?;
        let mut out = [0u8; 12];
        out[0..4].copy_from_slice(&self.source.octets());
        out[4..8].copy_from_slice(&self.destination.octets());
        out[8] = 0;
        out[9] = IP_PROTOCOL_VRRP;
        out[10..12].copy_from_slice(&length.to_be_bytes());
        Ok(out)
    }
}

/// Which bytes a checksum covers. Reported on every event so an operator can see what was
/// actually verified rather than assuming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksumScope {
    /// VRRPv2 and CARP: the message alone.
    Message,
    /// VRRPv3: an IPv4 pseudo-header followed by the message.
    PseudoHeaderAndMessage,
}

impl ChecksumScope {
    pub fn as_str(self) -> &'static str {
        match self {
            ChecksumScope::Message => "message",
            ChecksumScope::PseudoHeaderAndMessage => "pseudo_header_and_message",
        }
    }
}

/// True when `message` carries a correct checksum in place.
///
/// `pseudo` must be `Some` for VRRPv3 and `None` for VRRPv2/CARP; supplying the wrong one
/// produces a false negative, which is why the caller derives it from the decoded version
/// rather than guessing.
pub fn checksum_is_valid(message: &[u8], pseudo: Option<&PseudoHeader>) -> bool {
    match pseudo {
        None => internet_checksum(message) == 0,
        Some(pseudo) => {
            let Ok(prefix) = pseudo.bytes(message.len()) else {
                return false;
            };
            let mut buffer = Vec::with_capacity(prefix.len() + message.len());
            buffer.extend_from_slice(&prefix);
            buffer.extend_from_slice(message);
            internet_checksum(&buffer) == 0
        }
    }
}

// ---------------------------------------------------------------------------
// VRRP
// ---------------------------------------------------------------------------

/// One VRRP advertisement, version 2 or version 3.
///
/// The interval is held in **seconds** because that is what a person and a model reason in;
/// [`Self::interval_field`] does the version-dependent scaling in exactly one place, and the
/// codec test pins its output at the wire offsets for both versions.
#[derive(Debug, Clone, PartialEq)]
pub struct VrrpAdvertisement {
    /// 2 or 3.
    pub version: u8,
    /// Virtual Router Identifier, 1..=255. 0 is not a valid VRID.
    pub vrid: u8,
    /// 0 resigns, 255 claims address ownership, higher wins the election.
    pub priority: u8,
    /// Advertisement interval in seconds. VRRPv2 carries whole seconds in one octet;
    /// VRRPv3 carries centiseconds in a 12-bit field.
    pub advert_interval_seconds: f64,
    /// VRRPv2 only. RFC 3768 defines no method, so this is 0 in anything conformant; it is
    /// surfaced rather than discarded because a non-zero value on the wire says the sender is
    /// running RFC 2338-era authentication.
    pub auth_type: u8,
    /// The virtual IPv4 addresses this router is advertising. May be empty on the wire, but a
    /// real advertisement always carries at least one.
    pub addresses: Vec<Ipv4Addr>,
    /// The checksum as it appeared on the wire. Ignored by [`Self::encode`], which always
    /// recomputes.
    pub checksum: u16,
}

impl VrrpAdvertisement {
    /// True when this advertisement is the master resigning (RFC 3768 §6.4.3).
    pub fn is_resignation(&self) -> bool {
        self.priority == VRRP_PRIORITY_RESIGN
    }

    /// True when the sender claims to own the virtual addresses (RFC 3768 §5.3.4).
    pub fn is_address_owner(&self) -> bool {
        self.priority == VRRP_PRIORITY_ADDRESS_OWNER
    }

    /// Which bytes this version's checksum covers.
    pub fn checksum_scope(&self) -> ChecksumScope {
        if self.version >= VRRP_VERSION_3 {
            ChecksumScope::PseudoHeaderAndMessage
        } else {
            ChecksumScope::Message
        }
    }

    /// The raw interval field for this version.
    ///
    /// **This is the seconds-versus-centiseconds trap.** VRRPv2 puts whole seconds in one
    /// octet (RFC 3768 §5.3.7); VRRPv3 puts centiseconds in twelve bits (RFC 5798 §5.2.7).
    /// One second is `1` under v2 and `100` under v3.
    pub fn interval_field(&self) -> Result<u16> {
        ensure!(
            self.advert_interval_seconds.is_finite() && self.advert_interval_seconds > 0.0,
            "advert_interval must be a positive number of seconds, got {}",
            self.advert_interval_seconds
        );
        match self.version {
            VRRP_VERSION_2 => {
                let seconds = self.advert_interval_seconds;
                ensure!(
                    (seconds - seconds.round()).abs() < 1e-9,
                    "VRRPv2 carries the advertisement interval in WHOLE SECONDS in a single \
                     octet, so {seconds}s cannot be expressed. Use version 3, whose field is \
                     in centiseconds, for sub-second intervals."
                );
                let seconds = seconds.round();
                ensure!(
                    (1.0..=255.0).contains(&seconds),
                    "VRRPv2 advertisement interval must be 1..=255 seconds, got {seconds}"
                );
                Ok(seconds as u16)
            }
            VRRP_VERSION_3 => {
                let centiseconds = (self.advert_interval_seconds * 100.0).round();
                ensure!(
                    (1.0..=(VRRP_V3_MAX_INTERVAL_CENTISECONDS as f64)).contains(&centiseconds),
                    "VRRPv3 carries the advertisement interval in CENTISECONDS in a 12-bit \
                     field, so it must be 0.01..=40.95 seconds; {}s is {centiseconds} \
                     centiseconds",
                    self.advert_interval_seconds
                );
                Ok(centiseconds as u16)
            }
            other => bail!("unsupported VRRP version {other} (expected 2 or 3)"),
        }
    }

    /// Validate every field without needing a pseudo-header.
    ///
    /// Separate from [`Self::encode`] so `execute_action` can reject a bad action where the
    /// error reaches the model, rather than in the transport where it reaches only a log.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == VRRP_VERSION_2 || self.version == VRRP_VERSION_3,
            "unsupported VRRP version {} (expected 2 or 3)",
            self.version
        );
        ensure!(self.vrid != 0, "VRID 0 is not a valid virtual router id");
        ensure!(
            self.addresses.len() <= 255,
            "a VRRP advertisement can carry at most 255 addresses, got {}",
            self.addresses.len()
        );
        if self.version == VRRP_VERSION_3 {
            ensure!(
                self.auth_type == VRRP_AUTH_TYPE_NONE,
                "VRRPv3 has no authentication-type field — those four bits are reserved and \
                 must be zero (RFC 5798 §5.2.5); got auth_type {}",
                self.auth_type
            );
        }
        self.interval_field()?;
        Ok(())
    }

    /// Serialise, computing the checksum.
    ///
    /// `pseudo` **must** be `Some` for version 3 and is ignored for version 2. A v3 encode
    /// with `None` is an error rather than a message-only checksum, because a silently-wrong
    /// checksum is discarded by every real peer and looks exactly like the server being down.
    pub fn encode(&self, pseudo: Option<&PseudoHeader>) -> Result<Vec<u8>> {
        self.validate()?;
        let interval = self.interval_field()?;

        let mut message = Vec::new();
        message.push((self.version << 4) | VRRP_TYPE_ADVERTISEMENT);
        message.push(self.vrid);
        message.push(self.priority);
        message.push(self.addresses.len() as u8);

        match self.version {
            VRRP_VERSION_2 => {
                message.push(self.auth_type);
                message.push(interval as u8);
            }
            _ => {
                // Four reserved bits, then the top four bits of the 12-bit interval.
                message.push(((interval >> 8) & 0x0f) as u8);
                message.push((interval & 0xff) as u8);
            }
        }

        message.extend_from_slice(&[0, 0]); // checksum placeholder
        for address in &self.addresses {
            message.extend_from_slice(&address.octets());
        }
        if self.version == VRRP_VERSION_2 {
            // RFC 3768 §5.3.10 keeps the two authentication-data words and requires zeros.
            message.extend_from_slice(&[0u8; VRRP_V2_AUTH_DATA_LEN]);
        }

        let checksum = match self.version {
            VRRP_VERSION_2 => internet_checksum(&message),
            _ => {
                let pseudo = pseudo.context(
                    "VRRPv3 checksums cover an IP pseudo-header (RFC 5798 §5.2.8), so the \
                     source and destination addresses are required to encode one. Encoding \
                     without them would produce a packet every conformant peer discards.",
                )?;
                let prefix = pseudo.bytes(message.len())?;
                let mut buffer = Vec::with_capacity(prefix.len() + message.len());
                buffer.extend_from_slice(&prefix);
                buffer.extend_from_slice(&message);
                internet_checksum(&buffer)
            }
        };
        message[6..8].copy_from_slice(&checksum.to_be_bytes());
        Ok(message)
    }

    /// Parse an advertisement. The caller has already decided this is VRRP and not CARP.
    pub fn decode(data: &[u8]) -> Result<Self> {
        ensure!(
            data.len() >= VRRP_V2_HEADER_LEN,
            "VRRP message truncated: {} octets, need at least {}",
            data.len(),
            VRRP_V2_HEADER_LEN
        );

        let version = data[0] >> 4;
        let packet_type = data[0] & 0x0f;
        ensure!(
            packet_type == VRRP_TYPE_ADVERTISEMENT,
            "VRRP packet type {packet_type} is not an advertisement (the only type defined)"
        );
        ensure!(
            version == VRRP_VERSION_2 || version == VRRP_VERSION_3,
            "unsupported VRRP version {version} (expected 2 or 3). Note that a CARP \
             advertisement also starts 0x21 and must be decoded as CARP, not as VRRPv2."
        );

        let vrid = data[1];
        let priority = data[2];
        let count = data[3] as usize;
        let checksum = u16::from_be_bytes([data[6], data[7]]);

        let (auth_type, advert_interval_seconds) = if version == VRRP_VERSION_2 {
            (data[4], data[5] as f64)
        } else {
            let centiseconds = (((data[4] & 0x0f) as u16) << 8) | data[5] as u16;
            (VRRP_AUTH_TYPE_NONE, centiseconds as f64 / 100.0)
        };

        let needed = VRRP_V2_HEADER_LEN + count * 4;
        ensure!(
            data.len() >= needed,
            "VRRP message declares {count} address(es) but is only {} octets; {needed} needed",
            data.len()
        );

        let mut addresses = Vec::with_capacity(count);
        for i in 0..count {
            let offset = VRRP_V2_HEADER_LEN + i * 4;
            addresses.push(Ipv4Addr::new(
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ));
        }

        Ok(Self {
            version,
            vrid,
            priority,
            advert_interval_seconds,
            auth_type,
            addresses,
            checksum,
        })
    }
}

// ---------------------------------------------------------------------------
// CARP
// ---------------------------------------------------------------------------

/// One CARP advertisement (OpenBSD `struct carp_header`).
///
/// **CARP is not VRRP.** It shares only the IP protocol number. There is no CARP RFC; the
/// layout below is taken from OpenBSD's `sys/netinet/ip_carp.h`:
///
/// ```text
///  0      version(4) | type(4)   = 0x21 — the same first octet as a VRRPv2 advertisement
///  1      carp_vhid                virtual host id
///  2      carp_advskew             advertisement skew; LOWER WINS, the inverse of VRRP
///  3      carp_authlen             32-bit words of counter + HMAC, always 7
///  4      carp_demote              demotion counter
///  5      carp_advbase             advertisement base interval, whole seconds
///  6.. 8  carp_cksum               one's-complement checksum over these 36 octets
///  8..16  carp_counter             64-bit replay counter, big-endian on the wire
/// 16..36  carp_md                  HMAC-SHA1
/// ```
///
/// The election runs on `advbase` + `advskew`/256 seconds: the host that advertises soonest
/// wins, so a **lower** skew is stronger. That is the opposite direction from VRRP's
/// priority, and it is the single easiest thing to get backwards when treating the two as one
/// protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CarpAdvertisement {
    pub version: u8,
    pub vhid: u8,
    pub advskew: u8,
    pub auth_length_words: u8,
    pub demote: u8,
    pub advbase: u8,
    pub counter: u64,
    pub hmac: [u8; 20],
    /// The checksum as it appeared on the wire; ignored by [`Self::encode`].
    pub checksum: u16,
}

impl CarpAdvertisement {
    /// The advertisement interval this host is claiming, in seconds.
    pub fn interval_seconds(&self) -> f64 {
        self.advbase as f64 + (self.advskew as f64 / 256.0)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == CARP_VERSION,
            "unsupported CARP version {} (OpenBSD's CARP_VERSION is {CARP_VERSION})",
            self.version
        );
        ensure!(self.vhid != 0, "CARP vhid 0 is not a valid virtual host id");
        ensure!(
            self.advbase >= 1,
            "CARP advbase must be at least 1 second; 0 would advertise continuously"
        );
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut message = Vec::with_capacity(CARP_HEADER_LEN);
        message.push((self.version << 4) | CARP_TYPE_ADVERTISEMENT);
        message.push(self.vhid);
        message.push(self.advskew);
        message.push(self.auth_length_words);
        message.push(self.demote);
        message.push(self.advbase);
        message.extend_from_slice(&[0, 0]); // checksum placeholder
        message.extend_from_slice(&self.counter.to_be_bytes());
        message.extend_from_slice(&self.hmac);
        debug_assert_eq!(message.len(), CARP_HEADER_LEN);

        let checksum = internet_checksum(&message);
        message[6..8].copy_from_slice(&checksum.to_be_bytes());
        Ok(message)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        ensure!(
            data.len() >= CARP_HEADER_LEN,
            "CARP advertisement is {} octets, must be {CARP_HEADER_LEN}",
            data.len()
        );
        let version = data[0] >> 4;
        let packet_type = data[0] & 0x0f;
        ensure!(
            packet_type == CARP_TYPE_ADVERTISEMENT,
            "CARP packet type {packet_type} is not an advertisement"
        );
        let mut hmac = [0u8; 20];
        hmac.copy_from_slice(&data[16..36]);
        Ok(Self {
            version,
            vhid: data[1],
            advskew: data[2],
            auth_length_words: data[3],
            demote: data[4],
            advbase: data[5],
            checksum: u16::from_be_bytes([data[6], data[7]]),
            counter: u64::from_be_bytes([
                data[8], data[9], data[10], data[11], data[12], data[13], data[14], data[15],
            ]),
            hmac,
        })
    }
}

/// Build the CARP HMAC exactly as OpenBSD's `carp_hmac_prepare` / `carp_hmac_generate` do.
///
/// The key is the passphrase, zero-padded or truncated to [`CARP_KEY_LEN`] — OpenBSD's
/// `ifconfig carp pass` copies the raw bytes into a 20-octet field without hashing them
/// first. The message is `version || type || vhid || each virtual address || counter`, with
/// the counter in the same big-endian form it takes on the wire, and **version and type as
/// two separate octets** (`0x02`, `0x01`) rather than the packed `0x21` of the header.
///
/// **Unverified against a live peer.** This is derived from reading `sys/netinet/ip_carp.c`;
/// no OpenBSD `carp` interface has ever accepted a packet from this code. The HMAC-SHA1
/// underneath is a different matter: the hash is the `sha1` crate's and the RFC 2104
/// construction is pinned to RFC 2202's vectors. What remains unproven is the CARP-specific
/// *input ordering* assembled here — which fields, in which order, with which key derivation
/// — not the primitive computing over it. See `src/server/vrrp/CLAUDE.md`.
pub fn carp_hmac(passphrase: &[u8], vhid: u8, addresses: &[Ipv4Addr], counter: u64) -> [u8; 20] {
    let mut key = [0u8; CARP_KEY_LEN];
    let take = passphrase.len().min(CARP_KEY_LEN);
    key[..take].copy_from_slice(&passphrase[..take]);

    let mut message = vec![CARP_VERSION, CARP_TYPE_ADVERTISEMENT, vhid];
    for address in addresses {
        message.extend_from_slice(&address.octets());
    }
    message.extend_from_slice(&counter.to_be_bytes());

    hmac_sha1(&key, &message)
}

// ---------------------------------------------------------------------------
// SHA-1 / HMAC-SHA1
// ---------------------------------------------------------------------------

/// SHA-1 (RFC 3174 / FIPS 180-4) as a fixed-size array, over the `sha1` crate.
///
/// This is a thin adapter, not an implementation: the `vrrp` feature declares `dep:sha1` and
/// the hashing is entirely the crate's. It exists only so the rest of this file can take a
/// `[u8; 20]` without threading `GenericArray` and `Digest` through every caller.
///
/// `tests/server/vrrp/codec_test.rs` runs the FIPS 180 / RFC 3174 vectors and a 0..=130 length
/// sweep through it anyway. Those do not assert that SHA-1 is correct — that is the crate's
/// problem — they assert that **this adapter drives it correctly**: hashing the wrong buffer,
/// dropping an `update` or truncating the digest would pass every other test in that file and
/// fail these.
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut out = [0u8; 20];
    out.copy_from_slice(&Sha1::digest(data));
    out
}

/// HMAC-SHA1 (RFC 2104), block size 64.
///
/// The construction is ours because no HMAC crate is reachable from this feature — `hmac` is
/// optional and gated behind `tor` — but the hashing underneath is the `sha1` crate's. That
/// makes the key padding and the inner/outer ordering the only part written here, and it is
/// exactly the part RFC 2202's vectors check: an implementation that swapped `0x36` and
/// `0x5c`, forgot to hash an over-long key, or concatenated the wrong way round fails them
/// immediately.
pub fn hmac_sha1(key: &[u8], data: &[u8]) -> [u8; 20] {
    const BLOCK: usize = 64;

    // RFC 2104 §2: a key longer than the block size is replaced by its own hash, and any key
    // is then zero-padded to the block size.
    let mut padded = [0u8; BLOCK];
    if key.len() > BLOCK {
        padded[..20].copy_from_slice(&sha1(key));
    } else {
        padded[..key.len()].copy_from_slice(key);
    }

    let ipad: Vec<u8> = padded.iter().map(|byte| byte ^ 0x36).collect();
    let opad: Vec<u8> = padded.iter().map(|byte| byte ^ 0x5c).collect();

    let mut inner = Sha1::new();
    inner.update(&ipad);
    inner.update(data);
    let inner = inner.finalize();

    let mut outer = Sha1::new();
    outer.update(&opad);
    outer.update(inner);

    let mut out = [0u8; 20];
    out.copy_from_slice(&outer.finalize());
    out
}

// ---------------------------------------------------------------------------
// Variant dispatch
// ---------------------------------------------------------------------------

/// Which of the two protocols sharing IP protocol 112 a server speaks.
///
/// This is a configuration choice, not something inferred from the wire: a CARP
/// advertisement and a VRRPv2 advertisement have the same first octet (`0x21`), and CARP's
/// authentication-length field sits where VRRP's address count does, so a sniffing decoder
/// reads a CARP packet as a VRRPv2 packet claiming seven virtual addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Vrrp,
    Carp,
}

impl Variant {
    pub fn from_name(name: &str) -> Result<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "vrrp" | "vrrpv2" | "vrrpv3" | "rfc3768" | "rfc5798" => Ok(Variant::Vrrp),
            "carp" | "openbsd" => Ok(Variant::Carp),
            other => bail!("unknown variant '{other}' (expected 'vrrp' or 'carp')"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Variant::Vrrp => "vrrp",
            Variant::Carp => "carp",
        }
    }
}

/// A decoded packet of either variant.
#[derive(Debug, Clone, PartialEq)]
pub enum Advertisement {
    Vrrp(VrrpAdvertisement),
    Carp(CarpAdvertisement),
}

impl Advertisement {
    /// Decode according to the variant the operator configured.
    pub fn decode(variant: Variant, data: &[u8]) -> Result<Self> {
        match variant {
            Variant::Vrrp => VrrpAdvertisement::decode(data).map(Advertisement::Vrrp),
            Variant::Carp => CarpAdvertisement::decode(data).map(Advertisement::Carp),
        }
    }

    pub fn variant(&self) -> Variant {
        match self {
            Advertisement::Vrrp(_) => Variant::Vrrp,
            Advertisement::Carp(_) => Variant::Carp,
        }
    }
}
