"""pycapnp (the C++ Cap'n Proto runtime), unchanged, in either role over directory.capnp.

  pycapnp_peer.py client HOST:PORT SCHEMA   call every method, print one JSON line
  pycapnp_peer.py server SCHEMA             serve Directory on 127.0.0.1, print READY <port>
"""
import asyncio, json, sys
import capnp

capnp.remove_import_hook()
d = capnp.load(sys.argv[-1])

ENTRY = {
    "name": "src", "size": 4096, "kind": "directory", "tags": ["code", "rust"],
    "owner": {"uid": 1000, "gid": 100}, "target": "/srv/src",
    "children": [{"name": "main.rs", "size": 12, "kind": "file", "priority": -3, "blob": b"\x00\xffhi"}],
    "scores": [1.5, -2.25], "hidden": False,
}


def plain(entry):
    """An Entry as JSON, as pycapnp reads it."""
    out = {"name": entry.name, "size": entry.size, "kind": str(entry.kind), "tags": list(entry.tags),
           "priority": entry.priority, "owner": {"uid": entry.owner.uid, "gid": entry.owner.gid},
           "which": str(entry.which()), "scores": list(entry.scores), "hidden": entry.hidden,
           "children": [plain(c) for c in entry.children]}
    if entry.which() == "target":
        out["target"] = entry.target
    if entry.which() == "blob":
        out["blob"] = entry.blob.hex()
    return out


async def client(addr):
    host, port = addr.rsplit(":", 1)
    stream = await capnp.AsyncIoStream.create_connection(host=host, port=int(port))
    cap = capnp.TwoPartyClient(stream).bootstrap().cast_as(d.Directory)
    out = {}
    out["ping"] = (await cap.ping()).pong
    out["add"] = (await cap.add(40, 2)).sum
    out["lookup"] = plain((await cap.lookup("readme")).entry)
    stored = await cap.store(ENTRY)
    out["stored"] = plain(stored.stored)
    out["count"] = stored.count
    try:
        await cap.fail("on purpose")
        out["fail"] = "returned"
    except capnp.KjException as e:
        out["fail"] = e.description
        out["fail_type"] = str(e.type)
    # Several calls in flight at once.
    sums = await asyncio.gather(*[cap.add(i, i) for i in range(5)])
    out["pipelined"] = [s.sum for s in sums]
    print(json.dumps(out), flush=True)


class Directory(d.Directory.Server):
    async def ping(self, **kwargs):
        return "pong from pycapnp"

    async def add(self, a, b, **kwargs):
        return a + b

    async def lookup(self, name, _context, **kwargs):
        if name == "missing":
            raise ValueError("no entry named missing")
        e = _context.results.init("entry")
        e.name = name
        e.size = 1234
        e.kind = "file"
        e.tags = ["doc", name]
        e.owner.uid = 501
        e.target = "/docs/" + name
        e.scores = [0.5]

    async def store(self, entry, _context, **kwargs):
        _context.results.stored = entry
        _context.results.count = len(entry.children) + 1

    async def fail(self, why, **kwargs):
        raise ValueError("refused: " + why)


async def serve():
    async def connection(stream):
        await capnp.TwoPartyServer(stream, bootstrap=Directory()).on_disconnect()

    server = await capnp.AsyncIoStream.create_server(connection, "127.0.0.1", 0)
    print("READY", server.sockets[0].getsockname()[1], flush=True)
    async with server:
        await server.serve_forever()


if sys.argv[1] == "client":
    asyncio.run(capnp.run(client(sys.argv[2])))
else:
    asyncio.run(capnp.run(serve()))
