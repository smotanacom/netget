# WS-Discovery client

Probes for WS-Discovery target services (ONVIF cameras, WSD printers, Windows/wsdd hosts) and
resolves their addresses. It uses the server's codec (`src/server/wsdiscovery/wire.rs`); see
that AGENTS.md for versions and the Types prefix rule wsdd depends on.

## Shape

`connect` binds an ephemeral UDP socket. Probes and resolves go to `remote_addr` (a directed
probe), or to the group `239.255.255.250:3702` when `remote_addr` is empty.

With `listen_announcements` the client also binds 3702 (SO_REUSEADDR), joins the group and
reports Hello/Bye as `wsd_announcement`.

Three tasks: socket readers, the session (sends and collects), and the dispatcher (model turns
via `call_llm_for_client`).

- `wsd_probe{types, scopes, match_by, wait_ms}` and `wsd_resolve{endpoint_reference, wait_ms}`
  send one message with a fresh MessageID.
- Every ProbeMatches/ResolveMatches whose RelatesTo names it is collected until `wait_ms`
  (default 2000, 100–30000) ends. Duplicates are merged by endpoint, and the newest wins.
- The collection is then reported **once** as `wsd_probe_matches` / `wsd_resolve_matches`,
  with `matches`, `count`, `responders` and what was asked. An empty result is a result:
  WS-Discovery has no negative reply.
- Answers that relate to no probe of ours are dropped.

The model's answer to each event runs through the same session, so probe → resolve chains work.
They are bounded at `MAX_FOLLOWUP_DEPTH` (8). At most 16 probes/resolves collect at once
(`MAX_PENDING`); the next is refused.

Injected actions (`send_to_client`) are answered with the collected result as the `Executed`
detail when the wait ends; an invalid one is `Rejected` before anything is sent.

## Limits

- A probe is sent once, with no retransmission. On a lossy network a missed answer is a
  missing match.
- IPv4 group only.
- No discovery-proxy mode.
