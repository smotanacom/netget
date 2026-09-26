# WireGuard Tests

## What these tests validate (and the hard limit they work around)

NetGet's WireGuard server is a **thin orchestration layer over `defguard_wireguard_rs`**. NetGet implements *none* of
the WireGuard protocol itself - no Noise_IK handshake, no ChaCha20-Poly1305, no packet parsing. That all lives in the
platform backend defguard drives: the kernel module on Linux/FreeBSD/Windows, and the **external `wireguard-go` binary
on macOS**. Creating the interface therefore needs **root** (and, on macOS, wireguard-go installed and in PATH).

Two consequences shape this suite:

1. **There is no NetGet-authored handshake/crypto code to unit-test** with real curve25519 keys. That layer does not
   exist in this repo. So the strongest CI-able evidence anyone hoped for - "drive the server's own handshake" - is
   not applicable here.
2. **A real end-to-end handshake needs a running server, which needs root + a backend.** It has never been run and
   cannot run in CI. It exists here only as a root-gated `#[ignore]`d harness that **fails loudly** (never
   skips-as-pass).

So the suite validates the code NetGet *actually owns* and can run unprivileged: the action executors that implement
the LLM's control surface, and the event/action declarations that decide what the model may answer with.

### Why `boringtun` was evaluated and NOT added

`boringtun` (Cloudflare, BSD-3-Clause) is a genuine in-process WireGuard peer and would be an excellent handshake
driver. But it needs something to handshake *against*. On this host the NetGet server cannot start (no root, and
`wireguard-go` is not installed), so a boringtun initiator has no responder. A boringtun-against-boringtun test would
validate Cloudflare's library, not NetGet - so it would be theater. It was deliberately left out. If the design bug
below is fixed and a privileged environment is available, boringtun is the recommended driver for a real interop test.

### The design defect this work surfaced (now addressed)

WireGuard responders **drop a handshake whose static public key is not already a configured peer**. NetGet only learns
of a peer by polling `read_interface_data()` *after* it appears - and an unconfigured peer never appears. With no
user-triggered action to pre-add a peer, `wireguard_peer_connected` could effectively never fire for a genuinely new
peer, and the LLM authorize/reject flow was unreachable in practice.

Fixed by the **`wireguard_add_peer`** action, which pre-authorizes a peer's public key + allowed IPs on the interface
before it connects (reaching the live `WireguardServer` via `execute_action_with_state` + a registered handle). The
config-mutation path (`build_peer_config`) and the executor wiring are unit-tested below without a backend. What still
needs root + a WireGuard backend, and so stays `Beta`: a real client actually completing a handshake against a
pre-added key, `wireguard_peer_connected` firing for it, and a transport-packet exchange. `boringtun` is the
recommended in-process driver for that once a privileged environment exists. Documented in
`src/server/wireguard/CLAUDE.md` and the protocol's `metadata()` notes.

## Test inventory

All tests except the last are pure and unprivileged; they construct `WireguardProtocol::new()` directly and need no
mock Ollama, no network, and no root.

### Action executors (the LLM control surface)

- `authorize_peer_returns_structured_authorization` / `authorize_peer_endpoint_is_optional` - valid input yields an
  `Output` JSON blob carrying `action`/`public_key`/`allowed_ips`/`endpoint`/`message` for the server to apply;
  endpoint and message are optional with a sane default.
- `authorize_peer_rejects_missing_public_key` / `_rejects_empty_allowed_ips` / `_rejects_missing_allowed_ips` -
  fail-closed on bad input rather than pushing a useless peer to the interface. The error names the offending field.
- `disconnect_peer_returns_structured_disconnect` / `disconnect_peer_defaults_reason_and_requires_public_key` -
  structured disconnect output, default reason, mandatory public_key.
- `reject_peer_is_a_noop_result` / `set_peer_traffic_limit_is_unenforced_noop` - both resolve to `NoAction`. The
  traffic-limit case pins that the **unenforced** limit never pretends to have applied anything.
- `unknown_action_is_rejected` / `missing_type_is_rejected` - dispatch errors on garbage input.

### wireguard_add_peer (the pre-authorization action)

The config-mutation logic is the pure `build_peer_config`, split out so it can be tested with no root and no backend -
it is exactly what `add_peer` feeds to `configure_peer`.

- `build_peer_config_accepts_valid_key_and_ips` - a valid key + CIDRs + endpoint build a `WGPeer` carrying precisely
  those values (i.e. `add_peer` would authorize exactly that key).
- `build_peer_config_rejects_malformed_key` - fail-closed on a bad public key rather than configuring a garbage peer.
- `build_peer_config_rejects_empty_and_malformed_allowed_ips` - empty list and malformed CIDR both error; a bad IP
  fails the whole request rather than being silently dropped.
- `add_peer_action_is_declared_and_documented` - the action is advertised with `public_key` + `allowed_ips` required.
- `add_peer_via_stateless_execute_action_errors_without_server` - on the stateless path (no server context) it errors,
  never silently no-ops.
- `add_peer_via_state_fails_closed_without_server_id` / `_rejects_empty_allowed_ips_before_touching_server` -
  `execute_action_with_state` fail-closes when there is no server to reach and validates input before any lookup.
- `execute_action_with_state_delegates_non_add_peer_actions` - non-add_peer actions are forwarded verbatim to the
  stateless executor (disconnect_peer still yields structured `Output`).

### Declaration integrity

`call_llm` builds the model's tool list from `EventType.actions`, **not** from `get_sync_actions()`, so these guard
what the model is actually offered:

- `peer_connected_event_advertises_the_interface_changing_actions` - the `wireguard_peer_connected` event offers
  exactly `authorize_peer` / `reject_peer` / `disconnect_peer`, and does **not** advertise the unenforced
  `set_peer_traffic_limit` (which would promise enforcement that never happens).
- `protocol_advertises_wireguard_add_peer_as_the_only_async_action` - `get_async_actions()` advertises exactly
  `wireguard_add_peer` (the pre-add action) and nothing else.
- `metadata_is_beta_not_stable` - the rating is `Beta`. This assertion is the tripwire: if a future change earns
  `Stable` via a real interop test, it must be updated together with the metadata and both CLAUDE.md files.

### Root-gated real-backend harness

- `test_wireguard_real_backend_startup` - `#[ignore]`d. Drives NetGet's real spawn path and requires the interface to
  actually come up ("Interface created successfully" in the server log); it **panics** if it does not, so it can never
  masquerade as a pass. Run it explicitly under privilege:

  ```bash
  sudo ./cargo-isolated.sh test --features wireguard --test server -- \
      wireguard::e2e_test::test_wireguard_real_backend_startup --ignored --test-threads=1
  ```

  Note: even with root, a real client will not currently drive `wireguard_peer_connected` (see the design bug above).

## Running

```bash
# Unprivileged tests (run in CI):
./cargo-isolated.sh test --no-default-features --features wireguard --test server -- \
    wireguard::e2e_test --test-threads=100
```

## History

The previous version of this file described a "honeypot" packet-detection suite that did not match the
implementation: its tests mocked a `wireguard_packet_received` event and a `log_packet` action that **do not exist**
(the server raises `wireguard_peer_connected` and has no packet-sniffing path), and every test was `#[ignore]`d behind
root, so the mismatch never surfaced. Replaced wholesale.
