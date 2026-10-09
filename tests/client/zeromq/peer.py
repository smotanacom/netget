"""pyzmq 27.2.0 (libzmq 4.3.5), unchanged, driven against NetGet. Every mode prints JSON lines.

  peer.py req ADDR FRAME...        one request of these frames; prints the reply
  peer.py dealer ADDR IDENTITY     DEALER with an Identity: sends ["ping"], prints the reply
  peer.py push ADDR MSG...         one single-frame message per MSG
  peer.py rep|router|pull|pub      bind 127.0.0.1 on a free port, print {"ready": ADDR}, serve
"""
import json, sys, zmq

ctx = zmq.Context()
mode = sys.argv[1]
out = lambda v: print(json.dumps(v), flush=True)
text = lambda frames: [f.decode() for f in frames]

def connect(kind, addr, identity=None):
    s = ctx.socket(kind)
    s.setsockopt(zmq.LINGER, 2000)
    s.setsockopt(zmq.RCVTIMEO, 20000)
    if identity:
        s.setsockopt(zmq.IDENTITY, identity.encode())
    s.connect("tcp://" + addr)
    return s

def bind(kind):
    s = ctx.socket(kind)
    port = s.bind_to_random_port("tcp://127.0.0.1")
    out({"ready": "127.0.0.1:%d" % port})
    return s

if mode == "req":
    s = connect(zmq.REQ, sys.argv[2])
    s.send_multipart([f.encode() for f in sys.argv[3:]])
    out({"reply": text(s.recv_multipart())})
elif mode == "dealer":
    s = connect(zmq.DEALER, sys.argv[2], sys.argv[3])
    s.send_multipart([b"ping"])
    out({"reply": text(s.recv_multipart())})
elif mode == "push":
    s = connect(zmq.PUSH, sys.argv[2])
    for m in sys.argv[3:]:
        s.send(m.encode())
    s.close()
    out({"pushed": len(sys.argv) - 3})
elif mode == "rep":
    s = bind(zmq.REP)
    while True:
        frames = s.recv_multipart()
        s.send_multipart([b"ECHO"] + frames)
        out({"got": text(frames)})
elif mode == "router":
    s = bind(zmq.ROUTER)
    while True:
        identity, *frames = s.recv_multipart()
        s.send_multipart([identity] + frames + [b"routed to " + identity])
        out({"identity": identity.decode(), "got": text(frames)})
elif mode == "pull":
    s = bind(zmq.PULL)
    while True:
        out({"got": text(s.recv_multipart())})
elif mode == "pub":
    # XPUB sees each subscription, so it publishes only once the subscriber is listening.
    s = bind(zmq.XPUB)
    while True:
        sub = s.recv()
        out({"subscription": sub[:1].hex(), "topic": sub[1:].decode()})
        if sub[:1] == b"\x01":
            for frames in ([b"sports", b"goal"], [b"weather", b"sunny"], [b"weather", b"rain"]):
                s.send_multipart(frames)
else:
    sys.exit("unknown mode " + mode)
