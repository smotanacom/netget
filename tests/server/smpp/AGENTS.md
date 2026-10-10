# SMPP server tests

`SUBMIT_SCRIPT` accepts numbers starting 1555 with a DELIVRD receipt and a "Got: " reply and
rejects the rest with ESME_RINVDSTADR.

- `wire_test.rs`: raw PDUs — submit before bind, bind and rebind, an accepted submit followed
  by its receipt (text, receipted_message_id, message_state) and the reply, a 300-character
  UCS-2 text in and a 600-octet reply out through message_payload, a rejection with nothing
  after it, enquire_link, an unknown command, unbind; transmitter and receiver rules; wrong
  password and system_id. Then a handler-decided bind, command_length over the bound and
  under the header, an idle session, and no handler (ESME_RSYSERR, ESME_RBINDFAIL).
- `real_client_test.rs`: Python smpplib and linxGnu/gosmpp, each binding with credentials and
  reading three responses, the receipt and both replies.

Peers from `tests/client/smpp/install_peers.py`. No LLM calls.
