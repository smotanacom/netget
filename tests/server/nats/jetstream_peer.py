"""nats-py's JetStream client, unchanged, against a JetStream server: add a stream, publish
three messages (and one the server refuses), bind a durable pull subscription, fetch two and
ack them, fetch the third, read stream info, and delete the stream. Prints one JSON line.

Usage: jetstream_peer.py nats://host:port
"""
import asyncio, json, sys
import nats
from nats.errors import TimeoutError
from nats.js.errors import APIError, NotFoundError


async def main(url):
    nc = await nats.connect(url, connect_timeout=10, allow_reconnect=False)
    js = nc.jetstream(timeout=10)
    out = {}
    info = await js.add_stream(name="PYORDERS", subjects=["py.orders.*"])
    out["created"] = info.config.name
    seqs = []
    for i in range(3):
        ack = await js.publish("py.orders.new", json.dumps({"id": i + 1}).encode(), headers={"Order-Index": str(i + 1)})
        assert ack.stream == "PYORDERS", ack
        seqs.append(ack.seq)
    out["published_seqs"] = seqs
    try:
        await js.publish("py.orders.rejected", b"no")
        out["refused"] = None
    except APIError as e:
        out["refused"] = e.description
    sub = await js.pull_subscribe("py.orders.*", durable="pyproc", stream="PYORDERS")
    msgs = await sub.fetch(2, timeout=5)
    out["fetched"] = [{"subject": m.subject, "data": m.data.decode(), "header": (m.headers or {}).get("Order-Index"),
                       "stream_seq": m.metadata.sequence.stream, "consumer_seq": m.metadata.sequence.consumer} for m in msgs]
    await msgs[0].ack()
    await msgs[1].ack_sync()
    rest = await sub.fetch(5, timeout=2)
    out["rest"] = [m.data.decode() for m in rest]
    for m in rest:
        await m.ack()
    try:
        await sub.fetch(1, timeout=1)
        out["empty"] = "got a message"
    except TimeoutError:
        out["empty"] = "timeout"
    si = await js.stream_info("PYORDERS")
    out["stream_messages"] = si.state.messages
    try:
        await js.stream_info("MISSING")
    except NotFoundError as e:
        out["missing"] = e.description
    await js.delete_stream("PYORDERS")
    await nc.close()
    print(json.dumps(out), flush=True)


asyncio.run(main(sys.argv[1]))
