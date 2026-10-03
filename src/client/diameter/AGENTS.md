# Diameter client — selected Experimental negotiated TCP peer

`diameter` completes TCP/CER/CEA before reporting `diameter_connected`, then
keeps one bounded native peer connection for selected RFC7155 NASREQ AAR/AAA.
The selected server scope, AVPs and exclusions are in the
[server guide](../../server/diameter/AGENTS.md). Origin identities are consistent
clear TCP identifiers, not authenticated identities. No TLS/SCTP, stateful AAA,
accounting, routing/agents, failover or replay.

Startup: required local `origin_host`/`origin_realm`; default whole-frame/connect
and watchdog-answer90s,handler30s,watchdogidle30s, each configurable1..300s;
writes10s. `send_diameter_aa` accepts top-level UTF8 `username` and `password`,
`auth_request_type`1/2/3(default3), optional `nas_identifier` and u32 `nas_port`.
Type2 excludes password;1/3 require it. Native code adds the negotiated direct
peer destination, fresh random Hop-by-Hop/End-to-End IDs and UUID Session-Id.
Password action logging is redacted; original values remain unchanged on wire.

`diameter_aa_result` contains a credential-free request and typed reply fields:
`result_code`, `accepted`, `stateless`, `reply_messages`, `filter_ids`,
`service_type`, `session_timeout`. `accepted` is true only for2001 after strict
command/application/flags/IDs/session/type/origin and mandatory-AVP checks, and
explicit agreed Auth-Session-State=1. Missing/stateful success is refused.
Session-Id must occupy the fixed first AVP position; a success carrying Failed-AVP
is refused. Malformed or rejected CEA leaves no registered client/command handle
and emits no connected event.
Protocol-error answers use RFC6733 E-bit grammar and remain nonaccepting;
permanent/transient NASREQ answers use its mandatory fields. Multi-round1001 and other non2001 results do not accept or prompt/retry. Returned NAS
attributes are reported, never applied to a device or saved as a session store.

Connected/result/error events use common handlers, memory and access recording.
Unconfigured events with empty instruction simply record without model calls.
One pending AAA,16command channel slots,32queued events/actions,followupdepth8.
Every constructed action is checked against16KiB/4096nodes/depth16 before copying;
all handler actions are validated before any queued wire action. Malformed batches
fail closed. Injection and native DWR/DWA/DPR/DPA stay independent of a parked
handler. The retained owned reader future protects partially read frames.

`disconnect` cancels pending AAA/handlers, emits a bounded DPR and waits for DPA
before reporting Disconnected. AppState removal closes immediately and aborts
the registered task. A caller command timeout alone does not cancel its wire
operation. Deadline/framing/correlation failure closes without replay, replies
safely to pending command promises and emits a bounded `diameter_error` event.
No direct raw/header/AVP payload API and no arbitrary unsolicited peer messages.

Client task uses `spawn_client_task`; the connected address, local address,
transport counters, statuses and access records use AppState. Required unchanged
independent receiver roles and peer calibration are in the
[test guide](../../../tests/server/diameter/AGENTS.md). No fuzz/production capture.
