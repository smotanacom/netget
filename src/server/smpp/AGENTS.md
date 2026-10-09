# SMPP server (SMPP 3.4 SMSC)

Hand-written over Tokio TCP (`wire.rs`, shared with the client). The `rusmpp` crate was the
alternative; NetGet needs the bind state machine, receipt generation and status mapping in
its own hands, and the 3.4 PDUs it serves are small.

## What Rust owns

PDU framing and every bound; the bind state (nothing but a bind before binding; a second
bind is ESME_RALYBND; a receiver may not submit; a transmitter gets no deliver_sm); sequence
numbers; message ids (`NG` + 8 hex digits unless the handler names one); enquire_link,
unbind, generic_nack for unknown commands; text decoding for data_coding 0, 1, 3 and 8
(short_message or message_payload); the Appendix B receipt text with receipted_message_id
and message_state TLVs; and replies encoded as ASCII or UCS-2, in message_payload past 254
octets.

Credentials: with `esme_system_id` and `password` set, Rust checks them (password compared in
constant time; ESME_RINVSYSID / ESME_RINVPASWD) and the handler never sees them; without
them every bind is an `smpp_bind` event, which does carry the password.

## What the handler decides

`smpp_bind` → `smpp_bind_accept` / `smpp_bind_reject {status}`; `smpp_submit` →
`smpp_accept {message_id?, receipt?, reply_text?}` / `smpp_reject {status}`. A receipt is
sent only when the ESME set registered_delivery; a reply is a mobile-originated deliver_sm
from the destination back to the sender.

## Failure modes and bounds

Nothing is accepted by default: a handler failure, silence or a wrong answer on a submit is
ESME_RSYSERR (ESME_RTHROTTLED when the backend is saturated), on a bind ESME_RBINDFAIL, each
with its `decision=` tag. command_length outside 16..=MAX_PDU closes the session before the
body is read; each session may be silent `idle_timeout_secs` (default 300) between PDUs.
