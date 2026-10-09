# Diameter server — selected Experimental TCP/NASREQ scope

`diameter` implements native bounded version1 TCP messages from
[RFC6733](https://www.rfc-editor.org/rfc/rfc6733.html) and a useful selected
[RFC7155 NASREQ](https://www.rfc-editor.org/rfc/rfc7155.html) application1.
The native transport answers CER/CEA, DWR/DWA and DPR/DPA. The shared handler
chooses a typed `respond_diameter_aa` verdict for AAR/AAA command265.

This is a trusted clear TCP3868 test endpoint. No TLS/DTLS/SCTP, certificate
identity, integrity, full RFC compliance or production-readiness claim. Origin
host/realm consistency checks do not authenticate the peer. Advertise only
NASREQ; parse other base capability advertisements, including bounded flat
Vendor-Specific-Application-Id groups, without enabling their AAA applications.
No agents, routing, relay/proxy operation, failover or vendor AAA policy.

Startup requires `origin_host` and `origin_realm`. Selected identity syntax is
ASCII letters/digits/dot/hyphen,1..255bytes. `io_timeout_seconds`90,
`handler_timeout_seconds`30 and `watchdog_interval_seconds`30 default, each
1..300. Writes have a10second deadline. `llm_fallback=false` by default:
unmatched events reject without model calls. Explicit static/script/manual
handlers always run. Startup parameters are removed from cloned handler contexts.

`diameter_aa_request.request` carries `username`, optional `password`,
`auth_request_type`, optional `nas_identifier`/`nas_port`, `session_id`,
`origin_host`, `origin_realm`, and `source_addr`. The password intentionally
reaches the selected common handler and shared access-log request. Shared
credential detection suppresses incidental reflected model output, script
stderr and errors. The private access-log response describes action counts;
original typed actions and actual wire attributes remain intact.

Supported request types are1 authenticate-only,2 authorize-only (no password),
and3 authenticate-and-authorize. This is UTF8 PAP: no binary password, NUL,
CHAP/MSCHAP/EAP or multi-round flow. Username and NAS-Identifier<=255bytes,
password<=128. Require Auth-Session-State=NO_STATE_MAINTAINED(1) on each request;
no durable session or earlier authentication binding is asserted. Stateful
sessions, STR/ASR/RAR, accounting and durable recordkeeping are excluded.

`respond_diameter_aa.reply` is a strict object with `verdict` accept/reject/error
(default reject), optional `reply_messages`/`filter_ids` (<=8each, UTF8<=1024bytes),
`service_type`1..19, and u32 `session_timeout`. These become Result-Code2001,
4001,5012 and typed AVPs. Returned service/filter/timeout fields are peer-facing
assertions; NetGet applies no NAS/device policy. Shared access recording finishes
before any successful answer. A failed common action alongside a valid accept,
duplicate verdict, malformed reply or backend failure must not accept. Overload
maps to3004 with E-bit; other backend failure maps to5012. No-action rejects.
Malformed selected request fields receive generic5012; unsupported mandatory
AVPs receive5001 with Failed-AVP. This selected scope does not implement all
per-AVP diagnostic result codes and missing-field placeholder groups.

Frames<=16KiB,64AVPs,256accepted connections,one active AAA handler per peer.
Action JSON<=16KiB/4096nodes/depth16 is checked before copies or decoding;
rejected constructed trees are disposed of iteratively. Unknown optional AVPs
are ignored; unsupported mandatory AVPs reject with5001 and a native Failed-AVP.
Header/AVP reserved bits and received padding are ignored; native output clears
reserved bits and pads with zeros. Malformed length/version is refused before
body allocation. Selected identity comparisons are ASCII case-insensitive.
Product-Name, Firmware-Revision and Error-Message use their defined clear M-bit.
PAP passwords are at most128bytes as specified by RFC7155 section4.3.1.
Session-Id is the first AVP in native AAR/AAA and applicable error answers;
received NASREQ requests must preserve that fixed position. Capability refusals
include the required base CEA identities, address, vendor and product fields,
then close without invoking an AAA handler. Aggregate encoding is bounded before
each AVP copy as well as at final frame validation.

A retained reader future owns the socket half, so partial frame reads are not
canceled by another select arm. Native watchdog/disconnect stays responsive
while AAA is parked. A second concurrent AAA request gets3004; unexpected
answers or changed identity close. Disconnect, EOF, deadlines and AppState
removal cancel the handler and owned socket; no detached reader or replay.
Per-connection and listener tasks use `spawn_server_task`. Transport counters
and the bounded shared access log are used; no protocol account/policy store.

Required unchanged peers and their honest scope are documented in
[tests](../../../tests/server/diameter/AGENTS.md). No fuzz or production-capture
claim. Keep parser/policy/cancellation regressions in `tests/`, never `src/`.
