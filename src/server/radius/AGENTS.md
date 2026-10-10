# RADIUS Server (RFC 2865 / RFC 2866)

UDP AAA server. The model makes the authorization decision — Access-Accept, Access-Reject or
Access-Challenge — and picks the reply attributes. NetGet owns the socket, the codec and the
MD5 constructions.

Files: `packet.rs` (pure codec, no I/O), `actions.rs` (LLM vocabulary + executor),
`mod.rs` (socket loop + the fail-closed rule).

## The single most important property: it fails closed

`FailureMode` is `Answers`, declared with `.answers_on_failure()` in `metadata()`: every
undecided Access-Request gets a correctly signed Access-Reject. RADIUS is **not** a
deliberately-silent protocol, whatever older lists said — it has a denial on the wire, and uses it.

This protocol grants network access, so the OAuth2 post-mortem in the root `AGENTS.md`
applies with full force. There, "the LLM returned nothing" fell through to a hardcoded
access token, and a model's explicit denial was indistinguishable from its silence.

Here, in `RadiusServer::decide`:

| Situation | Wire result | Logged decision |
|---|---|---|
| Model returns `send_access_accept` | Access-Accept | `decision=model_accept` |
| Model returns `send_access_reject` | Access-Reject | `decision=model_reject` |
| Model returns `send_access_challenge` | Access-Challenge | `decision=model_challenge` |
| Model returns no usable action | **Access-Reject** (synthesised) | `decision=fail_closed_no_action` |
| Model's action fails to encode | **Access-Reject** (synthesised) | `decision=fail_closed_action_error` |
| LLM call errors / times out | **Access-Reject** (synthesised) | `decision=fail_closed_llm_error`, plus `category=overloaded`/`category=unavailable` from `WireFailure::classify` |
| Accounting-Request, no answer | **nothing** (NAS retransmits) | `decision=fail_closed_no_action` |

Three things make this structural rather than aspirational:

1. **There is no default reply anywhere in `actions.rs`.** `execute_action` produces only the
   packet a named action asked for. Nothing synthesises an Access-Accept, ever.
2. **Accept and reject are separate actions with separate code paths.** They cannot decay
   into one another.
3. **The synthesised reject is tagged differently.** `fail_closed_*` is never reported as
   `model_reject`, and the synthesised packet carries the Reply-Message
   `"Access denied: no authorization decision was produced"`, which the model has no way to
   produce accidentally. So the two denial paths differ in the log *and* on the wire.

A fourth rule lives in `spawn_with_llm_actions`: **without `shared_secret` the server refuses
to start** (returns `Err`, so `server_startup` sets `ServerStatus::Error`). A default secret
would be a fail-open of the same family — anyone who guessed it could both read decrypted
passwords and forge replies.

Accounting is the one place where silence, not denial, is the safe default: an
Accounting-Response is an acknowledgement that a record was stored, and sending one the model
did not authorise would tell the NAS something untrue. `packet::is_authorization_request`
draws the line.

## What the cryptography actually does

Claiming crypto that is not performed is a documented failure of this codebase's auth family.
The list below is exhaustive in both directions.

**Implemented and exercised by tests:**

- **Response Authenticator** — `MD5(Code | ID | Length | RequestAuth | Attributes | Secret)`,
  RFC 2865 §3. `packet::response_authenticator`. The `Length` is the *response's*, not the
  request's; get that wrong and every real client silently discards the reply, which looks
  exactly like the server being down.
- **User-Password unhiding** — RFC 2865 §5.2. `packet::decode_user_password`. Trailing NUL
  pad stripped. Ciphertext must be 16..=`MAX_USER_PASSWORD_LEN` bytes and a multiple of 16,
  else the packet's password is reported absent rather than guessed at. The inverse,
  `encode_user_password` — used by tests and by anything building an Access-Request; the
  server never encrypts a password — had **no** bound until September 2026 and returned
  `Vec<u8>`, so a 129-octet plaintext produced 144 octets of ciphertext this module's own
  decoder rejects. It now shares the constant and returns `Result`. Latent rather than
  reachable from the wire, but it is the same encode/decode asymmetry the codec property tests
  were written to find.
- **Accounting-Request Authenticator** — `MD5(Code | ID | Length | 16 zero octets |
  Attributes | Secret)`, RFC 2866 §3. Unlike an Access-Request's authenticator (a random
  nonce, unverifiable), this one is keyed, so the server **verifies** it and drops the packet
  on mismatch. Comparison is constant-time.

- **Message-Authenticator (attribute 80)**, the HMAC-MD5 of RFC 3579 §3.2
  (`packet::encode_signed_response`): the **first** attribute of every Access-Accept, -Reject
  and -Challenge, computed over the reply with the Request Authenticator in place before the
  Response Authenticator, which then covers it. A request that carries one has it verified
  (`packet::verify_request_message_authenticator`) and is silently discarded when it does not
  match; a request without one is still answered. Until October 2026 replies carried none, so
  a BlastRADIUS-patched (CVE-2024-3596) client — NetGet's own among them — refused every
  Access reply this server sent. The fail-closed test pins the signed packet byte for byte
  against a literal computed with Python's `hmac` and `hashlib`.

**Not implemented, and claimed nowhere:**

- **CHAP (§5.3), MS-CHAP, EAP (RFC 3579).** CHAP-Password, CHAP-Challenge and EAP-Message are
  decoded to hex and handed to the model as opaque. No challenge is validated, no EAP state
  machine exists. The event's `auth_method` says `"chap"` or `"eap"` precisely so the model
  knows it has *not* been given a verified identity.

## Events and actions

Three events, all emitted by `mod.rs`, all carrying `.with_actions(...)`:

| Event | Raised when | Actions offered |
|---|---|---|
| `radius_access_request` | code 1 received | accept / reject / challenge |
| `radius_accounting_request` | code 4 received **and its authenticator verified** | accounting response |
| `radius_status_server` | code 12 received (RFC 5997) | accept / reject |

Any other code — including replies (2/3/5/11) — is dropped with a WARN. A server must not
answer a response.

Actions: `send_access_accept`, `send_access_reject`, `send_access_challenge`,
`send_accounting_response`. Parameters are structured throughout; the only encoded fields are
`state` / `class` / vendor `value`, each paired with an explicit `*_encoding` of `"utf8"`
(default) or `"hex"`, and **each really decoded by the executor** (`decode_encoded_field`).
That is the `send_tcp_data` lesson from `d70bb5b5`: `"48656c6c6f"` is simultaneously valid
text and valid hex, so the sender must say which it meant.

`service_type` and `framed_protocol` accept a number *or* a well-known name (`"Framed-User"`,
`"PPP"`), because models produce the names far more reliably. Both are really translated;
an unknown name is an error naming the accepted set, not a silent zero.

Reply-Message longer than 253 octets is split across several attributes, which is what
RFC 2865 §5.18 prescribes.

Proxy-State (§5.33) attributes are copied into every reply automatically, in order —
including the synthesised fail-closed reject.

## Per-request protocol instances

Like NTP, the registry's `RadiusProtocol` is context-free and **cannot sign anything**;
`execute_action` on it returns an error naming the reason. `mod.rs` builds one
`RadiusProtocol::for_request(...)` per datagram carrying that request's identifier,
Authenticator, the shared secret and its Proxy-State. Overlapping requests therefore cannot
pick up each other's signing material.

## Ports and privilege

1812 (auth) and 1813 (accounting) are both above 1023, so `privilege_requirement` is `None`.
Declaring `PrivilegedPort(1812)` would be dead code — the `svn`/`PrivilegedPort(3690)`
mistake.

## Maturity: Beta (September 2026)

Promoted from `Experimental`. The rating rests on `tests/server/radius/real_client_test.rs`
driving **FreeRADIUS `radclient`** — a peer NetGet did not write and does not link — which
verifies our Response Authenticator itself and refuses the reply outright if the MD5 is
wrong. Both its tests run (neither is `#[ignore]`d) and both **fail rather than skip** when
`radclient` is absent. That last point is the promotion: the evidence was always there, but
behind a gate that printed `SKIPPED` and returned `Ok(())`, and the root `AGENTS.md` is
explicit that a skip-when-missing gate is a silent pass rather than evidence.

FreeRADIUS is therefore required wherever the `radius` feature's suite runs
(`brew install freeradius-server`, or `apt-get install -y freeradius-utils`). It is not in
CI's `CI_FEATURES`, so the CI gate neither compiles nor runs those tests — see
`tests/server/radius/AGENTS.md` for the per-job derivation.

What Beta does **not** claim: CHAP, MS-CHAP and EAP are unimplemented
(above), and the tests cover neither, deliberately.

## NetGet's RADIUS client shares `packet.rs`

`src/client/radius/` builds its packets with this module's header, TLV, User-Password and
authenticator functions, and `hmac_md5` now lives here for both. What only a client needs —
CHAP-Password, and signing requests and verifying replies — lives in
`src/client/radius/wire.rs`. Pointed at this server the client used to refuse every Access
reply for want of a Message-Authenticator; it no longer does, and `tests/client/radsec`
drives the client against this server's decision path over TLS to show it. The client's
independent evidence is still FreeRADIUS.

`RadiusServer::answer` is the decision path with the fail-closed rule, returning the bytes
to send; the UDP loop sends them, and RadSec (`src/server/radsec/`) writes them to its TLS
stream.

## Known limitations

- One `shared_secret` for the whole server; no per-NAS client table.
- No duplicate-request suppression. RADIUS retransmissions reuse the identifier and
  authenticator; each retransmission currently costs another LLM call.
- No proxying to an upstream RADIUS server.
- IPv4 only in the attribute helpers (`NAS-IP-Address`, `Framed-IP-Address`); RFC 3162 IPv6
  attributes are passed through as hex in the `attributes` array but have no named field.
- Accounting has no persistence, deliberately — protocols must not implement storage. The
  model may use the generic SQLite facility if it wants to record sessions.
