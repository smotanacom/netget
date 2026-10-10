"""Two matrix-nio users against a homeserver: a wrong password, login, a room created with
an invite, the invite seen and joined, messages both ways through /sync, a refused
message, members, history, logout. Prints one JSON line per step.

Usage: nio_session.py <homeserver URL> <server name>
"""
import asyncio
import json
import sys
import time

from nio import AsyncClient, RoomMessageText

URL, SERVER = sys.argv[1], sys.argv[2]


def out(**kw):
    print(json.dumps(kw), flush=True)


def kind(r):
    return type(r).__name__


async def main():
    bad = AsyncClient(URL, "alice")
    r = await bad.login("not-the-password")
    out(step="bad_login", type=kind(r), errcode=getattr(r, "status_code", None))
    await bad.close()

    alice = AsyncClient(URL, "alice")
    bob = AsyncClient(URL, "bob")
    try:
        r = await alice.login("wonderland", device_name="nio")
        out(step="login", type=kind(r), user_id=r.user_id, device_id=r.device_id)
        r = await bob.login("builder")
        out(step="login_bob", type=kind(r), user_id=r.user_id)
        await bob.sync(timeout=0)
        await alice.sync(timeout=0)

        bob_id = f"@bob:{SERVER}"
        r = await alice.room_create(name="netget-test", invite=[bob_id])
        out(step="create", type=kind(r), room_id=getattr(r, "room_id", None))
        room = r.room_id

        invited = {}
        deadline = time.monotonic() + 20
        while room not in invited and time.monotonic() < deadline:
            r = await bob.sync(timeout=5000)
            invited.update(r.rooms.invite)
        name = None
        if room in invited:
            for e in invited[room].invite_state:
                if getattr(e, "name", None):
                    name = e.name
        out(step="invite", seen=room in invited, room_name=name)

        r = await bob.join(room)
        out(step="join", type=kind(r), room_id=getattr(r, "room_id", None))
        await alice.sync(timeout=0)

        r = await alice.room_send(
            room, "m.room.message", {"msgtype": "m.text", "body": "hello"}
        )
        out(step="send", type=kind(r), event_id=getattr(r, "event_id", None))
        # Sending the same transaction again is answered with the same event.
        txn = "netget-idem"
        a = await alice.room_send(
            room, "m.room.message", {"msgtype": "m.text", "body": "once"}, tx_id=txn
        )
        b = await alice.room_send(
            room, "m.room.message", {"msgtype": "m.text", "body": "once"}, tx_id=txn
        )
        out(step="idempotent", same=a.event_id == b.event_id)

        seen = []
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline and not any(
            b == "echo: once" for _, b in seen
        ):
            r = await bob.sync(timeout=5000)
            joined = r.rooms.join.get(room)
            if joined:
                for e in joined.timeline.events:
                    if isinstance(e, RoomMessageText):
                        seen.append((e.sender, e.body))
        out(step="bob_saw", messages=seen)

        r = await bob.room_send(
            room, "m.room.message", {"msgtype": "m.text", "body": "forbidden"}
        )
        out(
            step="refused",
            type=kind(r),
            status_code=getattr(r, "status_code", None),
            message=getattr(r, "message", None),
        )

        r = await alice.joined_members(room)
        out(step="members", members=sorted(m.user_id for m in r.members))
        r = await alice.joined_rooms()
        out(step="joined_rooms", rooms=r.rooms)
        r = await alice.room_messages(room, start="", limit=50)
        out(
            step="history",
            bodies=[e.body for e in r.chunk if isinstance(e, RoomMessageText)],
        )
        r = await alice.whoami()
        out(step="whoami", user_id=r.user_id, device_id=r.device_id)
        await alice.logout()
        r = await alice.whoami()
        out(step="after_logout", type=kind(r), status_code=getattr(r, "status_code", None))
    finally:
        await alice.close()
        await bob.close()


asyncio.run(main())
