# DICT client tests

Run `cargo test --no-default-features --features dict --test client dict:: -- --test-threads=100`.
During the protocol expansion programme, use its shared `run_cargo.py` wrapper for every Cargo command.

`real_server_test` requires the independent C dictd server and dictfmt from the dictd project.
Missing binaries fail the test. Install with `brew install dict` or
`sudo apt-get install dictd dictfmt dict`. Validated peer version: dictd1.13.3/rf,
dictfmt1.13.3. Tests generate a private dictionary with dictfmt, start dictd on loopback
using a private config/PID file and foreground mode, and run structured requests through
NetGet's registered client. The RealServer guard kills the owned process group on success
and panic and deletes its temporary files. No external dictionary or network service is needed.

The independent test covers DEFINE, prefix MATCH, SHOW DB/STRAT/INFO/SERVER, CLIENT,
STATUS, HELP, missing word/database errors and QUIT. The NetGet pair tests structured
handler definitions, matches, listings, dot-stuffing and final QUIT delivery.

Wire tests cover quoting/injection, command size, list count/terminal-code correlation,
multiple definitions, dot decoding, CRLF, truncation, malformed fields, line/whole-response
bounds and greeting rejection. Lifecycle fixtures exercise fragmented final replies followed
by EOF, busy rejection, injected disconnect during a stalled response, and client removal
during a stalled greeting. Raw socket fixtures are negative/lifecycle evidence, separate
from the independent dictd interoperability test. No pcap oracle or fuzz claim is made.

Validation: all8 client tests and all25 existing server tests passed at100 threads,
including independent dictd, dictfmt and dict1.13.3. No cases ignored or skipped.
