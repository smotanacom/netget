# PFCP client (an SMF)

The control-plane side of one association with one UPF (`remote_addr`), using the server's
codec (`src/server/pfcp/wire.rs`).

## Actions

- `pfcp_associate` adds Node ID and Recovery Time Stamp.
- `pfcp_heartbeat`.
- `pfcp_establish_session{ies}`: a new CP SEID in an F-SEID, plus Node ID.
- `pfcp_modify_session{cp_seid, ies}` and `pfcp_delete_session{cp_seid}`. These are
  addressed by the UPF's SEID, learned from the establishment response.
- `pfcp_release_association`.
- `pfcp_respond{sequence, cause, ies}`: answers a request the UPF sent.

## Behaviour

- Requests are retransmitted after T1 (3 s), up to N1 (3) times, and then reported as
  `pfcp_timeout`.
- Answers arrive as `pfcp_response`: the request it answers, the cause, `cp_seid`/`up_seid`
  for session messages, and the IEs.
- A heartbeat from the UPF is answered in Rust. Any other UPF request is `pfcp_request`, for
  `pfcp_respond`.
- At most 64 requests are outstanding at once.
- Chains are bounded at `MAX_FOLLOWUP_DEPTH` (8).
- Injected actions return the response's event data as their `Executed` detail.
