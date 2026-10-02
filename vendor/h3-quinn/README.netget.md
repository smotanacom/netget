# h3-quinn 0.0.10 local cancellation patch

Source: crates.io h3-quinn 0.0.10, https://github.com/hyperium/h3, revision recorded
in `.cargo_vcs_info.json`; original Cargo.toml and MIT LICENSE are retained.
No dependency version was upgraded.

Upstream RecvStream transfers its Quinn receive stream into ReusableBoxFuture
while a read is pending, then stop_sending and recv_id unwrap the empty Option.
NetGet deadlines/removal can call stop_sending in that state and panic.
The patch keeps the Quinn stream in place and polls a borrowed read_chunk future
with std::pin::pin!. Quinn documents read_chunk as cancellation-safe, so dropping
that Pending future loses no data and retains stream identity. No allocation is
needed per poll. Other adapter behavior is unchanged.

`tests/http3_cancellation_test.rs` performs a real authenticated QUIC handshake,
polls an adapter receive to Pending, cancels it, and checks its ID. Verified
before patch: upstream panics at src/lib.rs:394 with Option::unwrap on None.
Client timeout/removal and server partial-body timeout/removal tests also cover
this path through real HTTP/3 request streams. Reapply only this RecvStream change
when updating the adapter; rerun those tests before removing the patch.
