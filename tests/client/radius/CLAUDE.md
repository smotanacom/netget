# RADIUS client tests

```bash
./cargo-isolated.sh test --no-default-features --features radius --test client -- radius --test-threads=100
```

| file | peer | LLM calls | proves |
|---|---|---|---|
| `real_server_test.rs` | FreeRADIUS `radiusd -X` | 8 | the client bar (below) |
| `transport_test.rs` | hand-written UDP servers built on the shared codec | 3-4 each | reply verification, source check, oversize, retransmission, in-flight cap |
| `request_test.rs` | none | 0 | HMAC-MD5 vectors, packet construction, refusals |

## real_server_test.rs — the evidence the rating rests on

FreeRADIUS 3 runs unprivileged from a raddb written into the `RealServer` temp dir: one client
(127.0.0.1, secret `testing123`, `require_message_authenticator = yes`), a `users` file (`alice`
/ `wonderland` with Reply-Message `Welcome, alice`; `mallory` always rejected with `Go away`),
the `pap`, `chap`, `files` and `detail` modules, and auth/acct listeners on two probed ports.
The dictionary and module directories are found at run time (Homebrew or a distribution layout).
Ready when it logs `Ready to process requests`. A missing `radiusd` **fails** the test; on
Ubuntu the binary is `freeradius`, which CI links to `radiusd`.

`radius_client_authenticates_and_accounts_against_freeradius` (8 LLM calls): PAP accepted with
`Welcome, alice`; CHAP accepted; a wrong password rejected; `mallory` rejected with `Go away`;
Accounting Start with a session id, NAS-Port 7 and Called-Station-Id; Status-Server answered
Access-Accept. FreeRADIUS verifies every authenticator NetGet computes and signs every reply
NetGet verifies. Then the `detail` file must hold the model's accounting attributes (condition
4 from the server's side). Every event is checked for the shared secret, and so is everything
NetGet printed.

`injected_radius_requests_reach_freeradius` drives the command channel in-process: an
Access-Request FreeRADIUS logs as `Login OK: [alice`, an Accounting Stop that reaches the detail
file, a reserved attribute refused, and `disconnect`.

## Verified by mutation

Each removed on its own, with its test failing: the loop over `result.actions`, both display
redactions (`real_server_test`); the Response Authenticator check, the Message-Authenticator
requirement, the Message-Authenticator check, the source check, the shared decoder's 4096-byte
refusal, the retransmission bound, `MAX_IN_FLIGHT` (`transport_test`); the reserved-attribute
refusal (`request_test`).
