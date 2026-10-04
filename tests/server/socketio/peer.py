"""python-socketio 5.17.0, unchanged, in both roles for NetGet's tests.

client URL [websocket-only]  the sync client (requests + websocket-client): connect (polling,
                             upgraded to WebSocket unless websocket-only), the welcome, an
                             emit with ack, the broadcast back, an ack the server asks for,
                             /admin with and without the right auth, an unknown namespace, a
                             server-side disconnect. One JSON line per step.
server                       an AsyncServer under uvicorn on 127.0.0.1:0 with / and /chat;
                             prints {"port": N}; serves until stdin closes.
"""
import json, sys, threading


def client(url, websocket_only):
    import socketio
    out = lambda **kw: print(json.dumps(kw), flush=True)
    transports = ["websocket"] if websocket_only else ["polling", "websocket"]
    got = {}
    events = {k: threading.Event() for k in ["welcome", "broadcast", "pong", "gone"]}
    sio = socketio.Client(reconnection=False)

    @sio.on("welcome")
    def welcome(*args):
        got["welcome"] = list(args); events["welcome"].set()

    @sio.on("chat message")
    def chat(*args):
        got["broadcast"] = list(args); events["broadcast"].set()

    @sio.on("ping me")
    def ping_me(arg):
        return "pong from python", arg

    @sio.on("pong received")
    def pong(*args):
        got["pong"] = list(args); events["pong"].set()

    @sio.event
    def disconnect(reason=None):
        got["gone"] = str(reason); events["gone"].set()

    sio.connect(url, transports=transports, auth={"token": "anyone"}, namespaces=["/"], wait_timeout=10)
    out(step="connect", sid=sio.sid, transport=sio.transport())
    assert events["welcome"].wait(10), "no welcome"
    out(step="welcome", args=got["welcome"])
    ack = sio.call("chat message", "hi from python", timeout=10)
    out(step="ack", args=list(ack) if isinstance(ack, (list, tuple)) else [ack])
    assert events["broadcast"].wait(10), "no broadcast"
    out(step="broadcast", args=got["broadcast"])
    sio.emit("ask me")
    assert events["pong"].wait(10), "no pong received"
    out(step="server_ack", args=got["pong"])
    for token, step in [("secret", "admin"), ("wrong", "admin_denied")]:
        other = socketio.Client(reconnection=False)
        try:
            other.connect(url, transports=transports, auth={"token": token}, namespaces=["/admin"], wait_timeout=10)
            out(step=step, connected=True, sid=other.get_sid("/admin"))
            other.disconnect()
        except socketio.exceptions.ConnectionError as e:
            out(step=step, connected=False, message=str(e))
    other = socketio.Client(reconnection=False)
    try:
        other.connect(url, transports=transports, namespaces=["/nowhere"], wait_timeout=10)
        out(step="unknown_namespace", connected=True)
    except socketio.exceptions.ConnectionError as e:
        out(step="unknown_namespace", connected=False, message=str(e))
    sio.emit("bye")
    assert events["gone"].wait(10), "server did not disconnect us"
    out(step="server_disconnect", reason=got["gone"])


def server():
    import asyncio
    import socketio, uvicorn
    sio = socketio.AsyncServer(async_mode="asgi", ping_interval=5, ping_timeout=5)

    @sio.event
    async def connect(sid, environ, auth):
        await sio.emit("welcome", ["python server", sid], to=sid)

    @sio.on("connect", namespace="/chat")
    async def chat_connect(sid, environ, auth):
        if not auth or auth.get("token") != "secret":
            raise socketio.exceptions.ConnectionRefusedError("not authorized")

    @sio.on("chat message")
    async def chat(sid, data):
        await sio.emit("chat message", ["broadcast", data])
        return "delivered", data

    @sio.on("chat message", namespace="/chat")
    async def chat2(sid, data):
        return "chat namespace", data

    @sio.on("ask")
    async def ask(sid, *args):
        async def answered(*reply):
            await sio.emit("answer received", list(reply), to=sid)
        await sio.emit("question", "ready?", to=sid, callback=answered)

    @sio.on("kick")
    async def kick(sid, *args):
        await sio.disconnect(sid)

    app = socketio.ASGIApp(sio)

    async def main():
        config = uvicorn.Config(app, host="127.0.0.1", port=0, log_level="warning", lifespan="off")
        srv = uvicorn.Server(config)
        task = asyncio.create_task(srv.serve())
        while not srv.started:
            await asyncio.sleep(0.01)
        print(json.dumps({"port": srv.servers[0].sockets[0].getsockname()[1]}), flush=True)
        await asyncio.get_running_loop().run_in_executor(None, sys.stdin.read)
        srv.should_exit = True
        await task

    asyncio.run(main())


if __name__ == "__main__":
    if sys.argv[1] == "client":
        client(sys.argv[2], len(sys.argv) > 3 and sys.argv[3] == "websocket-only")
    else:
        server()
