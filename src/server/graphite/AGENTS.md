# Graphite Carbon plaintext collector

Feature `graphite`; registry `Graphite`; keywords `graphite`, `carbon`; TCP 2003.
Experimental. Native UTF-8 codec, no additional Rust dependency.

The selected scope is timestamped Carbon plaintext: `<path> <value> <timestamp>\n`.
Paths have no whitespace/control characters; UTF-8 and semicolon-tagged paths pass through.
Values must be finite. Timestamps are nonnegative UNIX seconds (fractional seconds accepted),
or `-1`, resolved to receiver time before dispatch. No normalization, tag interpretation,
aggregation, Whisper database, query API, Pickle, UDP, AMQP, TLS or authentication.

Each `graphite_batch` exposes typed `metrics: [{path,value,timestamp}]`, `record_count`,
`source_addr`. TCP does not preserve sender action boundaries: complete lines available in
the stream buffer are grouped into at most 256 records. No task or model call per metric.
Unmatched batches go into the existing bounded access log; `llm_fallback=false` is the startup
default, even with a nonempty instruction. Explicit script/static/manual/LLM handlers always
use the standard dispatcher; `llm_fallback=true` additionally enables unmatched model calls.
`collect_graphite_batch` observes a batch; it does not persist a time series.

The parser retains at most 4096 bytes of an incomplete line plus an 8192-byte read. Lines
over 4096 bytes, invalid UTF-8, malformed numbers and incomplete EOF close the offending
connection with a diagnostic access log; the listener remains available. There are at most
256 concurrent connections. Obtaining the next complete line has an absolute 30-second
deadline that does not reset for trickled bytes; handler time sits outside this deadline.
No response exists in the Carbon plaintext grammar, so errors/cap refusals close silently
on the wire and log the reason. Handler errors fail closed. All listener/peer tasks are
registered; server stop cancels parked handlers and closes established sockets.

References: [Graphite feeding Carbon](https://graphite.readthedocs.io/en/stable/feeding-carbon.html),
[official Carbon parser](https://github.com/graphite-project/carbon/blob/1.1.10/lib/carbon/protocols.py).
Independent interoperability and bootstrap: `tests/server/graphite/CLAUDE.md`.

Peer controls: Carbon plaintext collectors never send application replies or unsolicited peer messages. Pending requests remain answerable through the ordinary event handler; no uncorrelated wire reply is offered.
