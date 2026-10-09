# SMPP client (SMPP 3.4 ESME)

Uses the server's `wire.rs`. `connect` binds as `bind` (transceiver by default) with
`system_id`, `password` and `system_type`; a refused bind fails the connection with the
SMSC's status name. A reader task feeds PDUs to the session, which answers enquire_link,
unbind and deliver_sm itself (deliver_sm_resp, or ESME_RINVMSGLEN for one it cannot parse).

- `smpp_submit {source_addr, destination_addr, text, registered_delivery?}`: ASCII as the
  default alphabet, anything else as UCS-2, message_payload past 254 octets. Submits may be
  pipelined (at most 64 awaiting); each `smpp_submit_result` is matched by sequence number.
- `smpp_enquire_link` → `smpp_link_ok`.
- deliver_sm → `smpp_deliver`, with Appendix B receipts parsed into `receipt`
  (`id`, `stat`, `err`, `submit_date`, `done_date`, …).
- `disconnect` sends unbind.
