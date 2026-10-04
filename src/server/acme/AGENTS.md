# ACME server (test CA) — Experimental

RFC 8555 over hyper HTTP/1.1, HTTPS when `tls_cert_file` and `tls_key_file` are given (lego
refuses a plain-HTTP directory; certbot accepts one). `ca.rs` generates a P-256 root at startup
(`ca_name`) and issues leaf certificates with rcgen; `jws.rs` parses flattened JWS, verifies
ES256, ES384, RS256 and EdDSA with ring and computes RFC 7638 thumbprints (the client signs
with the same module).

Rust owns:
- `/directory`, `/new-nonce` (HEAD 200, GET 204), `/roots/0` (the CA, as Pebble serves it), and
  every POST: `application/jose+json` (else 415), a 64 KiB body (413), single-use nonces
  (`badNonce` with a fresh one), the signed `url` equal to the request URL (`unauthorized`), the
  algorithm (`badSignatureAlgorithm` listing the supported ones), the signature, `jwk` for
  newAccount and `kid` for everything else (`accountDoesNotExist`), and ownership of every
  order, authorization, challenge and certificate.
- Accounts keyed by thumbprint (the same key returns the existing account, `onlyReturnExisting`),
  mailto contacts only (`unsupportedContact` / `invalidContact`), terms of service when
  `terms_of_service` is set, contact update and deactivation.
- Orders: dns identifiers only, LDH names lower-cased, wildcards only with dns-01, no
  `notBefore`/`notAfter`; one authorization per name with `challenge_types` (default http-01
  and dns-01). Status rolls up from authorizations.
- http-01: with `http01_target`, Rust GETs the token there with the identifier as Host and
  compares the key authorization; a mismatch or failure marks the challenge invalid
  (`incorrectResponse`) without asking the handler. dns-01 is never checked by Rust.
- Finalize: the order must be ready (`orderNotReady`); the CSR's signature must verify and its
  DNS names (and CN) must equal the order's (`badCSR`). Issuance is synchronous.
- Revocation by the owning account (not by certificate key), reasons 0–10 except 7,
  `alreadyRevoked`. No OCSP or CRL.

The handler answers `acme_new_account`, `acme_new_order`, `acme_validate` (with `verified`
true or null), `acme_finalize` and `acme_revoke` with `acme_accept` or `acme_reject` (an RFC 8555
error type and a detail). No answer is a 500 `serverInternal` with a category message, and
nothing is created.

State (accounts, orders, authorizations, nonces, certificates) is in memory, bounded, and lost on
stop. Not implemented: keyChange, external account binding, pre-authorization, tls-alpn-01,
profiles, ARI. A test CA only.
