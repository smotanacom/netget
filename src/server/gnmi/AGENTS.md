# gNMI receiver — native selected subset

The `gnmi` feature registers `gNMI` over native HTTP/2 and gRPC. Receiver binding
uses a literal IPv4/IPv6 host, default 127.0.0.1, and the IANA `gnmi-gnoi`
assigned TCP port 9339 for normal startup. An explicit port zero requests an
OS-assigned port atomically; all peer fixtures use it. The client also defaults
to 9339 when the endpoint omits its port. Primary registry:
https://www.iana.org/assignments/service-names-port-numbers?search=gnmi-gnoi.
`build.rs`
generates tonic 0.12/prost 0.13 types from the unmodified OpenConfig v0.14.1
schema under `proto/gnmi`; that directory records its license and digests.
Capabilities advertises specification version `0.10.0`. The receiver has no
device or configuration datastore: handlers decide snapshots, acknowledge an
entire Set transaction, and supply subscription updates.

Capabilities, Get and Set are unary. Set acknowledges every requested operation
in delete/replace/update order only after one `gnmi_set_accepted`; a handler error
rejects the entire transaction. Get supports ALL/CONFIG/STATE/OPERATIONAL and
the selected PROTO, JSON, JSON_IETF and ASCII encodings. Paths use structured
`elem` with origin, target and keys. Signed/unsigned 64-bit values, timestamps
and decimal digits are decimal strings at the model boundary. Values support
string, integer, unsigned integer, bool, finite double, decimal, scalar leaf
lists, structured JSON/JSON_IETF and ASCII. Protobuf and JSON byte fields never
enter model events.

Subscribe supports ONCE, POLL and STREAM. Initial and POLL snapshot actions end
with one `gnmi_sync`; ONCE then ends automatically. STREAM accepts updates and
`gnmi_wait` (1..1000 ms) or `gnmi_finish` after initial sync. `updates_only` emits
initial/POLL sync without snapshot updates. TARGET_DEFINED and ON_CHANGE are
selected; the handler controls ephemeral update opportunities. QoS zero is a
disabled no-op. SAMPLE, suppression, heartbeat, nonzero QoS, aggregation,
extensions, union_replace, deprecated paths/values/errors, opaque bytes/Any,
authentication, reflection and YANG execution are rejected or excluded.

`use_tls` defaults to false for local cleartext fixtures. TLS requires explicit
`cert_file` and `key_file`, 1..16 certificates and ALPN h2. Each credential is a
regular file of at most 1 MiB; metadata is checked before opening and on the
opened handle, with nonblocking open on Unix. File reads use a limit plus one.
Credential startup and each accepted TLS handshake have a 10-second deadline.
There is no verification bypass or client-certificate authentication.

Bounds are checked before prost allocates nested/repeated data: each encoded and
expanded message is at most 1 MiB, each value at most 64 KiB, depth 32 and a
shared protobuf/JSON node budget of 10,000. Paths have at most 32 elements and
8 keys per element; names/keys are 128 bytes, key values 1024 bytes, origin and
target 256 bytes. Leaf lists have 64 scalar entries, model lists and Get
notifications 128 entries, notification updates/deletes 256 entries, and Set
operations 256 combined. Typed model actions independently check depth, nodes,
text and retained size before cloning or serializing.

Transport admission is 256 TCP connections, 64 active RPCs per receiver, and
16 HTTP/2 streams per connection. The first byte is due within 30 seconds;
idle connections close after 120 seconds, checked every six seconds. HTTP/2
uses 64 KiB stream and 1 MiB connection receive windows, 16 KiB frames and
32 KiB header lists. Semantic request headers are limited to 64 fields/32 KiB;
reserved content type, encoding and timeout fields must be unique. Requests
require POST, HTTP/2, a known gNMI method and no query. Unary bodies contain
exactly one message, buffered only within 1 MiB plus its five-byte frame prefix.

`rpc_timeout_secs` defaults to 300 and accepts 1..3600; unique `grpc-timeout`
can shorten it. The RPC owns its handler, input and output futures, admission
permit and deadline until response body EOF/error/drop. It does not claim TCP
flush ownership after body EOS. Native trailers-only errors preserve the
original body's END_STREAM and size hint. Connection teardown cancels all
HTTP/2 workers. A subscription retains at most 16 pending messages/1 MiB,
256 input and output messages, and 258 handler invocations. Stop, reset and
deadline drop outstanding handlers and subscriptions.

No answer, an invalid answer or backend failure fails closed with a fixed gRPC
error; backend overload uses UNAVAILABLE. Server diagnostics are not copied
into the peer status. Startup examples provide actual LLM, Python script and
static-handler forms. Updates belong to established Subscribe RPCs, so this
protocol declares `request_only` rather than a connection-level send handle.

Maturity remains Experimental. Independent selected-scope tests use mandatory
pinned gNMIc and public generated grpcio peers in both roles, plus native pairs;
they do not prove full device/YANG behavior, browser interoperability, fuzz or
pcap conformance. Measured checks and retained initial failures are documented
in `tests/server/gnmi/AGENTS.md` and `tests/client/gnmi/AGENTS.md`.
