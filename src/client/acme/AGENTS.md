# ACME client — Experimental

Reads the directory at `scheme://remote_addr` + `directory_path` (trusting `ca_file` in addition
to the system roots), generates an ES256 account key and, with `http01_listen`, serves http-01
key authorizations. Every request is signed with `src/server/acme/jws.rs`; a `badNonce` is
retried twice with the nonce it carried.

Actions: `acme_register`, `acme_order` (fetches each authorization and gives the dns-01 TXT name
and value), `acme_validate` (http-01 through the responder, or dns-01 once the record is
published; polls the authorization), `acme_finalize` (P-256 key and CSR from rcgen, polls the
order, downloads the chain; `key_file` writes the key with mode 0600 and refuses to overwrite),
`acme_revoke` (the last certificate), `acme_deactivate`, `disconnect`. Each answer is one
`acme_response` with the HTTP status and, on refusal, the CA's problem.
