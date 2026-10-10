"""bob, a matrix-nio user on a real homeserver, as the other side of NetGet's client: waits
for NetGet's invite, joins, says "ping" once NetGet has welcomed him, and reports every
message he reads. Prints one JSON line per step; reads nothing from stdin until "done".

Usage: nio_bob.py <homeserver URL> <password>
"""
import asyncio
import json
import sys
import time

from nio import AsyncClient, RoomMessageText

URL, PASSWORD = sys.argv[1], sys.argv[2]


def out(**kw):
    print(json.dumps(kw), flush=True)


async def until(bob, room, seen, pred, secs=60):
    deadline = time.monotonic() + secs
    while time.monotonic() < deadline:
        if any(pred(s) for s in seen):
            return True
        r = await bob.sync(timeout=3000)
        joined = r.rooms.join.get(room) if room else None
        for e in joined.timeline.events if joined else []:
            if isinstance(e, RoomMessageText):
                seen.append([e.sender, e.body])
                out(step="message", sender=e.sender, body=e.body)
    return any(pred(s) for s in seen)


async def main():
    bob = AsyncClient(URL, "bob")
    try:
        r = await bob.login(PASSWORD)
        await bob.sync(timeout=0)
        out(step="ready", user_id=r.user_id)
        room = None
        deadline = time.monotonic() + 60
        while room is None and time.monotonic() < deadline:
            r = await bob.sync(timeout=3000)
            for rid, info in r.rooms.invite.items():
                room = rid
                inviter = next((e.sender for e in info.invite_state
                                if getattr(e, "state_key", None) == bob.user_id), None)
                out(step="invited", room_id=rid, inviter=inviter)
        if room is None:
            out(step="no_invite")
            return
        r = await bob.join(room)
        out(step="joined", type=type(r).__name__)
        seen = []
        ok = await until(bob, room, seen, lambda s: s[1].startswith("welcome"))
        out(step="welcomed", ok=ok)
        await bob.room_send(room, "m.room.message", {"msgtype": "m.text", "body": "ping"})
        ok = await until(bob, room, seen, lambda s: s[1] == "pong")
        out(step="ponged", ok=ok)
        ok = await until(bob, room, seen, lambda s: s[1] == "from the operator")
        out(step="operator", ok=ok, seen=seen)
    finally:
        await bob.close()


asyncio.run(main())
