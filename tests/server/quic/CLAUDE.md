# Raw QUIC server tests

`e2e_test.rs`: existing text/binary echo, custom response, simultaneous stream,
keyword and encoding regressions, using a Quinn client and mock model.
`llm_failure_test.rs`: backend failure resets with application-local 0x0102.
These peers now negotiate `netget-quic`; `h3` is reserved for HTTP/3.

`independent_peer_test.rs`: an independent aioquic 1.3.0 client authenticates the
certificate/hostname, opens concurrent streams, exchanges binary and text bytes,
and verifies endpoint cleanup. It requires the pinned external Python dependency.
See `tests/client/quic/CLAUDE.md` for its small isolated setup and combined command.
No real model or public network is used by the tests. Server maturity remains
Beta based on the existing evidence; the new client remains Experimental.

Verified during the expansion: combined `--features quic --test server --test
client -- quic:: --test-threads=4` passed 10 server and 4 client tests, zero ignored.
The server suite includes a 32-stream credit/cancellation/active-removal test and
an explicit close_this_stream-on-open regression with client FIN withheld.

One earlier four-worker run timed out in the existing mocked three-stream test
at its unchanged five-second read deadline. It passed isolated, alongside all
interfering mocked tests at four workers, and in the final complete four-worker
run. The initial trace stopped after connection notification; its cause was not
established, so these passes do not establish that the transient cannot recur.
