# tests/client/tor

Tor **client** (arti). **There is no local end-to-end test of this client, and there cannot
easily be one.** `arti_client::TorClient::create_bootstrapped()` does not "connect to a relay":
it fetches and validates a consensus signed by the directory authorities, then the relay
descriptors it names, then builds a multi-hop circuit. NetGet's `tor_relay` implements enough
of the OR link protocol to be talked to; it cannot serve a signed consensus, which would need
the authorities' signing keys. So the one E2E test is `#[ignore]`d and never runs.

That is the same shape as `tor_relay` losing its Stable rating — a rating resting on never
having been driven by a real client. An `#[ignore]`d test is not evidence, which is why
`src/client/tor` is `DevelopmentState::Experimental`.

## Files

### `test.rs` — seven tests (6 `#[test]`, 1 `#[tokio::test]`), **0 LLM calls**

The bootstrap-refusal contract. `create_bootstrapped()` contacts the real directory
authorities **before it looks at the requested address** (~14s in an unguarded run), so merely
opening a Tor client made outbound connections to third parties whatever the user asked for —
in a tool that binds loopback everywhere else. `bootstrap_target()` is the guard and these
tests pin it:

`no_bootstrap_choice_is_refused`, `a_named_directory_server_is_an_explicit_choice`,
`the_public_network_is_reachable_on_explicit_opt_in`,
`a_directory_server_plus_the_public_opt_in_is_refused`, `the_opt_in_parameter_is_declared`
(`ALLOW_PUBLIC_TOR_NETWORK_PARAM`), `the_metadata_notes_say_it_does_not_reach_the_internet_by_default`,
and `connect_without_an_opt_in_is_refused_before_any_network_io` — which measures elapsed time
to prove the refusal happens before any I/O.

Nothing here touches the network. That is the point.

### `apply_actions_test.rs` — three `#[tokio::test]`, **0 LLM calls**

The shared action executor. `tor_connected` and `tor_bootstrap_complete` both asked the model
what to do and threw the answer away (`Ok(_) => trace!("LLM called successfully")`, and an
`if let Err(..)` whose success arm did not exist). Both now run through `apply_actions`, which
the read loop also uses, so the vocabulary is executed in exactly one place.

- `a_disconnect_action_tells_the_caller_to_stop` — returns `true`, which is how the read loop
  learns to stop. Before the fix `disconnect` only `break`ed the action loop, so the model
  could never close a Tor client at all.
- `send_without_a_circuit_is_refused_out_loud_and_does_not_stop_the_answer` —
  `tor_bootstrap_complete` fires while `connect()` is still bootstrapping.
- `an_action_the_protocol_rejects_is_reported`, not swallowed.

### `command_channel_test.rs` — two tests (1 `#[tokio::test]`, 1 `#[test]`), **0 LLM calls**

The reachable halves of the `[ send ]` contract without a circuit:
`send_to_client_without_a_handle_is_refused` (the negative path the dashboard uses to grey out
`[ send ]`) and `command_channel_vocabulary_is_encodable_by_the_generic_arm`
(`send_tor_data` → `SendData`, `disconnect` → `Disconnect`).

### `e2e_test.rs` — one test, **`#[ignore]`d, 2 LLM calls if it ever runs**

`test_tor_client_with_local_relay`, ignored with
`"arti bootstraps a full Tor consensus, which tor_relay cannot serve"`. Two `.expect_calls(1)`
mocks (relay startup, `tor_relay_circuit_created`). Unignored it times out after 120s and cost
the suite two minutes per run.

The TLS fix underneath it is real and worth keeping: the relay accepted only TLS 1.3, so every
1.2 ClientHello was rejected before the test got as far as bootstrapping. Making this pass
means serving a real consensus (out of scope) or testing the link handshake directly instead of
through arti's bootstrap.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features tor \
    --test client -- client::tor --test-threads=100
```

There is no `tor_directory` protocol, feature or test directory — the client feature is `tor`
and the relay is `tor_relay`.
