# Zipkin server tests

`store_script` (in `wire_test.rs`) keeps reported spans in a JSON file and answers queries
from it; a span named refuse-me is a 429, an unknown trace a 404, and autocompleteKeys answers
a deliberately wrong shape.

- `wire_test.rs`: canonical spans (padding, dropped unknowns, stringified tags), every query
  shape answered from what was reported, gzip; malformed spans, an empty report, proto3,
  1001 spans, an oversized body, a gzip bomb, unknown endpoints and parameters, a wrong-shape
  answer (503) and an unreachable model (503, never 202).
- `real_client_test.rs`: openzipkin/zipkin-go 0.4.3 (tracer + HTTP reporter, then its model
  decoder reads the trace back through the read API, and its reporter logs the 429) and
  OpenTelemetry Python 1.45.1's Zipkin JSON exporter (SUCCESS, then FAILURE on the 429; the
  stored spans checked field by field).

Peers from `tests/server/zipkin/install_peers.py`. No LLM calls.
