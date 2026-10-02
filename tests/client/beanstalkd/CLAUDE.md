# Beanstalkd client tests

Run with the existing feature:

```sh
cargo test --no-default-features --features beanstalkd --test client beanstalkd:: -- --test-threads=100
```

Use the programme's run_cargo.py wrapper during protocol expansion to serialize builds,
share artifacts and preserve free disk space. Tests bind ephemeral loopback sockets.

`real_server_test.rs` launches the independent installed beanstalkd (verified here: 1.13)
through RealServer, with persistence disabled and a private temporary directory. Missing
beanstalkd fails, never skips: install via `brew install beanstalkd` or the Debian/Ubuntu
`beanstalkd` package. The test drives tube selection/watch/ignore, exact UTF-8 job round
trip, reserve/touch/release/peek/reserve-job/bury/kick/delete, stats, tube lists/pause and
empty-queue/missing-job errors. RealServer owns and reaps the process group on exit.

`session_test.rs` also connects the new client to NetGet's existing Beanstalkd server using
static event handlers, verifies framing and tube state, interrupts a stalled reservation
through injected disconnect, and delivers a fragmented final response even after EOF.
`wire_test.rs` checks numeric/size/injection validation, response correlation, byte counts,
CRLF, EOF and non-UTF8 rejection, as well as flat YAML shape, aliases and entry bounds.

All handlers in these tests are deterministic. The LLM endpoint is deliberately unreachable;
no test needs a live model or silent skip. No pcap/fuzz claim is made.
