"""One session of the official Python Pulsar client (pulsar-client, the C++ library) against a
broker: subscribe, produce single and batched messages, see one refused, read back what was
delivered. Prints one JSON line per step; the last is {"step": "done", ...}.

Usage: python3 py_session.py pulsar://127.0.0.1:PORT
"""
import json
import sys

import pulsar

url = sys.argv[1]
out = lambda **kw: print(json.dumps(kw), flush=True)

client = pulsar.Client(url, operation_timeout_seconds=20)
orders = client.subscribe("orders", "audit", consumer_type=pulsar.ConsumerType.Shared)
replies = client.subscribe("persistent://public/default/replies", "audit")
out(step="subscribed")

producer = client.create_producer("orders", producer_name="py-producer")
first = producer.send(b"order 1", properties={"customer": "ada"}, partition_key="ada")
producer.send("order 2 ✓".encode())
out(step="sent", first=str(first))
try:
    producer.send(b"forbidden order")
    out(step="refused", ok=False)
except Exception as e:  # the C++ library fails the send it was refused (as ChecksumError)
    out(step="refused", ok=True, error=str(e))
producer.send(b"order 3")

batched = client.create_producer("orders", producer_name="py-batch", batching_enabled=True,
                                 batching_max_messages=3, batching_max_publish_delay_ms=50)
for i in range(3):
    batched.send_async(f"batch {i}".encode(), None, properties={"i": str(i)})
batched.flush()
out(step="batched")


def drain(consumer, n):
    got = []
    for _ in range(n):
        m = consumer.receive(timeout_millis=15000)
        got.append({"data": m.data().decode(), "properties": m.properties(), "key": m.partition_key(),
                    "producer": m.publisher_name() if hasattr(m, "publisher_name") else None})
        consumer.acknowledge(m)
    return got


received = drain(orders, 6)
answered = drain(replies, 6)
out(step="done", received=received, replies=answered)
client.close()
