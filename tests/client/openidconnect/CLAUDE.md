# openidconnect client tests

Two tests, both against a provider served **in-process**, neither `#[ignore]`d, neither
needing Ollama or a network.

```bash
./cargo-isolated.sh test --no-default-features --features openidconnect \
    --test client -- openidconnect --test-threads=100
```

`--test` names a *target* (`client`); the module path after `--` is a filter.

| File | Test | Covers |
|---|---|---|
| `command_channel_test.rs` | `injected_oidc_exchange_reaches_the_provider_and_never_invents_a_token` | The dashboard's inject path: discovery document + JWKS + token endpoint served by a hand-rolled HTTP/1.1 listener on an ephemeral port; a real `client_credentials` exchange goes on the wire; only the provider's own token is stored; an unknown action is `Rejected`; `disconnect` ends the command loop |
| `e2e_test.rs` | `oidc_client_with_an_unreachable_provider_invents_nothing` | The same client pointed at a closed port: every flow fails, and `access_token` / `id_token` / `refresh_token` are all absent afterwards |

Zero LLM calls in both: the client's LLM points at `http://127.0.0.1:1`, so its connect-time
calls fail and the loop tolerates it. That is deliberate — these tests are about what reaches
the *provider*, not about prompting.

## What this file used to describe, and why it is gone

This document previously recommended "Option 3: Public Test Providers (Current)" and listed
`https://accounts.google.com`. `e2e_test.rs` held five tests built that way, every one
`#[ignore]`d with the reason in the attribute: no `.with_mock()`, hits real
`accounts.google.com`, needs `--use-ollama`.

So the suite was in the worst of both states — it proved nothing on any runner, and the only
way to make it run was to send live traffic to Google with whatever ambient configuration the
machine had. That is the client-side shape of the defect the root `CLAUDE.md` records for the
DynamoDB client: *a client that loses its target must fail, never fall back to the real
service*. Here it was the test rather than the code pointing at production, which is not
better — a test is the thing people run without reading.

The five are deleted. Everything they claimed to cover is covered for real by
`command_channel_test.rs`, which drives discovery *and* a token exchange against a provider
the test itself serves.

**Do not reintroduce a test that names a public provider.** The in-process provider in
`command_channel_test.rs::start_provider` is ~50 lines of `tokio::net::TcpListener` and needs
no extra feature; copy it.

## What is still not covered

- **ID token verification, because there is none to test.** The `openidconnect` crate can
  check a JWT's signature, issuer, audience and nonce through `id_token.claims(..)`; this
  client never calls it and stores the token as an opaque string. A test asserting a forged
  `id_token` is rejected would fail, correctly — write it when the verification is written,
  not before. `src/client/openidconnect/actions.rs`'s `metadata().notes` states the gap.
- **Authorization-code and device flows.** Both spawn their own tasks and a callback
  listener; the injected path covers `client_credentials` only.
- **Token refresh against a provider that rotates refresh tokens.**

## Secrets in events

`oidc_token_received` reports `access_token` / `id_token` / `refresh_token` as `"[REDACTED]"`
(or `""` when absent), matching the sibling `oauth2` client. The values live in
`protocol_data` and every action that needs one reads it back, so no test should assert on a
token value *in an event* — assert on `protocol_data`, as `command_channel_test.rs` does.
