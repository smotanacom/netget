# Selected legacy TACACS+ server

Feature `tacacs`, registry `TACACS`, aliases `tacacs`/`tacacs+`, stack
ETH>IP>TCP>TACACS, TCP49. Experimental selected
[RFC8907](https://www.rfc-editor.org/rfc/rfc8907.html) legacy AAA subset.
[RFC9887](https://www.rfc-editor.org/rfc/rfc9887.html) TLS1.3/mutual
certificate validation/port300 is explicitly excluded. Legacy MD5 chained
body obfuscation provides neither encryption security nor integrity.

The native codec owns header/session/version/odd-even sequence, ASCII LOGIN
GETUSER (at most3 retries)/GETPASS and PAP LOGIN minor1. Unknown header flag
bits are ignored, the unencrypted flag refused, and SINGLE_CONNECT declined;
one session per accepted connection. Defined unsupported authentication flows
receive FAIL; unknown enumerated values/malformed known-type bodies receive
ERROR. Unknown type mirrors the clear header with sequence advanced and zero
body; sequence255 closes without wrapping. Full PRECIS/Unicode usernames,
CHAP/MSCHAP/ENABLE/password changes/SENDAUTH, redirects/restarts and multiplexing
are excluded. Strings use printable ASCII; usernames have no spaces.

Required startup shared_secret is1..255UTF8bytes. Optional client_secrets
contains at most64 unique exact-IP overrides. Secrets are private transport
configuration and are removed from cloned SpawnContext startup fields before
handlers/tasks are created. There is no CIDR matching or rotation protocol.

Common static/script/manual/model handlers receive typed authentication,
authorization and accounting events. Authentication password is intentionally
chosen-handler input and follows common volatile access-log retention;
transport secrets never enter events. Shared request privacy suppresses incidental
model/script/error diagnostics, retaining the original input and executable
results. Default policy is authentication FAIL, authorization FAIL, accounting
ERROR with no default model call (`llm_fallback=false`). Backend/action failures,
wrong or duplicate replies produce ERROR before any acceptance. Responses are
terminal typed enums, strings and ordered mandatory/optional arguments, never
raw bytes, base64 or model-built packets. No account/policy domain store is
implemented, and authentication is not bound to later authorization.

`record_tacacs_accounting` SUCCESS records the validated request in the common
1000-entry process-volatile access log before transmitting success. Eviction
and process loss apply; owner removal does not make the log durable. There is
no journal, billing, durable storage or exactly-once claim. START, STOP,
WATCHDOG and UPDATE (START+WATCHDOG) remain distinct typed record kinds.

Bounds:16KiB packet body,32 ordered wire arguments,255byte short fields,
1024byte response message/data,8 authentication rounds,256 active connections.
Native action/reply JSON is checked iteratively before cloning/serialization:
128KiB retained-content estimate,1024nodes,depth8; rejected owned JSON/nested
results are dropped iteratively. Packet and handler deadlines default30s,
configurable1..300s; writes/shutdown10s. Accepted sockets and the listener are
registered through common owned server tasks with connection permits and cleanup.
Peer EOF cancels parked handlers; removal cancels listener/sockets/intercepts.
TCP connections without a complete bounded header close on timeout/capacity.

Required independent interoperability and literal fixture provenance are in
[tests](../../../tests/server/tacacs/AGENTS.md). No fuzz or production-capture
claim is made. Central root owns CI, capture mappings and roadmap publication.
