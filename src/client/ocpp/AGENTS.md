# OCPP charge point — Experimental, OCPP-J 1.6 and 2.0.1

Connects to `ws://<remote_addr><path_prefix><charge_point_id>` requesting `ocpp1.6` (default)
or `ocpp2.0.1` and refuses a server that agrees any other subprotocol. `ocpp_connected` lets
the handler boot. `ocpp_call` sends a CALL (one outstanding; `cp-<n>` ids; core request fields
checked); the answer raises `ocpp_call_response`, a 30 s silence ends the session.
Central-system CALLs raise `ocpp_csms_call` (missing core fields answered by Rust with the
occurrence error; a second concurrent CALL with GenericError); the handler answers with
`ocpp_call_result` (core response fields checked) or `ocpp_call_error`. Shares
`src/server/ocpp/frame.rs`. No meter, connector or transaction simulation in Rust: every
payload comes from the handler.
