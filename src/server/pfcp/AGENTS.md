# PFCP server (a UPF)

PFCP (3GPP TS 29.244), the 5G N4 / EPC Sx control interface, on UDP 8805. NetGet is the
user plane function's control side. It forwards no user traffic: there is no GTP-U.

## The codec (`wire.rs`, shared with the client)

- The header, with and without a SEID, and a 3-byte sequence number.
- IEs as JSON keyed by IE name:
  - A grouped IE is a nested object of the same shape.
  - A repeated IE is an array. The exception is `apply_action`, whose value is itself a list
    of flag names, so a list of lists is the repetition.
- Readable forms for the IEs a model needs:
  - Node ID, F-SEID, UE IP and Outer Header Creation as addresses.
  - F-TEID as `{teid, ipv4}`, or `{choose: true}` for CH.
  - Apply Action as flag names, and interfaces as names.
  - Recovery Time Stamp as unix seconds, causes as names.
  - Network Instance as text, with DNS labels joined.
- Anything else is `ie_<type>: {"hex": …}` in both directions, so nothing is lost.
- `is_request` is an explicit list (1, 3, 5, 7, 9, 12, 14, 16, 50, 52, 54, 56). Odd/even does
  not work: session requests are even.

## Who answers what

In Rust, with no model:

- Heartbeats, answered with this node's recovery time.
- A Version Not Supported response for another protocol version.
- A request from a peer with no association: `no_established_pfcp_association`.
- A session request naming an unknown SEID: `session_context_not_found`, header SEID 0.
- A missing Node ID or F-SEID: `mandatory_ie_missing`, with the offending IE.
- Establishment past `MAX_SESSIONS`: `no_resources_available`.
- Retransmissions: responses are cached by (peer, sequence), the last 1024. A retransmitted
  request gets the identical bytes, and a request still in flight is not answered twice.

The model answers through events, each with one action `pfcp_respond{cause, ies}`:

- `pfcp_association_setup`
- `pfcp_session_establishment` (carries the UP SEID NetGet allocated)
- `pfcp_session_request` (modification and deletion)
- `pfcp_request` (everything else)

Rust adds the mandatory IEs: Node ID, Cause, Recovery Time Stamp, and the UP F-SEID on an
accepted establishment. It drops the model's copies of those. It also keeps the association
and session tables: a deletion or release that is accepted removes what it names.

**Answers on failure.** Every request gets a response. A silent model gets
`request_rejected`. A backend failure gets `system_failure`, or `pfcp_entity_in_congestion`
when the backend is overloaded. The log tags each one.

## Bounds

- 65 507-byte datagrams.
- Grouped IEs nest at most 8 deep. A deeper message is dropped unanswered, and the encoder
  refuses that depth too.
- At most 1024 IEs per message.

## Not implemented

- GTP-U forwarding.
- UPF-initiated Session Report Requests.
- Load and overload control.
