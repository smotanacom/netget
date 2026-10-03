//! Bounded RFC3954 v9 UDP framing; transient wire state, never a flow database.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
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
#[derive(Clone, Copy)]
enum Kind {
    Counter,
    Unsigned,
    Ipv4,
    Ipv6,
    Uptime,
}
macro_rules! elements {($($variant:ident=>($id:literal,$kind:ident,$width:literal,$name:literal)),* $(,)?)=>{
 #[derive(Clone,Copy,Debug,Serialize,Deserialize,PartialEq,Eq)]
 #[serde(rename_all="snake_case")]
 pub enum Element {$($variant),*}
 impl Element {
  pub fn id(self)->u16{match self{$(Self::$variant=>$id),*}}
  pub fn field_name(self)->&'static str{match self{$(Self::$variant=>$name),*}}
  fn descriptor(self)->(Kind,u16){match self{$(Self::$variant=>(Kind::$kind,$width)),*}}
  fn from_id(id:u16)->Option<Self>{match id{$($id=>Some(Self::$variant)),*,_=>None}}
  fn default_length(self)->u16 {let (kind,width)=self.descriptor();match kind {Kind::Counter=>width.min(4),_=>width}}
 }
}}
elements! {
 InBytes=>(1,Counter,8,"IN_BYTES"), InPackets=>(2,Counter,8,"IN_PKTS"), Flows=>(3,Counter,8,"FLOWS"),
 Protocol=>(4,Unsigned,1,"PROTOCOL"), Tos=>(5,Unsigned,1,"TOS"), TcpFlags=>(6,Unsigned,1,"TCP_FLAGS"),
 SourcePort=>(7,Unsigned,2,"L4_SRC_PORT"), SourceIpv4=>(8,Ipv4,4,"IPV4_SRC_ADDR"), SourceMask=>(9,Unsigned,1,"SRC_MASK"),
 InputInterface=>(10,Counter,4,"INPUT_SNMP"), DestinationPort=>(11,Unsigned,2,"L4_DST_PORT"), DestinationIpv4=>(12,Ipv4,4,"IPV4_DST_ADDR"),
 DestinationMask=>(13,Unsigned,1,"DST_MASK"), OutputInterface=>(14,Counter,4,"OUTPUT_SNMP"), NextHopIpv4=>(15,Ipv4,4,"IPV4_NEXT_HOP"),
 SourceAs=>(16,Counter,4,"SRC_AS"), DestinationAs=>(17,Counter,4,"DST_AS"), BgpNextHopIpv4=>(18,Ipv4,4,"BGP_IPV4_NEXT_HOP"),
 LastSwitched=>(21,Uptime,4,"LAST_SWITCHED"), FirstSwitched=>(22,Uptime,4,"FIRST_SWITCHED"),
 OutBytes=>(23,Counter,8,"OUT_BYTES"), OutPackets=>(24,Counter,8,"OUT_PKTS"),
 SourceIpv6=>(27,Ipv6,16,"IPV6_SRC_ADDR"), DestinationIpv6=>(28,Ipv6,16,"IPV6_DST_ADDR"),
 SourceIpv6Mask=>(29,Unsigned,1,"IPV6_SRC_MASK"), DestinationIpv6Mask=>(30,Unsigned,1,"IPV6_DST_MASK"),
 IcmpType=>(32,Unsigned,2,"ICMP_TYPE"), SamplingInterval=>(34,Unsigned,4,"SAMPLING_INTERVAL"), SamplingAlgorithm=>(35,Unsigned,1,"SAMPLING_ALGORITHM"),
 SourceVlan=>(58,Unsigned,2,"SRC_VLAN"), DestinationVlan=>(59,Unsigned,2,"DST_VLAN"), IpVersion=>(60,Unsigned,1,"IP_PROTOCOL_VERSION"),
 Direction=>(61,Unsigned,1,"DIRECTION"), NextHopIpv6=>(62,Ipv6,16,"IPV6_NEXT_HOP"), BgpNextHopIpv6=>(63,Ipv6,16,"BGP_IPV6_NEXT_HOP"),
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    System,
    Interface,
    LineCard,
    Cache,
    Template,
}
impl Scope {
    fn id(self) -> u16 {
        match self {
            Self::System => 1,
            Self::Interface => 2,
            Self::LineCard => 3,
            Self::Cache => 4,
            Self::Template => 5,
        }
    }
    fn from_id(id: u16) -> Option<Self> {
        match id {
            1 => Some(Self::System),
            2 => Some(Self::Interface),
            3 => Some(Self::LineCard),
            4 => Some(Self::Cache),
            5 => Some(Self::Template),
            _ => None,
        }
    }
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
    UptimeMilliseconds(u32),
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Field {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element: Option<Element>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<Scope>,
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
    pub source_id: u32,
    #[serde(default)]
    pub export_time: Option<u32>,
    #[serde(default)]
    pub sys_uptime_ms: Option<u32>,
    pub templates: Vec<Template>,
    #[serde(default)]
    pub data_sets: Vec<DataSet>,
}
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct WireField {
    pub field_type: u16,
    pub scope: Option<Scope>,
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
            usize::from(self.scope_count) < self.fields.len(),
            "options templates need scopes followed by at least one option"
        );
        let mut fields = Vec::with_capacity(self.fields.len());
        for (i, f) in self.fields.iter().enumerate() {
            let scoped = i < usize::from(self.scope_count);
            ensure!(
                if scoped {
                    f.scope.is_some() && f.element.is_none()
                } else {
                    f.element.is_some() && f.scope.is_none()
                },
                "fields must declare exactly one scope/element in template order"
            );
            let length = f
                .length
                .unwrap_or_else(|| f.element.map(Element::default_length).unwrap_or(4));
            validate_length(f.element, f.scope.is_some(), length)?;
            fields.push(WireField {
                field_type: if let Some(e) = f.element {
                    e.id()
                } else {
                    f.scope.context("scope")?.id()
                },
                scope: f.scope,
                element: f.element,
                length,
            });
        }
        let t = WireTemplate {
            id: self.id,
            scope_count: self.scope_count,
            fields,
        };
        validate_record_size(&t)?;
        Ok(t)
    }
}
fn validate_length(element: Option<Element>, scoped: bool, length: u16) -> Result<()> {
    ensure!(
        length > 0 && usize::from(length) <= MAX_FIELD_BYTES,
        "fixed field byte bound1..1024; no IPFIX variable-length marker"
    );
    if scoped {
        ensure!(length <= 8, "selected unsigned scope width1..8");
    } else if let Some(e) = element {
        let (kind, width) = e.descriptor();
        match kind {
            Kind::Counter => ensure!(length <= width, "counter width bound"),
            _ => ensure!(length == width, "fixed scalar width required"),
        }
    }
    Ok(())
}
fn record_size(t: &WireTemplate) -> usize {
    t.fields.iter().map(|f| usize::from(f.length)).sum()
}
fn validate_record_size(t: &WireTemplate) -> Result<()> {
    ensure!(
        (4..=MAX_MESSAGE_BYTES).contains(&record_size(t)),
        "selected record byte size4..8192; shorter records have ambiguous v9 padding"
    );
    Ok(())
}
fn append(out: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    ensure!(
        out.len() + bytes.len() <= MAX_MESSAGE_BYTES,
        "NetFlow v9 message byte bound8192"
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
fn set_finish(out: &mut Vec<u8>, at: usize) -> Result<()> {
    let padding = (4 - (out.len() - at) % 4) % 4;
    append(out, &[0; 3][..padding])?;
    let n = (out.len() - at) as u16;
    out[at + 2..at + 4].copy_from_slice(&n.to_be_bytes());
    Ok(())
}
pub fn encode(
    batch: &Batch,
    sequence: u32,
    export_time: u32,
    sys_uptime_ms: u32,
) -> Result<(Vec<u8>, usize)> {
    ensure!(
        !batch.templates.is_empty() && batch.templates.len() <= MAX_TEMPLATES_PER_SESSION,
        "every export must provide1..32 templates"
    );
    ensure!(
        batch.data_sets.len() + batch.templates.len() <= MAX_SETS,
        "flowset count bound64"
    );
    let mut templates = BTreeMap::new();
    for t in &batch.templates {
        ensure!(
            templates.insert(t.id, t.wire()?).is_none(),
            "duplicate template ID"
        );
    }
    let mut out = Vec::with_capacity(MAX_MESSAGE_BYTES);
    append(&mut out, &[0, 9, 0, 0])?;
    for n in [sys_uptime_ms, export_time, sequence, batch.source_id] {
        append(&mut out, &n.to_be_bytes())?;
    }
    for t in templates.values() {
        let at = set_start(&mut out, if t.scope_count == 0 { 0 } else { 1 })?;
        u16out(&mut out, t.id)?;
        if t.scope_count > 0 {
            u16out(&mut out, t.scope_count * 4)?;
            u16out(&mut out, (t.fields.len() as u16 - t.scope_count) * 4)?;
        } else {
            u16out(&mut out, t.fields.len() as u16)?;
        }
        for f in &t.fields {
            u16out(&mut out, f.field_type)?;
            u16out(&mut out, f.length)?;
        }
        set_finish(&mut out, at)?;
    }
    let mut count = 0;
    for set in &batch.data_sets {
        let t = templates
            .get(&set.template_id)
            .context("every data set needs a template in this batch")?;
        ensure!(!set.records.is_empty(), "empty data flowset");
        ensure!(
            count + set.records.len() <= MAX_RECORDS,
            "data/options record count bound256"
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
        set_finish(&mut out, at)?;
    }
    let total = (templates.len() + count) as u16;
    out[2..4].copy_from_slice(&total.to_be_bytes());
    Ok((out, count))
}
fn encode_value(out: &mut Vec<u8>, f: &WireField, v: &FieldValue) -> Result<()> {
    if f.scope.is_some()
        || f.element
            .is_some_and(|e| matches!(e.descriptor().0, Kind::Counter | Kind::Unsigned))
    {
        let FieldValue::Unsigned(n) = v else {
            bail!("unsigned field value required")
        };
        ensure!(
            f.length == 8 || *n < (1u64 << (f.length * 8)),
            "unsigned value exceeds field width"
        );
        append(out, &n.to_be_bytes()[8 - usize::from(f.length)..])?;
        return Ok(());
    }
    let e = f.element.context("outbound unsupported field")?;
    match (e.descriptor().0, v) {
        (Kind::Ipv4, FieldValue::Ipv4(ip)) => append(out, &ip.octets())?,
        (Kind::Ipv6, FieldValue::Ipv6(ip)) => append(out, &ip.octets())?,
        (Kind::Uptime, FieldValue::UptimeMilliseconds(n)) => append(out, &n.to_be_bytes())?,
        _ => bail!("value kind does not match {}", e.field_name()),
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
        ensure!(n <= self.left(), "truncated NetFlow v9 field/set");
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
    uptime: Option<u32>,
    export_time: Option<u32>,
}
#[derive(Debug, Serialize)]
pub struct SequenceTracking {
    pub expected: Option<u32>,
    pub status: &'static str,
    pub missing_packets: Option<u32>,
    pub uptime_decreased: bool,
    pub cache_reset: Option<&'static str>,
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
    pub sys_uptime_ms: u32,
    pub export_time: u32,
    pub sequence_number: u32,
    pub source_id: u32,
    pub header_count: u16,
    pub known_total_record_count: usize,
    pub count_status: &'static str,
    pub sequence_tracking: SequenceTracking,
    pub template_changes: Vec<TemplateChange>,
    pub data_sets: Vec<DecodedDataSet>,
    pub unknown_data_sets: Vec<UnknownDataSet>,
    pub record_count: usize,
}
pub struct TemplateCache {
    sessions: BTreeMap<(IpAddr, u32), Session>,
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
            (24..=MAX_MESSAGE_BYTES).contains(&bytes.len()),
            "NetFlow v9 datagram byte bound24..8192"
        );
        let mut r = Reader::new(bytes);
        ensure!(r.u16()? == 9, "NetFlow v9 version9 required");
        let header_count = r.u16()?;
        ensure!(
            header_count > 0
                && usize::from(header_count) <= MAX_RECORDS + MAX_TEMPLATES_PER_SESSION,
            "header total record count bound1..288"
        );
        let uptime = r.u32()?;
        let export_time = r.u32()?;
        let sequence = r.u32()?;
        let source_id = r.u32()?;
        let key = (peer.ip(), source_id);
        ensure!(
            self.sessions.contains_key(&key) || self.sessions.len() < MAX_SESSIONS,
            "source-IP/source-ID session cap128"
        );
        let mut candidate = self.sessions.get(&key).cloned().unwrap_or(Session {
            templates: BTreeMap::new(),
            expected: None,
            last_seen: now,
            uptime: None,
            export_time: None,
        });
        let expected = candidate.expected;
        let delta = expected.map(|e| sequence.wrapping_sub(e));
        let advancing = delta.is_none_or(|d| d < 0x8000_0000);
        let reset = advancing && candidate.export_time.is_some_and(|old| export_time < old);
        if reset {
            candidate.templates.clear();
        }
        let status = if reset {
            "clock_regression"
        } else {
            match delta {
                None => "untracked",
                Some(0) => "in_order",
                Some(d) if d < 0x8000_0000 => "gap",
                _ => "out_of_order_or_duplicate",
            }
        };
        let mut message = Message {
            sys_uptime_ms: uptime,
            export_time,
            sequence_number: sequence,
            source_id,
            header_count,
            known_total_record_count: 0,
            count_status: "validated",
            sequence_tracking: SequenceTracking {
                expected,
                status,
                missing_packets: delta.filter(|d| *d > 0 && *d < 0x8000_0000),
                uptime_decreased: candidate.uptime.is_some_and(|old| uptime < old),
                cache_reset: if reset {
                    Some("clock_regression")
                } else {
                    None
                },
            },
            template_changes: vec![],
            data_sets: vec![],
            unknown_data_sets: vec![],
            record_count: 0,
        };
        let mut sets = 0;
        let mut template_records = 0;
        while r.left() > 0 {
            sets += 1;
            ensure!(sets <= MAX_SETS, "flowset count bound64");
            let id = r.u16()?;
            let length = usize::from(r.u16()?);
            ensure!(length >= 4, "flowset length includes header");
            let mut set = Reader::new(r.take(length - 4)?);
            if id == 0 || id == 1 {
                let min_header = if id == 0 { 4 } else { 6 };
                let mut in_set = 0;
                while set.left() >= min_header {
                    template_records += 1;
                    in_set += 1;
                    ensure!(
                        template_records <= MAX_TEMPLATES_PER_SESSION,
                        "template record count bound32"
                    );
                    let tid = set.u16()?;
                    let first = set.u16()?;
                    let (scope, count) = if id == 1 {
                        let option = set.u16()?;
                        ensure!(
                            first > 0 && option > 0 && first % 4 == 0 && option % 4 == 0,
                            "options scope/option lengths are positive multiples of4 bytes"
                        );
                        (first / 4, usize::from(first / 4) + usize::from(option / 4))
                    } else {
                        (0, usize::from(first))
                    };
                    ensure!(
                        tid >= 256 && (1..=MAX_FIELDS).contains(&count),
                        "template ID/field count bound; v9 has no withdrawal record"
                    );
                    let mut fields = Vec::with_capacity(count);
                    for i in 0..count {
                        let field_type = set.u16()?;
                        let length = set.u16()?;
                        ensure!(field_type != 0, "reserved field type0");
                        let scoped = i < usize::from(scope);
                        let element = if scoped {
                            None
                        } else {
                            Element::from_id(field_type)
                        };
                        let scope_type = if scoped {
                            Scope::from_id(field_type)
                        } else {
                            None
                        };
                        validate_length(element, scope_type.is_some(), length)?;
                        fields.push(WireField {
                            field_type,
                            element,
                            scope: scope_type,
                            length,
                        });
                    }
                    let t = WireTemplate {
                        id: tid,
                        scope_count: scope,
                        fields,
                    };
                    validate_record_size(&t)?;
                    let change = match candidate.templates.get(&tid) {
                        None => Some("new"),
                        Some(old) if old.template != t => Some("replaced"),
                        _ => None,
                    };
                    ensure!(
                        candidate.templates.contains_key(&tid)
                            || candidate.templates.len() < MAX_TEMPLATES_PER_SESSION,
                        "per-source template cap32"
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
                }
                ensure!(
                    in_set > 0 && set.left() <= 3,
                    "empty template flowset or excessive padding"
                );
            } else if id >= 256 {
                ensure!(set.left() > 0, "empty data flowset");
                if let Some(cached) = candidate.templates.get(&id) {
                    let t = &cached.template;
                    let size = record_size(t);
                    let mut records = vec![];
                    while set.left() >= size {
                        ensure!(
                            message.record_count < MAX_RECORDS,
                            "data/options record count bound256"
                        );
                        let mut values = Vec::with_capacity(t.fields.len());
                        for f in &t.fields {
                            values.push(decode_value(f, set.take(usize::from(f.length))?)?);
                        }
                        records.push(values);
                        message.record_count += 1;
                    }
                    ensure!(
                        !records.is_empty() && set.left() <= 3,
                        "incomplete data record or excessive padding"
                    );
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
                bail!("reserved NetFlow v9 FlowSet ID");
            }
        }
        message.known_total_record_count = template_records + message.record_count;
        if message.unknown_data_sets.is_empty() {
            ensure!(
                message.known_total_record_count == usize::from(header_count),
                "NetFlow v9 Count must include all template and data/options records"
            );
        } else {
            ensure!(
                message.known_total_record_count + message.unknown_data_sets.len()
                    <= usize::from(header_count),
                "each unknown-template flowset needs at least one remaining header record"
            );
            message.count_status = "unverifiable_unknown_template";
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
        if advancing {
            candidate.expected = Some(sequence.wrapping_add(1));
            candidate.uptime = Some(uptime);
            candidate.export_time = Some(export_time);
        }
        candidate.last_seen = now;
        self.sessions.insert(key, candidate);
        Ok(message)
    }
}
fn decode_value(f: &WireField, bytes: &[u8]) -> Result<Option<FieldValue>> {
    let unsigned = || bytes.iter().fold(0u64, |n, b| (n << 8) | u64::from(*b));
    if f.scope.is_some() {
        return Ok(Some(FieldValue::Unsigned(unsigned())));
    }
    let Some(e) = f.element else { return Ok(None) };
    Ok(Some(match e.descriptor().0 {
        Kind::Counter | Kind::Unsigned => FieldValue::Unsigned(unsigned()),
        Kind::Ipv4 => FieldValue::Ipv4(Ipv4Addr::from(<[u8; 4]>::try_from(bytes)?)),
        Kind::Ipv6 => FieldValue::Ipv6(Ipv6Addr::from(<[u8; 16]>::try_from(bytes)?)),
        Kind::Uptime => FieldValue::UptimeMilliseconds(unsigned() as u32),
    }))
}
