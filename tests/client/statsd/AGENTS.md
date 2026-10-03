# StatsD emitter validation

`e2e_test` starts clients through `ClientForm`: exact UDP wire bytes and counts, multiline
UTF-8 events, tagged counters, signed gauges, service checks, strict StatsD rejection, static
and Python-script connected handlers, IPv6, injection during a parked manual connect event,
command-handle teardown and OS socket release on stop/disconnect. Handwritten wire assertions
are distinguished from independent implementation evidence.

`real_server_test` drives the independent reference Node collector `statsd@0.9.0`. A tiny custom
backend prints its aggregates; assertions require sample-adjusted counter=6, gauge=7 after
absolute/delta emissions, timer=[12], and a deduplicated set=["alice"]. Its own UDP parser and
aggregation perform that work. Harness compatibility shims supply removed Node `util.log` and
force ephemeral loopback listening ports (StatsD otherwise interprets port zero as 8125/8126).
No parser or aggregation replacement is used. Process/import/timeouts fail, never skip.

Setup and combined commands are in `tests/server/statsd/AGENTS.md`. Requires Node and Python
peers; environment `NODE_PATH` points to the extracted pinned reference source, `PYTHONPATH`
to the pinned emitter installation. `kill_on_drop` and explicit kill/wait bound peer lifetime.
All deterministic tests use zero model calls. External DogStatsD Agent receiver, pcap and fuzz
coverage remain absent, so the protocol is Experimental.
