# Selected Bolt client

Experimental direct Neo4j-compatible sessions. The feature `bolt` registers this
client beside the existing programmable server. Native `bolt://` uses TCP and
`bolt+s://` verifies WebPKI roots and the endpoint name; browser transport is TCP
only. A missing scheme means `bolt://`. URL credentials, paths, queries, fragments,
`neo4j://` routing and TLS trust overrides are refused.

The legacy handshake proposes5.6..5.8 then5.0..5.4. Version5.5 was never released;
manifest negotiation and Bolt6 are excluded. HELLO includes a user agent and5.3+
bolt_agent. In5.0 authentication belongs to HELLO; in5.1+ it belongs to LOGON.
Startup username/password must appear together. With neither,5.1+ connects in the
authentication phase and awaits `bolt_login`; omitted login credentials explicitly
request the none scheme. A connected event honestly reports whether native
successful authentication already occurred. No challenge, password, token file,
local query engine or graph database is exposed to the model.

`bolt_login`, `bolt_logoff`, `bolt_run`, `bolt_pull`, `bolt_discard`, `bolt_begin`,
`bolt_commit`, `bolt_rollback`, `bolt_reset` and `disconnect` are selected actions.
RUN accepts plain JSON parameters (signed64-bit integers) and selected database,
mode, opaque bookmarks and transaction timeout. Maps such as `$node` remain maps:
graph/temporal/spatial/bytes input values are excluded. One ordered operation,
one open result and one explicit transaction are supported. RUN establishes native
field order and optional qid; it does not establish query completion. A result must
be consumed/discarded before another RUN or transaction completion. PULL requests
1..500records; DISCARD requests all remaining rows without exposing records.

Responses require native exact arity and typed known metadata. Positional record
arrays preserve duplicate/empty field names without flattening to objects. RECORDs
remain tentative until PULL SUCCESS; FAILURE/IGNORED withholds them. `has_more`
keeps the cursor; omission means the native final summary. Optional bookmarks,
query type, timing, update counts, hints/statuses and plans are preserved only when
returned. Native metadata's `type` is a query classification inside `summary`,
distinct from the common action-envelope `type` discriminant. Native pre5.7
FAILURE code/message and5.7+ GQL status, description, Neo4j code, diagnostic record
and nested cause are kept. Failed queries require RESET. Authentication, LOGOFF
and RESET failures close the connection; their terminal event is best effort.

Result values are primitive JSON or explicit typed wrappers: `$node`,
`$relationship`, `$unbound_relationship`, `$path`, `$date`, `$time`, `$local_time`,
`$datetime`, `$datetime_zone`, `$local_datetime`, `$duration`, `$point`, `$bytes`
and `$float`. Graph wrappers preserve native ids/element ids and compact path
node/relationship tables plus signed index pairs; no expansion into a graph store.
Relationship labels use `relationship_type`. Temporal values retain native epoch,
nanosecond and offset units rather than inventing ISO text. Bytes expose length
and content_omitted only. Nonfinite floats retain their named category; spatial
coordinates must be finite. Unsupported tags/schema fail closed.

Both registered owned tasks use event/action queues8; the common injection channel is bounded16. One task owns the transport;
the other invokes common static/script/manual/model handlers and shared memory.
The command channel exists before the connected event. Four handler followups
bound automatic action chains; fresh injection remains available. Pending I/O
refuses overlapping operations, permits disconnect immediately and is cancelled
by client removal. RESET is supported between operations, including failure
recovery; it does not interrupt an in-flight operation. Idle disconnect attempts
bounded GOODBYE. Deadline/transport/schema failure closes the session with unknown
backend outcome; no retry, replay, reconnect or invented success follows.

Bounds: message1MiB including chunked body; PackStream wire containers32; action
JSON depth32/nodes65,536/retained8MiB before copies and iterative owned refusal
disposal; page500records and4MiB wire/converted records; fields256; query/result
strings64KiB; password16KiB; parameter maps256/lists10,000; endpoint4096; startup
and whole-operation deadline1..30s(default15), unaffected by NOOP/dribbling. No
idle session deadline. Converted events are bounded at depth40/nodes65,536/8MiB
to accommodate generated typed wrappers around strictly depth32 native values.

The latest bounded password is retained only for reflection redaction. Native
string values and dynamic map keys are redacted before typed wrapper construction,
so schema field names stay stable even if a password equals `properties`. Escaped
literal forms are covered. Fixed schema metadata keys remain; arbitrary extra keys
can be omitted/redacted. Every event continues offering the password-bearing login
definition to keep common incidental diagnostics private after authentication.
Injected access-log copies contain only validated known action discriminants.
Action execution and actual credential/query wire values are preserved. Common
explicit display actions retain their intentionally requested behavior.

Source: `actions.rs` defines actions/events/metadata/examples; `api.rs` validates
selected messages and native result values; `mod.rs` owns negotiation, transport,
state transitions, event dispatch and cancellation. Pure PackStream/chunking is
reused from the existing server, without reusing its model/graph answer engine.

Tests use an isolated verified official Neo4j Community5.26.31/Java21 daemon plus
native cypher-shell, the NetGet pair, scripted/static/mocked-model handlers and an
owned mechanical peer. Missing independent peers fail. Existing server evidence
remains independent cypher-shell against programmable NetGet, including actual
connection bounds. No positive trusted-local TLS/browser/capture/conformance
claim is made; independent untrusted-certificate rejection is tested.

Primary specifications: [handshake](https://neo4j.com/docs/bolt/current/bolt/handshake/),
[messages](https://neo4j.com/docs/bolt/current/bolt/message/),
[structures](https://neo4j.com/docs/bolt/current/bolt/structure-semantics/),
[compatibility](https://neo4j.com/docs/bolt/current/bolt-compatibility/).
