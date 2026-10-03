# Bolt client verification

All Cargo runs use the programme's serialized run_cargo.py/shared target and disk
reserve guard. Filter the client target by `bolt::`; run the full existing server
Bolt suite as well. Missing peers fail; no ignores or skip-when-missing gates.

Bootstrap the official Community5.26.31 peer with
`scripts/test-peers/install-neo4j.sh /absolute/owned/peer/root`, then export
`NETGET_NEO4J_HOME` to the printed installation and `NETGET_BOLT_JAVA_HOME` to an
existing Java21 home (or supply JAVA_HOME). Local native evidence uses
OpenJDK21.0.12.1 and bundled cypher-shell5.26.31/JavaDriver5.28.15. The existing
server suite also passes with installed cypher-shell2026.09.0/JavaDriver6.2.1.
Put the verified distribution's bin directory first on PATH for the required
client and server suites; no separate cypher-shell installation is needed.
The official archive is
165,211,960bytes; SHA256
f8fc23340561405f1ff10ca6ac2d317d095d3c74509a616883c45d7a61f5cfec.
The installer uses native verified HTTPS, exact length/digest checks and bounded
path-safe extraction into caller-owned storage. Python3.12+ is needed for its
hashlib.file_digest and tar data filter; the peer wrapper itself uses ordinary
Python3. No global installation, container or user daemon/config change.

real_server_test owns separate temporary HOME, NEO4J_CONF/data/logs/run/import/
plugins/transaction directories; loopback Bolt only, HTTP/HTTPS and usage reporting
disabled, heap256MiB/pagecache64MiB/worker threads4, small transaction-log caps.
Neo4j-admin's supported @argument file provisions the fixed fixture password from
a temporary0600file, removed before daemon launch. Credentials are absent from
CLI arguments. RealServer owns the complete process group and death tie. Readiness
requires Neo4j's Started log and a TCP probe, not elapsed sleep. Cypher-shell uses
credential environment variables and the fixture HOME under a bounded deadline.
The service/version/authentication/queries/paging/types/transactions/errors and
independent CLI readback are observed on the native service. Its temporary graph
is solely peer-owned fixture state, never a NetGet protocol database.

schema_test verifies every advertised action/event, plain parameter preservation,
exact action depth/node/retained-content boundaries and safe10,000-depth disposal,
version proposal selection/refusals, native response arity/metadata/failure types,
graph path tables/direction/index checks, temporal units, byte omission/nonfinite
float categories, fixed-schema reflection redaction, actual depth32/+1 and
message1MiB/+1 boundaries. Mechanical tests use NetGet's codec only as mechanics
evidence, never as independent interoperability evidence.

session_test covers correlated qid/field ordering, bounded pages/has_more/native
optional metadata, discard/transaction phase refusals, withheld tentative rows
on FAILURE/IGNORED and RESET recovery,5.0 HELLO authentication, malformed width/
page count/summary/message/page-byte refusals, whole deadline despite NOOP,
overlapping operation refusal, disconnect/removal while I/O is pending, endpoint
and startup bounds, selected-version dribbling, exact event-queue capacity and
partial PULL/DISCARD summary refusals. Every helper task is owned and cancelled/joined.

pair_test exercises the advertised static/script examples against the existing
programmable server, scripted graph/parameter values, mocked-model login/RUN/PULL
with common memory and private diagnostics, plus four bounded handler followups.
The OpenSSL peer proves certificate rejection only; it is not evidence of a
successful trusted TLS exchange. The client remains Experimental.
