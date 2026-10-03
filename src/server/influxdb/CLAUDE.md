# InfluxDB v2 write collector

Implemented Experimental scope: POST /api/v2/write surface, typed native line protocol,
ns/us/ms/s timestamps, identity/gzip bodies, Token/Bearer authentication when a token
is configured, model-controlled acceptance/rejection/partial decisions. Standard bounded
access logs and shared memory only; no private time-series/schema/query storage.

Primary references: https://docs.influxdata.com/influxdb/v2/api/write/ and
https://docs.influxdata.com/influxdb/v2/reference/syntax/line-protocol/.
Independent receiver uses unmodified MIT InfluxData line-protocol v2.2.1 decoder behind a
test-only v2 HTTP contract; it is a decoder-backed receiver, not an official InfluxDB daemon.
Official Python InfluxDB2 client 1.50.0 is the independent emitter. Official InfluxDB 2.9.1
is the service receiver, with official Python readback. Daemon evidence is mandatory on
Linux/macOS; macOS uses existing amd64 execution support. Do not claim Beta/Stable, complete InfluxDB, queries, schema
conflict tracking, durable delivery, auth roles/org ACLs, TLS listener or retries.

Bounds:256 KiB compressed/decompressed body, 16 KiB line, 256 points, 1024 source lines,
64 tags/fields, 1024 byte names/org/bucket/token, 256 connections, 32 KiB/64 headers,
30s absolute header/body deadlines, 10s response-write deadline. No read/write timer wraps
the event handler; manual handlers keep their ordinary intercept timeout. Every listener
and peer task is owned by AppState. One request per TCP connection; stop releases sockets.
The quoted string subset excludes CR/LF/NUL; names exclude ASCII controls and trailing
backslashes. Single field/timestamp separators only; reserved underscore namespace/time
tag and field keys are rejected. Timestamps normalize with checked ns/us/ms/s arithmetic.

`influx_write` contains org, bucket, precision, typed points with source line/timestamp_ns,
syntax errors and authentication facts. No raw payload or token reaches event handlers.
Accept valid lines by default; syntax errors return400 with line and explicit counts.
Successful configured handlers without a protocol decision accept valid lines. Mixed
failed actions, dispatch failure, multiple decisions or invalid subsets fail closed503.
Configured actions can accept all, accept a valid subset or reject with bounded JSON
code/message and optional numeric Retry-After. No acceptance means persistent storage.

Peer licenses are MIT. Decoder archive SHA256 and every Python dependency version are
pinned in install_peers.py; official daemon platform hashes come from the v2.9.1 release
manifest (the install guide has obsolete example hashes):
https://github.com/influxdata/influxdb/releases/tag/v2.9.1.
