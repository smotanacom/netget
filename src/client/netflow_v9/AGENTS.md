# NetFlow v9 UDP exporter — Experimental

`export_netflow_v9_records` accepts a typed `batch` with unsigned32 `source_id`,
optional unsigned32 `export_time`/`sys_uptime_ms`,1..32templates, and ordered
data sets. Templates have ID>=256, `scope_count` default0 and ordered fields.
Each field declares exactly one supported `element` or `scope`, with optional
fixed `length`; scopes must precede options. Options need at least one scope
and one option. Each record is an aligned array of `{kind,value}` fields:
unsigned, ipv4, ipv6 or uptime_milliseconds. The server codec/docs enumerate
the selected35RFC fields and five scope kinds. Unknown properties and raw
values fail validation. See `actions::example_batch` for a minimal IPv4 flow.

The whole batch is validated before one UDP packet is emitted. Referenced
templates must be included and are always repeated before their data. Header
Count includes all templates plus data/options records. Source-ID packet
sequence starts0 and increments once per successful local packet, including
template-only refresh; data-record count never determines sequence. At most
32Source IDs and32active templates each are retained. Identical definitions
are accepted; template-ID redefinition requires a new exporter instance.
Candidate state commits only after local send success. A UDP source port change
is not a distinct collector domain, so redefinition may replace a collector's
old source-IP/source-ID definition when the new template reaches it.

Each Source ID's sysUpTime defaults to monotonic elapsed milliseconds since
its first batch, modulo2^32; an explicit caller value becomes that source's
new base for subsequent sends and refresh. UNIX export seconds default to the
system clock; an explicit value applies to that command only. Active templates
refresh periodically (`template_refresh_seconds`,60default,1..3600) with current
clock values. Every command also repeats its templates. There is no separate
packet-count refresh setting, deletion/withdrawal, automatic data retry or flow
store. Refresh packets for all active Source IDs are bounded; candidate states
commit after all local sends succeed. A partial refresh failure closes the
client, discarding the catalog; already accepted UDP packets cannot be undone.

One owned task polls sends, injected commands, refresh ticks and common
handlers independently. Parked handlers do not block injection/refresh.
Resolution and each send operation have ten-second deadlines, one operation
is in flight, and events/actions are bounded32 each with followup depth8.
Capacity exhaustion closes the client. Unsolicited datagrams or transport
errors close its one-way session. Disconnect/removal cancels sends, timers,
queues, command handle and parked intercepts. `netflow_v9_connected` means a
logical UDP exporter is ready; `netflow_v9_exported` reports sequence, Count,
record/template/byte counts and `local_transport_only:true`. Neither command
completion nor those events acknowledge collector receipt, decoding, processing,
storage or durability. Common handlers/scripts and client memory are used;
a blank instruction without a handler makes no model call.

Limits match the server codec:8192bytes,64FlowSets,256data/options records,
32fields, minimum4bytes/record and fixed scalar widths. Counters are bounded
unsigned1..native maximum bytes, IPv4/IPv6/ports/flags/uptime have fixed widths,
and selected scopes are unsigned1..8. No variable-length fields, enterprise
bit, vendor extensions or raw/base64 values are exported. Tests require an
actual pinned GoFlow2 2.2.7 service to decode native IPv4/IPv6, large counters,
relative time and options scope/value semantics. Native pair/common memory,
packet sequence including refresh, atomic validation, bounds and cancellation
are separately checked. The server docs link primary references and licenses.
No protocol dependency, TCP/SCTP/TLS/auth, data retry, full RFC/vendor model,
flow capture/storage, durability, fuzz or production pcap claim is made.
