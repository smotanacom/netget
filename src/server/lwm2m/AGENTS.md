# LwM2M server — Experimental

LwM2M 1.1 over CoAP/UDP, built on the `coap` feature's codec (`lwm2m` enables `coap`).

- `exchange.rs`: CoAP exchanges over one socket in both directions. Confirmable requests are
  retransmitted (2 s, doubling, 4 retries); responses are matched by token, piggybacked or
  separate (an empty ACK stops retransmission and a separate response is awaited for 30 s, then
  ACKed if confirmable). Requests are handed out once: a retransmission while the first is being
  handled is dropped, and one after the answer gets the cached answer. Observations (RFC 7641)
  are registered before the request is sent, delivered by token, and answered with RST once
  forgotten. 16 KiB datagrams; no block-wise transfer.
- `content.rs`: SenML JSON (110: v, vs, vb, vd, vlo, with base names), plain text (0) for one
  resource (a number only when it reads back exactly, so "+02" stays text), opaque (42), and CoRE
  link format (40).

Rust owns the registration interface: `POST /rd` (endpoint, lifetime, version, binding, object
links; a missing endpoint or empty object list is 4.00) → `lwm2m_register`, answered by
`lwm2m_accept` (2.01 with Location-Path rd/<id>) or `lwm2m_reject` (4.03 or 4.00); `POST /rd/<id>`
updates (address and lifetime refreshed) → `lwm2m_update`; `DELETE /rd/<id>` → `lwm2m_deregister`
(deregistered); registrations expire 15 s after their lifetime (expired). A device registering
again replaces its earlier registration. 1024 registrations.

Operations — `lwm2m_read` (SenML JSON or text), `lwm2m_write` (text for one value, SenML JSON
replace or update for several), `lwm2m_execute`, `lwm2m_discover`, `lwm2m_observe`,
`lwm2m_cancel_observe`, `lwm2m_create`, `lwm2m_delete` — come from the handler's answers or a
registration's peer handle. Each result is `lwm2m_response` (code, values or links, or the error)
and each notification `lwm2m_notification`; chains stop at depth 4. No handler answer to a
registration is 5.03.

Not implemented: DTLS, OSCORE, bootstrap, queue mode, TLV/CBOR/LwM2M JSON, Send, composite
operations, block-wise transfer.
