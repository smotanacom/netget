"""OpenTelemetry Python's Zipkin JSON exporter, unchanged, against a collector.

Usage: otel_peer.py http://host:port
Builds a two-span trace with the OpenTelemetry SDK, exports it with ZipkinExporter (v2 JSON),
then exports a span named refuse-me, and prints one JSON line with both export results and
the trace id. The exporter reports FAILURE for any non-2xx answer.
"""
import json, sys
from opentelemetry.sdk.resources import Resource
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import SimpleSpanProcessor
from opentelemetry.sdk.trace.export.in_memory_span_exporter import InMemorySpanExporter
from opentelemetry.exporter.zipkin.json import ZipkinExporter
from opentelemetry.trace import SpanKind

base = sys.argv[1]
memory = InMemorySpanExporter()
provider = TracerProvider(resource=Resource.create({"service.name": "otel-checkout"}))
provider.add_span_processor(SimpleSpanProcessor(memory))
tracer = provider.get_tracer("netget-peer")
with tracer.start_as_current_span("place-order", kind=SpanKind.SERVER) as root:
    root.set_attribute("order.id", 42)
    root.add_event("validated")
    with tracer.start_as_current_span("reserve-stock", kind=SpanKind.CLIENT):
        pass
spans = memory.get_finished_spans()
exporter = ZipkinExporter(endpoint=base + "/api/v2/spans", timeout=10)
first = exporter.export(spans)
memory.clear()
with tracer.start_as_current_span("refuse-me"):
    pass
second = exporter.export(memory.get_finished_spans())
trace_id = format(spans[0].context.trace_id, "032x")
print(json.dumps({"accepted": first.name, "refused": second.name, "trace_id": trace_id}))
