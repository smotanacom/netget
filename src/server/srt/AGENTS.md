# SRT listener — Experimental

srt-tokio 0.4.4 (pure Rust: the induction/conclusion handshake, ARQ with NAK and
retransmission, TSBPD, too-late drop, optional AES-128 with `passphrase`) carries the
connections; NetGet binds the UDP socket itself because srt-tokio does not report its address.
`streamid.rs` parses `#!::r=…,m=publish|request,u=…` and MediaMTX's `publish:path` / `read:path`
(default mode request; bidirectional is refused).

Rust owns:
- Refusals in the handshake: an empty resource or bad stream ID (2400 bad request), a second
  publisher of a resource (2409 conflict, checked before and after admission), more than 64
  readers (2402 overload), no handler answer (2500).
- The relay: each publisher's messages go to every reader of the same resource unchanged; a
  reader whose 4096-message queue fills is dropped. Readers may connect before the publisher.
- A publisher that sends nothing for `idle_timeout_secs` is closed (a fixed deadline from the
  last data; the statistics stream wakes the loop every second and must not reset it).
- srt-tokio announces its payload size in the handshake MSS field, so the payload size is raised
  to 1456 (`announce_mss`): with the default 1316 a libsrt peer limits itself to 1272-byte
  messages and cannot send the standard 1316.

The handler answers `srt_connect` (stream ID, resource, mode, user, remote) with `srt_accept` or
`srt_reject` (unauthorized, forbidden, not_found, bad_request, conflict, overload) and is told
`srt_closed` with the connection's last SRT statistics. A reader's peer handle accepts
`srt_send_text` and `disconnect`.

Not implemented: rendezvous, bidirectional mode, FEC, bonding, file (non-live) mode. MediaMTX's
gosrt refuses srt-tokio's SRT 1.3 handshake in both directions, so MediaMTX cannot exchange SRT
with NetGet.
