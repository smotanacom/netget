# LwM2M client — Experimental

A device over `src/server/lwm2m/exchange.rs` and `content.rs`. Registers `objects` (instance
paths) with `endpoint` and `lifetime` (`</>;rt="oma.lwm2m";ct=110` first), refreshes the
registration at half the lifetime (registering again on 4.04) and reports `lwm2m_registered`.
Requests for objects it did not register are 4.04 from Rust; discovery is answered from the
registered list. Reads (and observe starts) are `lwm2m_read_request`, answered with
`lwm2m_content` values (text when asked or for one resource without Accept, else SenML JSON);
writes, executes, creates and deletes are their own events answered by `lwm2m_ok` or
`lwm2m_error`; no answer is 5.03. `lwm2m_notify` sends a notification for an observed path (or a
resource beneath it) with the next sequence number; `lwm2m_update` refreshes now; `disconnect`
deregisters. No DTLS, OSCORE or bootstrap.
