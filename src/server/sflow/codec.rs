//! Bounded sFlow(R) v5 XDR framing. Opaque payloads never reach model data.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};
use tokio::time::Instant;

pub const MAX_MESSAGE_BYTES: usize = 8192;
pub const MAX_SAMPLES: usize = 32;
pub const MAX_RECORDS_PER_SAMPLE: usize = 64;
pub const MAX_RECORDS: usize = 256;
pub const MAX_HEADER_BYTES: usize = 256;
pub const MAX_OPAQUE_BYTES: usize = 4096;
pub const MAX_SESSIONS: usize = 128;
pub const MAX_IPV6_EXTENSIONS: usize = 8;
pub const DEFAULT_SESSION_IDLE: u64 = 1800;
pub const DEFAULT_LLM_FALLBACK: bool = false;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub class: u32,
    pub index: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Interface {
    pub format: u32,
    pub value: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Packet {
    pub source_ip: IpAddr,
    pub destination_ip: IpAddr,
    pub packet_length: u32,
    pub protocol: u8,
    pub source_port: u16,
    pub destination_port: u16,
    pub tcp_flags: u32,
    pub traffic_class: u8,
}
impl Packet {
    fn validate(&self, ipv6: bool) -> Result<()> {
        ensure!(
            self.source_ip.is_ipv6() == ipv6 && self.destination_ip.is_ipv6() == ipv6,
            "packet address families must match record"
        );
        ensure!(self.tcp_flags <= 0x1ff, "TCP flags limit9bits");
        Ok(())
    }
}
macro_rules! counters {
    ($name:ident {$($field:ident:$ty:ident),* $(,)?}) => {
        #[derive(Clone,Debug,Serialize,Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct $name { $(pub $field:$ty),* }
        impl $name {
            fn encode(&self,out:&mut Vec<u8>) { $(out.extend_from_slice(&self.$field.to_be_bytes());)* }
            fn decode(r:&mut Reader<'_>)->Result<Self>{Ok(Self{$($field:r.$ty()?),*})}
        }
    };
}
counters!(InterfaceCounters {
    if_index: u32,
    if_type: u32,
    if_speed: u64,
    if_direction: u32,
    if_status: u32,
    in_octets: u64,
    in_unicast_packets: u32,
    in_multicast_packets: u32,
    in_broadcast_packets: u32,
    in_discards: u32,
    in_errors: u32,
    in_unknown_protocols: u32,
    out_octets: u64,
    out_unicast_packets: u32,
    out_multicast_packets: u32,
    out_broadcast_packets: u32,
    out_discards: u32,
    out_errors: u32,
    promiscuous_mode: u32
});
counters!(EthernetCounters {
    alignment_errors: u32,
    fcs_errors: u32,
    single_collision_frames: u32,
    multiple_collision_frames: u32,
    sqe_test_errors: u32,
    deferred_transmissions: u32,
    late_collisions: u32,
    excessive_collisions: u32,
    internal_mac_transmit_errors: u32,
    carrier_sense_errors: u32,
    frame_too_longs: u32,
    internal_mac_receive_errors: u32,
    symbol_errors: u32
});
counters!(VlanCounters {
    vlan_id: u32,
    octets: u64,
    unicast_packets: u32,
    multicast_packets: u32,
    broadcast_packets: u32,
    discards: u32
});
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CounterRecord {
    Interface(InterfaceCounters),
    Ethernet(EthernetCounters),
    Vlan(VlanCounters),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FlowRecord {
    SynthesizedHeader {
        packet: Packet,
        frame_length: u32,
        stripped: u32,
    },
    SampledIpv4(Packet),
    SampledIpv6(Packet),
    Ethernet {
        frame_length: u32,
        source_mac: String,
        destination_mac: String,
        ether_type: u32,
    },
    ExtendedSwitch {
        source_vlan: u32,
        source_priority: u32,
        destination_vlan: u32,
        destination_priority: u32,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Sample {
    Flow {
        #[serde(default)]
        expanded: bool,
        sequence_number: u32,
        source: Source,
        sampling_rate: u32,
        sample_pool: u32,
        drops: u32,
        input: Interface,
        output: Interface,
        records: Vec<FlowRecord>,
    },
    Counters {
        #[serde(default)]
        expanded: bool,
        sequence_number: u32,
        source: Source,
        records: Vec<CounterRecord>,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Batch {
    pub agent_address: IpAddr,
    pub sub_agent_id: u32,
    #[serde(default)]
    pub uptime_ms: Option<u32>,
    pub samples: Vec<Sample>,
}
#[derive(Debug, Serialize)]
pub struct LinkLayer {
    pub source_mac: String,
    pub destination_mac: String,
    pub ether_type: u32,
    pub vlans: Vec<u16>,
}
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    SampledHeader {
        header_protocol: u32,
        frame_length: u32,
        stripped: u32,
        header_length: usize,
        packet: Option<Packet>,
        link_layer: Option<LinkLayer>,
        decode_status: &'static str,
    },
    SampledIpv4(Packet),
    SampledIpv6(Packet),
    Ethernet {
        frame_length: u32,
        source_mac: String,
        destination_mac: String,
        ether_type: u32,
    },
    ExtendedSwitch {
        source_vlan: u32,
        source_priority: u32,
        destination_vlan: u32,
        destination_priority: u32,
    },
    InterfaceCounters(InterfaceCounters),
    EthernetCounters(EthernetCounters),
    VlanCounters(VlanCounters),
    Unknown {
        enterprise: u32,
        format: u32,
        byte_count: usize,
    },
}
#[derive(Debug, Serialize)]
pub struct FlowMeta {
    pub sampling_rate: u32,
    pub sample_pool: u32,
    pub drops: u32,
    pub input: Interface,
    pub output: Interface,
}
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DecodedSample {
    Flow {
        expanded: bool,
        sequence_number: u32,
        source: Source,
        flow: FlowMeta,
        records: Vec<Record>,
    },
    Counters {
        expanded: bool,
        sequence_number: u32,
        source: Source,
        records: Vec<Record>,
    },
    Unknown {
        enterprise: u32,
        format: u32,
        byte_count: usize,
    },
}
#[derive(Debug, Serialize)]
pub struct Message {
    pub agent_address: IpAddr,
    pub sub_agent_id: u32,
    pub sequence_number: u32,
    pub uptime_ms: u32,
    pub samples: Vec<DecodedSample>,
    pub record_count: usize,
    pub sequence_tracking: SequenceTracking,
}
#[derive(Debug, Serialize)]
pub struct SequenceTracking {
    pub expected: Option<u32>,
    pub status: &'static str,
    pub missing_datagrams: Option<u32>,
    pub uptime_decreased: bool,
}
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }
    fn left(&self) -> usize {
        self.bytes.len() - self.at
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        ensure!(n <= self.left(), "truncated sFlow XDR field");
        let s = &self.bytes[self.at..self.at + n];
        self.at += n;
        Ok(s)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into()?))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into()?))
    }
    fn opaque(&mut self, limit: usize) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        ensure!(n <= limit, "sFlow opaque byte bound");
        let data = self.take(n)?;
        self.take((4 - n % 4) % 4)?;
        Ok(data)
    }
    fn done(&self) -> Result<()> {
        ensure!(self.left() == 0, "unexpected trailing sFlow fields");
        Ok(())
    }
}
fn int(out: &mut Vec<u8>, n: u32) {
    out.extend_from_slice(&n.to_be_bytes());
}
fn opaque(out: &mut Vec<u8>, tag: u32, data: &[u8]) -> Result<()> {
    ensure!(
        data.len() <= MAX_MESSAGE_BYTES,
        "sFlow structure byte limit"
    );
    int(out, tag);
    int(out, data.len() as u32);
    out.extend_from_slice(data);
    out.resize(out.len() + (4 - data.len() % 4) % 4, 0);
    ensure!(
        out.len() <= MAX_MESSAGE_BYTES,
        "sFlow message byte limit8192"
    );
    Ok(())
}
fn source(out: &mut Vec<u8>, s: &Source, expanded: bool) -> Result<()> {
    ensure!(s.class <= 2, "unsupported source class");
    if expanded {
        int(out, s.class);
        int(out, s.index);
    } else {
        ensure!(s.index < 1 << 24, "compact source index limit24bits");
        int(out, (s.class << 24) | s.index);
    }
    Ok(())
}
fn interface(out: &mut Vec<u8>, i: &Interface, expanded: bool) -> Result<()> {
    ensure!(i.format <= 2, "unsupported interface format");
    if expanded {
        int(out, i.format);
        int(out, i.value);
    } else {
        ensure!(i.value < 1 << 30, "compact interface value limit30bits");
        int(out, (i.format << 30) | i.value);
    }
    Ok(())
}
fn read_source(r: &mut Reader<'_>, expanded: bool) -> Result<Source> {
    let n = r.u32()?;
    Ok(if expanded {
        Source {
            class: n,
            index: r.u32()?,
        }
    } else {
        Source {
            class: n >> 24,
            index: n & 0xffffff,
        }
    })
}
fn read_interface(r: &mut Reader<'_>, expanded: bool) -> Result<Interface> {
    let n = r.u32()?;
    Ok(if expanded {
        Interface {
            format: n,
            value: r.u32()?,
        }
    } else {
        Interface {
            format: n >> 30,
            value: n & 0x3fffffff,
        }
    })
}
pub fn encode(batch: &Batch, sequence: u32, uptime: u32) -> Result<(Vec<u8>, usize)> {
    ensure!(
        (1..=MAX_SAMPLES).contains(&batch.samples.len()),
        "sample count must be1..32"
    );
    let mut out = Vec::new();
    int(&mut out, 5);
    match batch.agent_address {
        IpAddr::V4(ip) => {
            int(&mut out, 1);
            out.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            int(&mut out, 2);
            out.extend_from_slice(&ip.octets());
        }
    }
    int(&mut out, batch.sub_agent_id);
    int(&mut out, sequence);
    int(&mut out, uptime);
    int(&mut out, batch.samples.len() as u32);
    let mut count = 0;
    for sample in &batch.samples {
        let mut body = Vec::new();
        let (tag, n) = match sample {
            Sample::Flow {
                expanded,
                sequence_number,
                source: s,
                sampling_rate,
                sample_pool,
                drops,
                input,
                output,
                records,
            } => {
                ensure!(
                    (1..=MAX_RECORDS_PER_SAMPLE).contains(&records.len()),
                    "flow record count1..64"
                );
                int(&mut body, *sequence_number);
                source(&mut body, s, *expanded)?;
                int(&mut body, *sampling_rate);
                int(&mut body, *sample_pool);
                int(&mut body, *drops);
                interface(&mut body, input, *expanded)?;
                interface(&mut body, output, *expanded)?;
                int(&mut body, records.len() as u32);
                for record in records {
                    let (t, b) = encode_flow(record)?;
                    opaque(&mut body, t, &b)?;
                }
                (if *expanded { 3 } else { 1 }, records.len())
            }
            Sample::Counters {
                expanded,
                sequence_number,
                source: s,
                records,
            } => {
                ensure!(
                    (1..=MAX_RECORDS_PER_SAMPLE).contains(&records.len()),
                    "counter record count1..64"
                );
                int(&mut body, *sequence_number);
                source(&mut body, s, *expanded)?;
                int(&mut body, records.len() as u32);
                for record in records {
                    let mut b = Vec::new();
                    let t = match record {
                        CounterRecord::Interface(c) => {
                            ensure!(
                                c.if_direction <= 4 && c.if_status <= 3 && c.promiscuous_mode <= 2,
                                "interface counter flags"
                            );
                            c.encode(&mut b);
                            1
                        }
                        CounterRecord::Ethernet(c) => {
                            c.encode(&mut b);
                            2
                        }
                        CounterRecord::Vlan(c) => {
                            ensure!(c.vlan_id <= 4095, "VLAN identifier limit4095");
                            c.encode(&mut b);
                            5
                        }
                    };
                    opaque(&mut body, t, &b)?;
                }
                (if *expanded { 4 } else { 2 }, records.len())
            }
        };
        count += n;
        ensure!(count <= MAX_RECORDS, "total record count limit256");
        opaque(&mut out, tag, &body)?;
    }
    Ok((out, count))
}
fn mac(text: &str) -> Result<[u8; 6]> {
    ensure!(text.len() == 17, "MAC address must contain six octets");
    let mut out = [0; 6];
    let parts = text.split(':').collect::<Vec<_>>();
    ensure!(parts.len() == 6, "MAC address delimiter");
    for (i, p) in parts.iter().enumerate() {
        ensure!(p.len() == 2, "MAC octet width");
        out[i] = u8::from_str_radix(p, 16)?;
    }
    Ok(out)
}
fn mac_text(b: &[u8]) -> String {
    b.iter()
        .map(|n| format!("{n:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}
fn encode_flow(record: &FlowRecord) -> Result<(u32, Vec<u8>)> {
    let mut out = Vec::new();
    let tag = match record {
        FlowRecord::SynthesizedHeader {
            packet,
            frame_length,
            stripped,
        } => {
            let ipv6 = packet.source_ip.is_ipv6();
            packet.validate(ipv6)?;
            ensure!(
                *frame_length >= *stripped,
                "stripped exceeds original frame"
            );
            let b = synthesized_header(packet)?;
            ensure!(
                usize::try_from(frame_length - stripped)? >= b.len(),
                "captured header exceeds remaining frame"
            );
            int(&mut out, if ipv6 { 12 } else { 11 });
            int(&mut out, *frame_length);
            int(&mut out, *stripped);
            int(&mut out, b.len() as u32);
            out.extend_from_slice(&b);
            out.resize(out.len() + (4 - b.len() % 4) % 4, 0);
            1
        }
        FlowRecord::SampledIpv4(p) | FlowRecord::SampledIpv6(p) => {
            let ipv6 = matches!(record, FlowRecord::SampledIpv6(_));
            p.validate(ipv6)?;
            int(&mut out, p.packet_length);
            int(&mut out, u32::from(p.protocol));
            match (p.source_ip, p.destination_ip) {
                (IpAddr::V4(a), IpAddr::V4(b)) => {
                    out.extend_from_slice(&a.octets());
                    out.extend_from_slice(&b.octets());
                }
                (IpAddr::V6(a), IpAddr::V6(b)) => {
                    out.extend_from_slice(&a.octets());
                    out.extend_from_slice(&b.octets());
                }
                _ => bail!("packet families differ"),
            };
            int(&mut out, u32::from(p.source_port));
            int(&mut out, u32::from(p.destination_port));
            int(&mut out, p.tcp_flags);
            int(&mut out, u32::from(p.traffic_class));
            if ipv6 {
                4
            } else {
                3
            }
        }
        FlowRecord::Ethernet {
            frame_length,
            source_mac,
            destination_mac,
            ether_type,
        } => {
            ensure!(*ether_type <= 65535, "EtherType width");
            int(&mut out, *frame_length);
            out.extend_from_slice(&mac(source_mac)?);
            out.extend_from_slice(&[0, 0]);
            out.extend_from_slice(&mac(destination_mac)?);
            out.extend_from_slice(&[0, 0]);
            int(&mut out, *ether_type);
            2
        }
        FlowRecord::ExtendedSwitch {
            source_vlan,
            source_priority,
            destination_vlan,
            destination_priority,
        } => {
            for v in [*source_vlan, *destination_vlan] {
                ensure!(v <= 4095 || v == u32::MAX, "VLAN value");
            }
            for p in [*source_priority, *destination_priority] {
                ensure!(p <= 7 || p == u32::MAX, "VLAN priority");
            }
            for n in [
                source_vlan,
                source_priority,
                destination_vlan,
                destination_priority,
            ] {
                int(&mut out, *n);
            }
            1001
        }
    };
    Ok((tag, out))
}
fn synthesized_header(p: &Packet) -> Result<Vec<u8>> {
    ensure!(
        p.protocol == 6 || p.protocol == 17,
        "synthesized headers support TCP/UDP only"
    );
    let ipv6 = p.source_ip.is_ipv6();
    let ip_len = if ipv6 { 40 } else { 20 };
    let transport_len = if p.protocol == 6 { 20 } else { 8 };
    ensure!(
        p.packet_length >= ip_len + transport_len && p.packet_length - ip_len <= 65535,
        "IP packet length"
    );
    let mut b = vec![0; ip_len as usize];
    match (p.source_ip, p.destination_ip) {
        (IpAddr::V4(a), IpAddr::V4(c)) => {
            b[0] = 0x45;
            b[1] = p.traffic_class;
            b[2..4].copy_from_slice(&u16::try_from(p.packet_length)?.to_be_bytes());
            b[8] = 64;
            b[9] = p.protocol;
            b[12..16].copy_from_slice(&a.octets());
            b[16..20].copy_from_slice(&c.octets());
            let checksum = ipv4_checksum(&b);
            b[10..12].copy_from_slice(&checksum.to_be_bytes());
        }
        (IpAddr::V6(a), IpAddr::V6(c)) => {
            b[0] = 0x60 | (p.traffic_class >> 4);
            b[1] = p.traffic_class << 4;
            b[4..6].copy_from_slice(&((p.packet_length - 40) as u16).to_be_bytes());
            b[6] = p.protocol;
            b[7] = 64;
            b[8..24].copy_from_slice(&a.octets());
            b[24..40].copy_from_slice(&c.octets());
        }
        _ => bail!("synthesized packet family mismatch"),
    }
    let at = b.len();
    b.resize(at + transport_len as usize, 0);
    b[at..at + 2].copy_from_slice(&p.source_port.to_be_bytes());
    b[at + 2..at + 4].copy_from_slice(&p.destination_port.to_be_bytes());
    if p.protocol == 6 {
        b[at + 12] = 0x50 | ((p.tcp_flags >> 8) as u8);
        b[at + 13] = p.tcp_flags as u8;
    } else {
        b[at + 4..at + 6].copy_from_slice(&((p.packet_length - ip_len) as u16).to_be_bytes());
    }
    // These are telemetry header summaries, not captured/live data packets. Payload
    // and transport checksums are unavailable and are never claimed as validated.
    Ok(b)
}
fn ipv4_checksum(b: &[u8]) -> u16 {
    let mut sum = b
        .chunks_exact(2)
        .map(|w| u32::from(u16::from_be_bytes([w[0], w[1]])))
        .sum::<u32>();
    while sum >> 16 != 0 {
        sum = (sum & 65535) + (sum >> 16);
    }
    !(sum as u16)
}
pub fn decode(bytes: &[u8]) -> Result<Message> {
    ensure!(
        bytes.len() <= MAX_MESSAGE_BYTES,
        "sFlow message byte limit8192"
    );
    let mut r = Reader::new(bytes);
    ensure!(r.u32()? == 5, "sFlow version5 required");
    let agent_address = match r.u32()? {
        1 => IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(r.take(4)?)?)),
        2 => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(r.take(16)?)?)),
        _ => bail!("unsupported agent address type"),
    };
    let sub_agent_id = r.u32()?;
    let sequence_number = r.u32()?;
    let uptime_ms = r.u32()?;
    let count = r.u32()? as usize;
    ensure!(count <= MAX_SAMPLES, "sample count limit32");
    let mut samples = Vec::with_capacity(count);
    let mut record_count = 0;
    for _ in 0..count {
        let tag = r.u32()?;
        let data = r.opaque(MAX_MESSAGE_BYTES)?;
        let enterprise = tag >> 12;
        let format = tag & 4095;
        if enterprise != 0 || !(1..=4).contains(&format) {
            ensure!(
                data.len() <= MAX_OPAQUE_BYTES,
                "unknown sample byte limit4096"
            );
            samples.push(DecodedSample::Unknown {
                enterprise,
                format,
                byte_count: data.len(),
            });
            continue;
        }
        let flow = format == 1 || format == 3;
        let expanded = format >= 3;
        let mut s = Reader::new(data);
        let sequence_number = s.u32()?;
        let source = read_source(&mut s, expanded)?;
        let flow_meta = if flow {
            Some(FlowMeta {
                sampling_rate: s.u32()?,
                sample_pool: s.u32()?,
                drops: s.u32()?,
                input: read_interface(&mut s, expanded)?,
                output: read_interface(&mut s, expanded)?,
            })
        } else {
            None
        };
        let n = s.u32()? as usize;
        ensure!(n <= MAX_RECORDS_PER_SAMPLE, "records/sample limit64");
        record_count += n;
        ensure!(record_count <= MAX_RECORDS, "total record limit256");
        let mut records = Vec::with_capacity(n);
        for _ in 0..n {
            let tag = s.u32()?;
            let b = s.opaque(MAX_OPAQUE_BYTES)?;
            records.push(decode_record(tag, b, flow)?);
        }
        s.done()?;
        samples.push(if let Some(flow) = flow_meta {
            DecodedSample::Flow {
                expanded,
                sequence_number,
                source,
                flow,
                records,
            }
        } else {
            DecodedSample::Counters {
                expanded,
                sequence_number,
                source,
                records,
            }
        });
    }
    r.done()?;
    Ok(Message {
        agent_address,
        sub_agent_id,
        sequence_number,
        uptime_ms,
        samples,
        record_count,
        sequence_tracking: SequenceTracking {
            expected: None,
            status: "untracked",
            missing_datagrams: None,
            uptime_decreased: false,
        },
    })
}
fn decode_record(tag: u32, bytes: &[u8], flow: bool) -> Result<Record> {
    let enterprise = tag >> 12;
    let format = tag & 4095;
    let mut r = Reader::new(bytes);
    if enterprise != 0
        || !if flow {
            [1, 2, 3, 4, 1001].contains(&format)
        } else {
            [1, 2, 5].contains(&format)
        }
    {
        return Ok(Record::Unknown {
            enterprise,
            format,
            byte_count: bytes.len(),
        });
    }
    let record = if flow {
        match format {
            1 => {
                let header_protocol = r.u32()?;
                let frame_length = r.u32()?;
                let stripped = r.u32()?;
                let b = r.opaque(MAX_HEADER_BYTES)?;
                ensure!(
                    stripped <= frame_length
                        && b.len() <= usize::try_from(frame_length - stripped)?,
                    "sampled header/frame lengths"
                );
                let (packet, link_layer, decode_status) = header_summary(header_protocol, b);
                Record::SampledHeader {
                    header_protocol,
                    frame_length,
                    stripped,
                    header_length: b.len(),
                    packet,
                    link_layer,
                    decode_status,
                }
            }
            2 => {
                let frame_length = r.u32()?;
                let source_mac = mac_text(&r.take(8)?[..6]);
                let destination_mac = mac_text(&r.take(8)?[..6]);
                let ether_type = r.u32()?;
                Record::Ethernet {
                    frame_length,
                    source_mac,
                    destination_mac,
                    ether_type,
                }
            }
            3 | 4 => {
                let packet_length = r.u32()?;
                let protocol = u8::try_from(r.u32()?).context("IP protocol width")?;
                let ipv6 = format == 4;
                let mut addr = || -> Result<IpAddr> {
                    Ok(if ipv6 {
                        IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(r.take(16)?)?))
                    } else {
                        IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(r.take(4)?)?))
                    })
                };
                let source_ip = addr()?;
                let destination_ip = addr()?;
                let p = Packet {
                    source_ip,
                    destination_ip,
                    packet_length,
                    protocol,
                    source_port: u16::try_from(r.u32()?)?,
                    destination_port: u16::try_from(r.u32()?)?,
                    tcp_flags: r.u32()?,
                    traffic_class: u8::try_from(r.u32()?)?,
                };
                if ipv6 {
                    Record::SampledIpv6(p)
                } else {
                    Record::SampledIpv4(p)
                }
            }
            1001 => Record::ExtendedSwitch {
                source_vlan: r.u32()?,
                source_priority: r.u32()?,
                destination_vlan: r.u32()?,
                destination_priority: r.u32()?,
            },
            _ => unreachable!(),
        }
    } else {
        match format {
            1 => Record::InterfaceCounters(InterfaceCounters::decode(&mut r)?),
            2 => Record::EthernetCounters(EthernetCounters::decode(&mut r)?),
            5 => Record::VlanCounters(VlanCounters::decode(&mut r)?),
            _ => unreachable!(),
        }
    };
    r.done()?;
    Ok(record)
}
fn header_summary(
    protocol: u32,
    bytes: &[u8],
) -> (Option<Packet>, Option<LinkLayer>, &'static str) {
    let mut at = 0;
    let mut link = None;
    let ip_protocol = if protocol == 1 {
        if bytes.len() < 14 {
            return (None, None, "truncated");
        }
        let mut ether_type = u16::from_be_bytes([bytes[12], bytes[13]]);
        at = 14;
        let mut vlans = vec![];
        while ether_type == 0x8100 || ether_type == 0x88a8 {
            if vlans.len() == 2 {
                return (None, None, "vlan_depth_limit");
            }
            if bytes.len() < at + 4 {
                return (None, None, "truncated");
            }
            vlans.push(u16::from_be_bytes([bytes[at], bytes[at + 1]]) & 4095);
            ether_type = u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]);
            at += 4;
        }
        link = Some(LinkLayer {
            source_mac: mac_text(&bytes[6..12]),
            destination_mac: mac_text(&bytes[..6]),
            ether_type: u32::from(ether_type),
            vlans,
        });
        match ether_type {
            0x800 => 11,
            0x86dd => 12,
            _ => return (None, link, "unsupported_network_protocol"),
        }
    } else {
        protocol
    };
    let b = &bytes[at..];
    let (mut p, mut offset, non_initial) = match ip_protocol {
        11 => {
            if b.len() < 20 {
                return (None, link, "truncated");
            }
            let n = usize::from(b[0] & 15) * 4;
            if b[0] >> 4 != 4 || n < 20 {
                return (None, link, "invalid_ip_header");
            }
            if b.len() < n {
                return (None, link, "truncated");
            }
            (
                Packet {
                    source_ip: IpAddr::V4(Ipv4Addr::new(b[12], b[13], b[14], b[15])),
                    destination_ip: IpAddr::V4(Ipv4Addr::new(b[16], b[17], b[18], b[19])),
                    packet_length: u32::from(u16::from_be_bytes([b[2], b[3]])),
                    protocol: b[9],
                    source_port: 0,
                    destination_port: 0,
                    tcp_flags: 0,
                    traffic_class: b[1],
                },
                n,
                u16::from_be_bytes([b[6], b[7]]) & 0x1fff != 0,
            )
        }
        12 => {
            if b.len() < 40 {
                return (None, link, "truncated");
            }
            if b[0] >> 4 != 6 {
                return (None, link, "invalid_ip_header");
            }
            (
                Packet {
                    source_ip: IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(&b[8..24]).unwrap())),
                    destination_ip: IpAddr::V6(Ipv6Addr::from(
                        <[u8; 16]>::try_from(&b[24..40]).unwrap(),
                    )),
                    packet_length: 40 + u32::from(u16::from_be_bytes([b[4], b[5]])),
                    protocol: b[6],
                    source_port: 0,
                    destination_port: 0,
                    tcp_flags: 0,
                    traffic_class: ((b[0] & 15) << 4) | (b[1] >> 4),
                },
                40,
                false,
            )
        }
        _ => return (None, link, "unsupported_header_protocol"),
    };
    if non_initial {
        return (Some(p), link, "non_initial_fragment");
    }
    let mut extensions = 0;
    if ip_protocol == 12 {
        while [0, 43, 44, 51, 60].contains(&p.protocol) {
            if extensions == MAX_IPV6_EXTENSIONS {
                return (Some(p), link, "extension_depth_limit");
            }
            extensions += 1;
            if b.len() < offset + 2 {
                return (Some(p), link, "truncated");
            }
            let n = match p.protocol {
                44 => 8,
                51 => (usize::from(b[offset + 1]) + 2) * 4,
                _ => (usize::from(b[offset + 1]) + 1) * 8,
            };
            if b.len() < offset + n {
                return (Some(p), link, "truncated");
            }
            let fragmented = p.protocol == 44
                && u16::from_be_bytes([b[offset + 2], b[offset + 3]]) & 0xfff8 != 0;
            p.protocol = b[offset];
            offset += n;
            if fragmented {
                return (Some(p), link, "non_initial_fragment");
            }
        }
    }
    if p.protocol == 6 || p.protocol == 17 {
        if b.len() < offset + 4 {
            return (Some(p), link, "truncated");
        }
        p.source_port = u16::from_be_bytes([b[offset], b[offset + 1]]);
        p.destination_port = u16::from_be_bytes([b[offset + 2], b[offset + 3]]);
        if p.protocol == 6 {
            if b.len() < offset + 14 {
                return (Some(p), link, "truncated");
            }
            p.tcp_flags = (u32::from(b[offset + 12] & 1) << 8) | u32::from(b[offset + 13]);
        }
    }
    (Some(p), link, "decoded")
}
struct Session {
    expected: u32,
    uptime: u32,
    last_seen: Instant,
}
pub struct SequenceCache {
    sessions: BTreeMap<(SocketAddr, IpAddr, u32), Session>,
    idle: Duration,
}
impl SequenceCache {
    pub fn new(idle: Duration) -> Self {
        Self {
            sessions: BTreeMap::new(),
            idle,
        }
    }
    pub fn count(&self) -> usize {
        self.sessions.len()
    }
    pub fn expire(&mut self, now: Instant) {
        self.sessions
            .retain(|_, s| now.duration_since(s.last_seen) < self.idle);
    }
    pub fn ingest(&mut self, peer: SocketAddr, bytes: &[u8], now: Instant) -> Result<Message> {
        self.expire(now);
        let mut m = decode(bytes)?;
        let key = (peer, m.agent_address, m.sub_agent_id);
        ensure!(
            self.sessions.contains_key(&key) || self.sessions.len() < MAX_SESSIONS,
            "sFlow session cap128"
        );
        let mut advance = true;
        if let Some(s) = self.sessions.get(&key) {
            let delta = m.sequence_number.wrapping_sub(s.expected);
            m.sequence_tracking = SequenceTracking {
                expected: Some(s.expected),
                status: if delta == 0 {
                    "in_order"
                } else if delta < 0x80000000 {
                    "gap"
                } else {
                    advance = false;
                    "out_of_order_or_duplicate"
                },
                missing_datagrams: if delta > 0 && delta < 0x80000000 {
                    Some(delta)
                } else {
                    None
                },
                // A lower uptime can mean a reboot or a late datagram. It does not
                // justify regressing sequence state; the idle timeout resets sessions.
                uptime_decreased: m.uptime_ms.wrapping_sub(s.uptime) >= 0x80000000,
            };
        }
        if advance {
            self.sessions.insert(
                key,
                Session {
                    expected: m.sequence_number.wrapping_add(1),
                    uptime: m.uptime_ms,
                    last_seen: now,
                },
            );
        } else if let Some(s) = self.sessions.get_mut(&key) {
            s.last_seen = now;
        }
        Ok(m)
    }
}
