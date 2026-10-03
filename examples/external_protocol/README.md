# External protocol example

`EchoProtocol` demonstrates NetGet's current public `Protocol` and `Server` traits in
an external Rust crate. Its TCP listener echoes bytes deterministically and makes no
model calls. Listener and peer tasks are registered with `AppState`, so removing the
server stops both. The example is Experimental; it makes no interoperability or
production-readiness claim.

Common protocol information belongs to `impl Protocol`: metadata uses
`ProtocolMetadataV2`, and descriptions, groups, actions and startup examples are
required. `impl Server` provides `spawn` and `execute_action`. `ActionDefinition`
includes `log_template`; the example uses `None`.

The illustrative `send_echo_data` encoder can be called directly. The listener itself
always echoes and does not dispatch model actions or configured event handlers. Add
NetGet's event dispatch machinery if extending this into an LLM-controlled protocol.

From the repository root, check the standalone crate with:

```sh
./cargo-isolated.sh check --manifest-path examples/external_protocol/Cargo.toml
```

The dependency disables NetGet's default protocol feature set because this example
needs only the public API and Tokio. The root CPU-only regression target includes
this exact source file, checks action encoding and performs a loopback echo/stop test:

```sh
./cargo-isolated.sh test --no-default-features --features tcp \
  --test test_infrastructure_review_test -- --test-threads=100
```

An embedding application that depends on both crates can register
`Arc::new(EchoProtocol::new())` in its protocol registry. NetGet does not dynamically
load this library or discover a plugin directory. Adding this crate back as a dependency
of NetGet itself would create a dependency cycle; use a separate embedding application
or place an in-tree protocol under `src/server` instead.

See `src/llm/actions/protocol_trait.rs` for the traits,
`src/protocol/server_registry.rs` for registration, and `src/server/tcp` for a complete
LLM-controlled implementation.
