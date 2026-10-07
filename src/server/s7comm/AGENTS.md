# S7comm selected legacy scope

Rust owns ISO-on-TCP TPKT, COTP CR/CC/data and S7 setup/Job/AckData framing.
Only S7ANY byte transport is offered, for DB, input, output and marker areas.
Server handlers receive `s7comm_request` with operation and ordered items
(area, DB, byte offset, count; writes include byte values). `s7comm_reply`
answers each read with values, each write with explicit `accepted: true`, or
an address/denied/unsupported error. Omitted approvals and missing handlers
fail closed. No PLC memory is stored: use handlers or generic server memory.
The scanner offers `s7comm_read` and `s7comm_write` with those structured fields,
automatically negotiates TSAP 0102 and reports values/status in
`s7comm_response`. Injected commands share its serial transaction path.

Boundaries: 480-byte S7 PDU (487 with TPKT/COTP), 16 server request items,
200 bytes per item, 256 TCP connections, 30-second initial frame and
600-second idle deadline, 10-second scanner exchange. Server stop owns every
connection task, client stop owns socket and handler tasks. Peer injection only
supports disconnect outside a pending request. No S7plus, authentication,
PLC commands, upload/download, timers/counters, bit or multi-byte S7ANY items.
Experimental: tests use independent python-snap7 3.2.1, no maturity promotion.
