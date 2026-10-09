# EPP client — Experimental

A registrar: TLS (a single trusted certificate from `ca_cert_path`, otherwise the webpki
roots; `server_name` for the name checked) or plain TCP with `tls: false`, RFC 5734 framing, the
greeting, and a login right after it when `client_id` and `password` are configured (a refused
login fails the connection). `epp_connected` reports the greeting and the login.

Actions render RFC 5730–5733 commands: `epp_check`, `epp_info`, `epp_create_domain`,
`epp_create_host`, `epp_create_contact`, `epp_renew`, `epp_transfer`, `epp_delete`,
`epp_hello`, `epp_login`, `epp_logout` (which ends the session). Each answer is `epp_response`
with the code, message, reason, transaction ids and the resData read by the server's parser:
`results` for a check, the object's fields (status, contacts, addrs and ns as lists) for info,
names and dates for create, renew and transfer. Handler-driven commands chain to depth 16.
