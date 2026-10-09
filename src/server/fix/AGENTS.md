# FIX acceptor — Experimental

`codec.rs` frames FIX tag=value: BeginString first, BodyLength second (at most 256 KiB),
MsgType third, CheckSum last; data fields (RawDataLength/RawData and the other FIX 4.4 pairs) may
carry SOH; repeated tags keep their order. A message with a wrong BodyLength or CheckSum is
garbled and ignored, as FIX requires. `dict.rs` is generated from QuickFIX/Go's FIX44.xml (953
fields, 92 message types). `session.rs` is the session layer the client shares.

Rust owns:
- The Logon: the first message must be a Logon in `begin_string` within `logon_timeout_secs`
  (else the connection is dropped); TargetCompID must be `sender_comp_id`, HeartBtInt 0–3600,
  EncryptMethod 0 — each refused with a Logout naming the reason before the handler is asked.
- Sequence numbers both ways: a gap sends one ResendRequest (from the expected number to
  infinity) and holds nothing — out-of-order messages are discarded until resent; too low
  without PossDupFlag is a Logout; SequenceReset in reset and gap-fill modes.
- A ResendRequest from the peer: stored application messages (last 2048) come back with their
  numbers, PossDupFlag and OrigSendingTime; everything else is one SequenceReset-GapFill per run.
- TestRequest → Heartbeat with its TestReqID; a Heartbeat after HeartBtInt idle; a TestRequest
  after HeartBtInt + 20% of silence; a Logout if that goes unanswered for HeartBtInt.
- Session Reject for CompID problems (and a Logout), a second Logon, a missing SendingTime, a
  bad NewSeqNo.

The handler answers `fix_logon` (`fix_accept_logon` / `fix_reject_logon`) and each in-sequence
application message `fix_message` (fields as `{tag, name, value}`) with any number of `fix_send`
(header, BodyLength and CheckSum are Rust's; session MsgTypes are refused), `fix_reject`
(BusinessMessageReject naming the message), `fix_ignore` or `fix_logout`. No logon decision is a
Logout; no answer to a message is a BusinessMessageReject, reason 4 (application not available).
The connection's peer handle accepts `fix_send`, `fix_logout` and `disconnect`, numbered in the
session.

While the handler is deciding, the session does not read or send heartbeats; with a short
HeartBtInt a slow model can cost the session. One session per connection, in memory; no FIXT.1.1 /
FIX 5.0, no persistence, no dictionary validation of application messages.
