# Raw QUIC server tests

`e2e_test.rs`: existing text/binary echo, custom response, simultaneous stream,
keyword and encoding regressions, using a Quinn client and mock model.
`llm_failure_test.rs`: backend failure resets with application-local 0x0102.
These peers now negotiate `netget-quic`; `h3` is reserved for HTTP/3.

`independent_peer_test.rs`: an independent aioquic 1.3.0 client authenticates the
certificate/hostname, opens concurrent streams, exchanges binary and text bytes,
and verifies endpoint cleanup. It requires the pinned external Python dependency.
See `tests/client/quic/AGENTS.md` for its small isolated setup and combined command.
No real model or public network is used by the tests. Server maturity remains
Beta based on the existing evidence; the new client remains Experimental.

Verified during the expansion: combined `--features quic --test server --test
client -- quic:: --test-threads=4` passed 10 server and 4 client tests, zero ignored.
The server suite includes a 32-stream credit/cancellation/active-removal test and
an explicit close_this_stream-on-open regression with client FIN withheld.

One earlier four-worker run timed out in the existing mocked three-stream test
at its unchanged five-second read deadline. It passed isolated, alongside all
interfering mocked tests at four workers, and in the final complete four-worker
run. A later 32-worker native run exposed the cause: the accept branch awaited
AppState to allocate a stream ID behind an older stream's queued write, while
that branch stopped polling the older stream. The fair lock could never progress.
ID allocation now runs inside the stream future, so the collection keeps polling
all state waiters.

`state_contention_test.rs` deterministically holds AppState when the first real
stream queues its model feedback write, accepts another stream, then releases
state. Both authenticated Quinn streams must receive their replies within the
existing five-second deadline. The rendezvous uses a thread-local trace subscriber
and bounded channels, so it does not depend on sleeps or interfere with other tests.

## Certificate parameter regression

`certificate_validation_test.rs` verifies that numeric, null and object SAN
entries are rejected before binding. It uses no peer or model. This audit
regression runs alongside the existing stream and independent-peer suites.
