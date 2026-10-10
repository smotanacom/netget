"""pyguacamole against NetGet's Guacamole server: handshake, read the first frame, type a
line, press Escape, click, send the clipboard, reading what the server draws after each.
One JSON object per line.

Usage: pyguac_session.py <host> <port> <username>
"""
import base64
import json
import struct
import sys

from guacamole.client import GuacamoleClient
from guacamole.instruction import GuacamoleInstruction as Instruction

host, port, user = sys.argv[1], int(sys.argv[2]), sys.argv[3]


def out(**kw):
    print(json.dumps(kw), flush=True)


c = GuacamoleClient(host, port, timeout=20)
try:
    c.handshake(protocol="vnc", width=800, height=600, dpi=96, image=["image/png"],
                hostname="desktop.example", port="5900", username=user, password="s3cret")
except Exception as e:  # pyguacamole raises on an error instead of ready
    out(step="refused", error=str(e))
    sys.exit(0)
out(step="ready", id=c.id)


def frame():
    """Instructions up to the next sync after something was drawn; answers the sync."""
    got = []
    while True:
        ins = c.read_instruction()
        if ins.opcode == "sync":
            c.send_instruction(Instruction("sync", ins.args[0]))
            if got:
                return got
            continue
        got.append(ins)


def describe(ins):
    ops, images, clip, data = [], [], "", {}
    for i in ins:
        ops.append(i.opcode)
        if i.opcode == "img":
            data[i.args[0]] = ("img", i.args[3], int(i.args[4]), int(i.args[5]), [])
        elif i.opcode == "clipboard":
            data[i.args[0]] = ("clip", i.args[1], 0, 0, [])
        elif i.opcode == "blob":
            data[i.args[0]][4].append(i.args[1])
        elif i.opcode == "end":
            kind, mime, x, y, parts = data.pop(i.args[0])
            raw = base64.b64decode("".join(parts))
            if kind == "img":
                assert raw[:8] == b"\x89PNG\r\n\x1a\n", "not a PNG"
                w, h = struct.unpack(">II", raw[16:24])
                images.append({"mimetype": mime, "x": x, "y": y, "width": w, "height": h})
            else:
                clip += raw.decode()
    rects = [i.args for i in ins if i.opcode == "rect"]
    fills = [i.args for i in ins if i.opcode == "cfill"]
    return {"ops": ops, "images": images, "clipboard": clip, "rects": rects, "fills": fills}


first = frame()
sizes = [i.args for i in first if i.opcode == "size"]
out(step="first", sizes=sizes, **describe(first))

for ch in "hi":
    c.send_instruction(Instruction("key", ord(ch), 1))
    c.send_instruction(Instruction("key", ord(ch), 0))
c.send_instruction(Instruction("key", 0xff08, 1))  # BackSpace takes the i back
c.send_instruction(Instruction("key", ord("o"), 1))
c.send_instruction(Instruction("key", 0xff0d, 1))
out(step="typed", **describe(frame()))

c.send_instruction(Instruction("key", 0xff1b, 1))
out(step="escape", **describe(frame()))

c.send_instruction(Instruction("mouse", 50, 60, 0))
c.send_instruction(Instruction("mouse", 50, 60, 1))
c.send_instruction(Instruction("mouse", 50, 60, 0))
out(step="click", **describe(frame()))

c.send_instruction(Instruction("clipboard", 5, "text/plain"))
c.send_instruction(Instruction("blob", 5, base64.b64encode("from python".encode()).decode()))
c.send_instruction(Instruction("end", 5))
out(step="clipboard", **describe(frame()))
c.close()
