"""autobahn-python 24.4.2, unchanged (asyncio flavour, wamp.2.json), against a WAMP router.

Usage: peer.py HOST PORT

Joins realm1, registers com.example.add2, subscribes exactly and by prefix, calls add2 through
the router (and with the wrong arity, so its own callee raises), publishes with acknowledgement
without excluding itself, calls the router's com.example.time and com.example.forbidden, is
refused a second registration of add2 from another session and refused realm "blocked", and
leaves with GOODBYE. One JSON line per step.
"""
import asyncio, json, sys
from autobahn.asyncio.wamp import ApplicationSession
from autobahn.asyncio.websocket import WampWebSocketClientFactory
from autobahn.wamp.exception import ApplicationError
from autobahn.wamp.types import ComponentConfig, PublishOptions, SubscribeOptions

out = lambda **kw: print(json.dumps(kw), flush=True)
host, port = sys.argv[1], int(sys.argv[2])


async def join(realm):
    loop = asyncio.get_running_loop()
    joined = loop.create_future()

    class Session(ApplicationSession):
        def onJoin(self, details):
            if not joined.done():
                joined.set_result(self)

        def onLeave(self, details):
            if not joined.done():
                joined.set_exception(ApplicationError(details.reason, details.message))
            super().onLeave(details)

        def onDisconnect(self):
            if not joined.done():
                joined.set_exception(ConnectionError("disconnected before WELCOME"))

    factory = WampWebSocketClientFactory(lambda: Session(ComponentConfig(realm)), url=f"ws://{host}:{port}/")
    await loop.create_connection(factory, host, port)
    return await asyncio.wait_for(joined, 15)


async def main():
    s = await join("realm1")
    out(step="join", session=s._session_id)
    await s.register(lambda a, b: a + b, "com.example.add2")
    got = asyncio.Queue()
    await s.subscribe(lambda *a, **k: got.put_nowait(("exact", list(a), k)), "com.example.topic")
    await s.subscribe(lambda *a, details=None, **k: got.put_nowait(("prefix", details.topic, list(a))), "com.example.pre", options=SubscribeOptions(match="prefix", details_arg="details"))
    out(step="add2", result=await s.call("com.example.add2", 2, 3))
    try:
        await s.call("com.example.add2", 1)
        out(step="arity", error=None)
    except ApplicationError as e:
        out(step="arity", error=e.error)
    pub = await s.publish("com.example.topic", "hello", who="autobahn", options=PublishOptions(acknowledge=True, exclude_me=False))
    out(step="published", publication=pub.id)
    out(step="event", event=list(await asyncio.wait_for(got.get(), 5)))
    s.publish("com.example.pre.fix", 7, options=PublishOptions(exclude_me=False))
    out(step="prefix", event=list(await asyncio.wait_for(got.get(), 5)))
    res = await s.call("com.example.time", "utc")
    out(step="time", result=res if not hasattr(res, "results") else {"args": list(res.results), "kwargs": res.kwresults})
    try:
        await s.call("com.example.forbidden")
        out(step="forbidden", error=None)
    except ApplicationError as e:
        out(step="forbidden", error=e.error, args=list(e.args))
    other = await join("realm1")
    try:
        await other.register(lambda: 0, "com.example.add2")
        out(step="duplicate", error=None)
    except ApplicationError as e:
        out(step="duplicate", error=e.error)
    other.leave()
    try:
        await join("blocked")
        out(step="blocked", error=None)
    except ApplicationError as e:
        out(step="blocked", error=e.error)
    left = asyncio.get_running_loop().create_future()
    s.onLeave = lambda details: left.set_result(details.reason)
    s.leave()
    out(step="leave", reason=await asyncio.wait_for(left, 5))


asyncio.run(main())
