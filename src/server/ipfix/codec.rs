//! Bounded RFC7011 UDP wire state, not a flow database.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};
use tokio::time::Instant;
pub const DEFAULT_LLM_FALLBACK: bool = false;
pub const MAX_MESSAGE_BYTES: usize = 8192;
pub const MAX_SETS: usize = 64;
pub const MAX_TEMPLATES_PER_SESSION: usize = 32;
pub const MAX_SESSIONS: usize = 128;
pub const MAX_CACHED_TEMPLATES: usize = 1024;
pub const MAX_FIELDS: usize = 32;
pub const MAX_RECORDS: usize = 256;
pub const MAX_FIELD_BYTES: usize = 1024;
pub const DEFAULT_TEMPLATE_TTL: u64 = 600;
pub const DEFAULT_SESSION_IDLE: u64 = 1800;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Unsigned,
    Ipv4,
    Ipv6,
    String,
    Seconds,
    Milliseconds,
}
macro_rules! elements {($($variant:ident=>($id:literal,$kind:ident,$width:literal,$name:literal)),* $(,)?)=>{
 #[derive(Clone,Copy,Debug,Serialize,Deserialize,PartialEq,Eq)]
 #[serde(rename_all="snake_case")]
 pub enum Element {$($variant),*}
 impl Element {
  pub fn id(self)->u16{match self{$(Self::$variant=>$id),*}}
  pub fn iana_name(self)->&'static str{match self{$(Self::$variant=>$name),*}}
  fn descriptor(self)->(Kind,u16){match self{$(Self::$variant=>(Kind::$kind,$width)),*}}
  fn from_id(id:u16)->Option<Self>{match id{$($id=>Some(Self::$variant)),*,_=>None}}
 }
}}
elements! {
 OctetDeltaCount=>(1,Unsigned,8,"octetDeltaCount"),
 PacketDeltaCount=>(2,Unsigned,8,"packetDeltaCount"),
 ProtocolIdentifier=>(4,Unsigned,1,"protocolIdentifier"),
 IpClassOfService=>(5,Unsigned,1,"ipClassOfService"),
 TcpControlBits=>(6,Unsigned,2,"tcpControlBits"),
 SourceTransportPort=>(7,Unsigned,2,"sourceTransportPort"),
 SourceIpv4Address=>(8,Ipv4,4,"sourceIPv4Address"),
 IngressInterface=>(10,Unsigned,4,"ingressInterface"),
 DestinationTransportPort=>(11,Unsigned,2,"destinationTransportPort"),
 DestinationIpv4Address=>(12,Ipv4,4,"destinationIPv4Address"),
 EgressInterface=>(14,Unsigned,4,"egressInterface"),
 IpNextHopIpv4Address=>(15,Ipv4,4,"ipNextHopIPv4Address"),
 BgpSourceAsNumber=>(16,Unsigned,4,"bgpSourceAsNumber"),
 BgpDestinationAsNumber=>(17,Unsigned,4,"bgpDestinationAsNumber"),
 SourceIpv6Address=>(27,Ipv6,16,"sourceIPv6Address"),
 DestinationIpv6Address=>(28,Ipv6,16,"destinationIPv6Address"),
 SamplingInterval=>(34,Unsigned,4,"samplingInterval"),
 SamplingAlgorithm=>(35,Unsigned,1,"samplingAlgorithm"),
 IpVersion=>(60,Unsigned,1,"ipVersion"),
 InterfaceName=>(82,String,65535,"interfaceName"),
 InterfaceDescription=>(83,String,65535,"interfaceDescription"),
 ApplicationName=>(96,String,65535,"applicationName"),
 ObservationPointId=>(138,Unsigned,8,"observationPointId"),
 MeteringProcessId=>(143,Unsigned,4,"meteringProcessId"),
 ExportingProcessId=>(144,Unsigned,4,"exportingProcessId"),
 TemplateId=>(145,Unsigned,2,"templateId"),
 ObservationDomainId=>(149,Unsigned,4,"observationDomainId"),
 FlowStartSeconds=>(150,Seconds,4,"flowStartSeconds"),
 FlowEndSeconds=>(151,Seconds,4,"flowEndSeconds"),
 FlowStartMilliseconds=>(152,Milliseconds,8,"flowStartMilliseconds"),
 FlowEndMilliseconds=>(153,Milliseconds,8,"flowEndMilliseconds"),
 SystemInitTimeMilliseconds=>(160,Milliseconds,8,"systemInitTimeMilliseconds"),
 SelectorId=>(302,Unsigned,8,"selectorId"),
 SelectorAlgorithm=>(304,Unsigned,2,"selectorAlgorithm"),
 SamplingPacketInterval=>(305,Unsigned,4,"samplingPacketInterval"),
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum FieldValue {
    Unsigned(u64),
    Ipv4(Ipv4Addr),
    Ipv6(Ipv6Addr),
    String(String),
    TimestampSeconds(u32),
    TimestampMilliseconds(u64),
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Field {
    pub element: Element,
    #[serde(default)]
    pub length: Option<u16>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Template {
    pub id: u16,
    #[serde(default)]
    pub scope_count: u16,
    pub fields: Vec<Field>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataSet {
    pub template_id: u16,
    pub records: Vec<Vec<FieldValue>>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Batch {
    pub observation_domain_id: u32,
    #[serde(default)]
    pub export_time: Option<u32>,
    pub templates: Vec<Template>,
    #[serde(default)]
    pub data_sets: Vec<DataSet>,
}
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct WireField {
    pub element_id: u16,
    pub enterprise: Option<u32>,
    pub element: Option<Element>,
    pub length: u16,
}
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct WireTemplate {
    pub id: u16,
    pub scope_count: u16,
    pub fields: Vec<WireField>,
}
impl Template {
    pub fn wire(&self) -> Result<WireTemplate> {
        ensure!(self.id >= 256, "template ID must be256..65535");
        ensure!(
            !self.fields.is_empty() && self.fields.len() <= MAX_FIELDS,
            "template field count1..32"
        );
        ensure!(
            usize::from(self.scope_count) <= self.fields.len(),
            "scope count exceeds fields"
        );
        let fields = self
            .fields
            .iter()
            .map(|f| {
                let length = f.length.unwrap_or(f.element.descriptor().1);
                validate_length(Some(f.element), length)?;
                Ok(WireField {
                    element_id: f.element.id(),
                    enterprise: None,
                    element: Some(f.element),
                    length,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let t = WireTemplate {
            id: self.id,
            scope_count: self.scope_count,
            fields,
        };
        ensure!(
            minimum_record_size(&t) <= MAX_MESSAGE_BYTES,
            "minimum record exceeds message bound"
        );
        Ok(t)
    }
}
fn validate_length(element: Option<Element>, length: u16) -> Result<()> {
    match element.map(Element::descriptor) {
        Some((Kind::Unsigned, width)) => ensure!(
            (1..=width).contains(&length),
            "unsigned field length/reduced-size bound"
        ),
        Some((Kind::Ipv4 | Kind::Ipv6 | Kind::Seconds | Kind::Milliseconds, width)) => {
            ensure!(length == width, "structured scalar cannot use reduced size")
        }
        Some((Kind::String, _)) | None => ensure!(
            length == 65535 || (length > 0 && usize::from(length) <= MAX_FIELD_BYTES),
            "field length must be1..1024 or65535"
        ),
    }
    Ok(())
}
fn minimum_record_size(t: &WireTemplate) -> usize {
    t.fields
        .iter()
        .map(|f| {
            if f.length == 65535 {
                1
            } else {
                usize::from(f.length)
            }
        })
        .sum()
}
fn append(out: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    ensure!(
        out.len() + bytes.len() <= MAX_MESSAGE_BYTES,
        "IPFIX message byte bound8192"
    );
    out.extend_from_slice(bytes);
    Ok(())
}
fn u16out(out: &mut Vec<u8>, v: u16) -> Result<()> {
    append(out, &v.to_be_bytes())
}
fn set_start(out: &mut Vec<u8>, id: u16) -> Result<usize> {
    let at = out.len();
    u16out(out, id)?;
    u16out(out, 0)?;
    Ok(at)
}
fn set_finish(out: &mut [u8], at: usize) {
    let n = (out.len() - at) as u16;
    out[at + 2..at + 4].copy_from_slice(&n.to_be_bytes());
}
pub fn encode(batch: &Batch, sequence: u32, export_time: u32) -> Result<(Vec<u8>, usize)> {
    ensure!(
        !batch.templates.is_empty() && batch.templates.len() <= MAX_TEMPLATES_PER_SESSION,
        "every export must provide1..32 templates"
    );
    ensure!(
        batch.data_sets.len() <= MAX_SETS - batch.templates.len(),
        "set count limit64"
    );
    let mut templates = BTreeMap::new();
    for t in &batch.templates {
        ensure!(
            templates.insert(t.id, t.wire()?).is_none(),
            "duplicate template ID"
        );
    }
    let mut out = Vec::with_capacity(MAX_MESSAGE_BYTES);
    append(&mut out, &[0, 10, 0, 0])?;
    append(&mut out, &export_time.to_be_bytes())?;
    append(&mut out, &sequence.to_be_bytes())?;
    append(&mut out, &batch.observation_domain_id.to_be_bytes())?;
    for t in templates.values() {
        let at = set_start(&mut out, if t.scope_count == 0 { 2 } else { 3 })?;
        u16out(&mut out, t.id)?;
        u16out(&mut out, t.fields.len() as u16)?;
        if t.scope_count > 0 {
            u16out(&mut out, t.scope_count)?;
        }
        for f in &t.fields {
            u16out(&mut out, f.element_id)?;
            u16out(&mut out, f.length)?;
        }
        set_finish(&mut out, at);
    }
    let mut count = 0;
    for set in &batch.data_sets {
        let t = templates
            .get(&set.template_id)
            .context("every data set needs a template in this batch")?;
        ensure!(!set.records.is_empty(), "empty data set");
        ensure!(
            count + set.records.len() <= MAX_RECORDS,
            "record count limit256"
        );
        let at = set_start(&mut out, set.template_id)?;
        for r in &set.records {
            ensure!(
                r.len() == t.fields.len(),
                "record field count must match template order"
            );
            for (f, v) in t.fields.iter().zip(r) {
                encode_value(&mut out, f, v)?;
            }
        }
        count += set.records.len();
        set_finish(&mut out, at);
    }
    let n = out.len() as u16;
    out[2..4].copy_from_slice(&n.to_be_bytes());
    Ok((out, count))
}
fn encode_value(out: &mut Vec<u8>, f: &WireField, v: &FieldValue) -> Result<()> {
    let element = f.element.context("outbound unsupported element")?;
    let (kind, _) = element.descriptor();
    match (kind, v) {
        (Kind::Unsigned, FieldValue::Unsigned(n)) => {
            ensure!(
                f.length == 8 || *n < (1u64 << (f.length * 8)),
                "unsigned value exceeds field length"
            );
            ensure!(
                element != Element::TcpControlBits || n & 0xf000 == 0,
                "tcpControlBits data-offset bits must be zero"
            );
            append(out, &n.to_be_bytes()[8 - usize::from(f.length)..])?;
        }
        (Kind::Ipv4, FieldValue::Ipv4(ip)) => append(out, &ip.octets())?,
        (Kind::Ipv6, FieldValue::Ipv6(ip)) => append(out, &ip.octets())?,
        (Kind::Seconds, FieldValue::TimestampSeconds(n)) => append(out, &n.to_be_bytes())?,
        (Kind::Milliseconds, FieldValue::TimestampMilliseconds(n)) => {
            append(out, &n.to_be_bytes())?
        }
        (Kind::String, FieldValue::String(s)) => {
            ensure!(s.len() <= MAX_FIELD_BYTES, "UTF-8 field byte bound1024");
            if f.length == 65535 {
                if s.len() < 255 {
                    append(out, &[s.len() as u8])?;
                } else {
                    append(out, &[255])?;
                    u16out(out, s.len() as u16)?;
                }
                append(out, s.as_bytes())?;
            } else {
                ensure!(
                    s.len() == usize::from(f.length),
                    "fixed string byte count must match declared field length"
                );
                append(out, s.as_bytes())?;
            }
        }
        _ => bail!("value kind does not match {}", element.iana_name()),
    }
    Ok(())
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
        ensure!(n <= self.left(), "truncated IPFIX field/set");
        let s = &self.bytes[self.at..self.at + n];
        self.at += n;
        Ok(s)
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into()?))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into()?))
    }
}
#[derive(Clone)]
struct CachedTemplate {
    template: WireTemplate,
    received: Instant,
}
#[derive(Clone)]
struct Session {
    templates: BTreeMap<u16, CachedTemplate>,
    expected: Option<u32>,
    last_seen: Instant,
}
#[derive(Debug, Serialize)]
pub struct SequenceTracking {
    pub expected: Option<u32>,
    pub status: &'static str,
    pub missing_records: Option<u32>,
}
#[derive(Debug, Serialize)]
pub struct TemplateChange {
    pub template: WireTemplate,
    pub change: &'static str,
}
#[derive(Debug, Serialize)]
pub struct DecodedDataSet {
    pub template: WireTemplate,
    pub records: Vec<Vec<Option<FieldValue>>>,
}
#[derive(Debug, Serialize)]
pub struct UnknownDataSet {
    pub template_id: u16,
    pub byte_count: usize,
}
#[derive(Debug, Serialize)]
pub struct Message {
    pub export_time: u32,
    pub sequence_number: u32,
    pub observation_domain_id: u32,
    pub sequence_tracking: SequenceTracking,
    pub template_changes: Vec<TemplateChange>,
    pub ignored_withdrawals: Vec<u16>,
    pub data_sets: Vec<DecodedDataSet>,
    pub unknown_data_sets: Vec<UnknownDataSet>,
    pub record_count: usize,
}
pub struct TemplateCache {
    sessions: BTreeMap<(SocketAddr, u32), Session>,
    template_ttl: Duration,
    session_idle: Duration,
}
impl TemplateCache {
    pub fn new(template_ttl: Duration, session_idle: Duration) -> Self {
        Self {
            sessions: BTreeMap::new(),
            template_ttl,
            session_idle,
        }
    }
    pub fn counts(&self) -> (usize, usize) {
        (
            self.sessions.len(),
            self.sessions.values().map(|s| s.templates.len()).sum(),
        )
    }
    pub fn expire(&mut self, now: Instant) {
        self.sessions.retain(|_, s| {
            s.templates
                .retain(|_, t| now.duration_since(t.received) < self.template_ttl);
            now.duration_since(s.last_seen) < self.session_idle
        });
    }
    pub fn ingest(&mut self, peer: SocketAddr, bytes: &[u8], now: Instant) -> Result<Message> {
        self.expire(now);
        ensure!(
            (16..=MAX_MESSAGE_BYTES).contains(&bytes.len()),
            "IPFIX message byte bound16..8192"
        );
        let mut r = Reader::new(bytes);
        ensure!(r.u16()? == 10, "IPFIX version10 required");
        ensure!(
            usize::from(r.u16()?) == bytes.len(),
            "IPFIX header length must match UDP datagram"
        );
        let export_time = r.u32()?;
        let sequence = r.u32()?;
        let domain = r.u32()?;
        let key = (peer, domain);
        ensure!(
            self.sessions.contains_key(&key) || self.sessions.len() < MAX_SESSIONS,
            "IPFIX session/domain cap128"
        );
        let mut candidate = self.sessions.get(&key).cloned().unwrap_or(Session {
            templates: BTreeMap::new(),
            expected: None,
            last_seen: now,
        });
        let expected = candidate.expected;
        let delta = expected.map(|e| sequence.wrapping_sub(e));
        let status = match delta {
            None => "untracked",
            Some(0) => "in_order",
            Some(d) if d < 0x8000_0000 => "gap",
            Some(_) => "out_of_order_or_duplicate",
        };
        let mut message = Message {
            export_time,
            sequence_number: sequence,
            observation_domain_id: domain,
            sequence_tracking: SequenceTracking {
                expected,
                status,
                missing_records: delta.filter(|d| *d > 0 && *d < 0x8000_0000),
            },
            template_changes: vec![],
            ignored_withdrawals: vec![],
            data_sets: vec![],
            unknown_data_sets: vec![],
            record_count: 0,
        };
        let mut sets = 0;
        let mut template_records = 0;
        while r.left() > 0 {
            sets += 1;
            ensure!(sets <= MAX_SETS, "set count limit64");
            let id = r.u16()?;
            let len = usize::from(r.u16()?);
            ensure!(len >= 4, "set length must include header");
            let mut set = Reader::new(r.take(len - 4)?);
            if id == 2 || id == 3 {
                while set.left() >= 4 {
                    template_records += 1;
                    ensure!(
                        template_records <= MAX_TEMPLATES_PER_SESSION,
                        "template record count limit32"
                    );
                    let tid = set.u16()?;
                    let count = usize::from(set.u16()?);
                    if count == 0 {
                        ensure!(tid >= 256 || tid == id, "invalid UDP withdrawal ID");
                        message.ignored_withdrawals.push(tid);
                        continue;
                    }
                    ensure!(
                        tid >= 256 && count <= MAX_FIELDS,
                        "template ID/field count bound"
                    );
                    let scope = if id == 3 { set.u16()? } else { 0 };
                    ensure!(
                        id != 3 || (scope > 0 && usize::from(scope) <= count),
                        "options scope count must be1..field_count"
                    );
                    let mut fields = Vec::with_capacity(count);
                    for _ in 0..count {
                        let n = set.u16()?;
                        let length = set.u16()?;
                        let enterprise = if n & 0x8000 != 0 {
                            Some(set.u32()?)
                        } else {
                            None
                        };
                        let element_id = n & 0x7fff;
                        ensure!(element_id != 0, "reserved element ID0");
                        let element = if enterprise.is_none() {
                            Element::from_id(element_id)
                        } else {
                            None
                        };
                        validate_length(element, length)?;
                        fields.push(WireField {
                            element_id,
                            enterprise,
                            element,
                            length,
                        });
                    }
                    let t = WireTemplate {
                        id: tid,
                        scope_count: scope,
                        fields,
                    };
                    ensure!(
                        minimum_record_size(&t) <= MAX_MESSAGE_BYTES,
                        "minimum record exceeds message bound"
                    );
                    let change = match candidate.templates.get(&tid) {
                        None => Some("new"),
                        Some(old) if old.template != t => Some("replaced"),
                        _ => None,
                    };
                    ensure!(
                        candidate.templates.contains_key(&tid)
                            || candidate.templates.len() < MAX_TEMPLATES_PER_SESSION,
                        "per-session template cap32"
                    );
                    if let Some(change) = change {
                        message.template_changes.push(TemplateChange {
                            template: t.clone(),
                            change,
                        });
                    }
                    candidate.templates.insert(
                        tid,
                        CachedTemplate {
                            template: t,
                            received: now,
                        },
                    );
                } // RFC7011 permits padding shorter than a record; nonzero padding is not malformed.
            } else if id >= 256 {
                if let Some(cached) = candidate.templates.get(&id) {
                    let t = &cached.template;
                    let min = minimum_record_size(t);
                    let mut records = vec![];
                    while set.left() >= min {
                        ensure!(message.record_count < MAX_RECORDS, "record count limit256");
                        let mut values = Vec::with_capacity(t.fields.len());
                        for f in &t.fields {
                            let n = if f.length == 65535 {
                                let first = set.take(1)?[0];
                                if first == 255 {
                                    usize::from(set.u16()?)
                                } else {
                                    usize::from(first)
                                }
                            } else {
                                usize::from(f.length)
                            };
                            ensure!(n <= MAX_FIELD_BYTES, "field byte limit1024");
                            let bytes = set.take(n)?;
                            values.push(decode_value(f, bytes)?);
                        }
                        records.push(values);
                        message.record_count += 1;
                    }
                    message.data_sets.push(DecodedDataSet {
                        template: t.clone(),
                        records,
                    });
                } else {
                    message.unknown_data_sets.push(UnknownDataSet {
                        template_id: id,
                        byte_count: set.left(),
                    });
                }
            } else {
                bail!("reserved IPFIX set ID");
            }
        }
        let old_count = self
            .sessions
            .get(&key)
            .map(|s| s.templates.len())
            .unwrap_or(0);
        ensure!(
            self.counts().1 - old_count + candidate.templates.len() <= MAX_CACHED_TEMPLATES,
            "global template cap1024"
        );
        if !message.unknown_data_sets.is_empty() {
            candidate.expected = None;
        } else if delta.is_none_or(|d| d < 0x8000_0000) {
            candidate.expected = Some(sequence.wrapping_add(message.record_count as u32));
        }
        candidate.last_seen = now;
        self.sessions.insert(key, candidate);
        Ok(message)
    }
}
fn decode_value(f: &WireField, bytes: &[u8]) -> Result<Option<FieldValue>> {
    let Some(element) = f.element else {
        return Ok(None);
    };
    let (kind, _) = element.descriptor();
    let unsigned = || bytes.iter().fold(0u64, |n, b| (n << 8) | u64::from(*b));
    Ok(Some(match kind {
        Kind::Unsigned => {
            let mut n = unsigned();
            if element == Element::TcpControlBits {
                n &= 0x0fff;
            }
            FieldValue::Unsigned(n)
        }
        Kind::Ipv4 => FieldValue::Ipv4(Ipv4Addr::from(<[u8; 4]>::try_from(bytes)?)),
        Kind::Ipv6 => FieldValue::Ipv6(Ipv6Addr::from(<[u8; 16]>::try_from(bytes)?)),
        Kind::Seconds => FieldValue::TimestampSeconds(unsigned() as u32),
        Kind::Milliseconds => FieldValue::TimestampMilliseconds(unsigned()),
        Kind::String => FieldValue::String(std::str::from_utf8(bytes)?.to_owned()),
    }))
}
