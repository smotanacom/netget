#!/usr/bin/env python3
"""Regenerate `fuzz/corpus/` — the committed seed inputs for every fuzz target.

Run as: `python3 fuzz/seed_corpus.py .` from the repository root.

Two kinds of seed live here, and the second is the one worth understanding.

**Valid messages.** A fuzzer that starts from random bytes spends its whole budget
rediscovering the header of every protocol. These are the shapes the real decoders
accept, from the protocols' own tests and their RFCs, so libFuzzer starts at the edges
that matter instead of at byte zero.

**Depth bombs**, for the decoders that have a nesting guard. Coverage-guided
fuzzing gives *no gradient toward depth*: a value nested 10,000 deep executes exactly
the same basic blocks as one nested 3 deep, so libFuzzer scores it as uninteresting and
throws it away. It will not grow one on its own. This was measured, not assumed — with
`utils::bencode`'s guard removed and no deep seed, 300 seconds and 15.5M executions
found nothing; with the bomb seeded, the same build died in 2.8 seconds. Every one of
this repository's seven stack overflows is in that class, so a corpus without depth in it
cannot find the next one.

With the guards in place the bombs are refused in microseconds and nothing downstream
sees them, so they cost the running fuzzer nothing. They exist for the day a guard
regresses.

This file is the provenance for 74 otherwise-opaque binary blobs; edit it rather than
the blobs.
"""
import os
import struct
import sys

ROOT = sys.argv[1]
CORPUS = os.path.join(ROOT, "fuzz", "corpus")


def write(target, name, data):
    d = os.path.join(CORPUS, target)
    os.makedirs(d, exist_ok=True)
    with open(os.path.join(d, name), "wb") as f:
        f.write(data)


# --- bencode: KRPC ping, and the shapes the guard has to get right ---------
write("bencode_structure", "krpc_ping",
      b"d1:ad2:id20:abcdefghij0123456789e1:q4:ping1:t2:aa1:y1:qe")
write("bencode_structure", "krpc_find_node",
      b"d1:ad2:id20:abcdefghij01234567896:target20:mnopqrstuvwxyz123456e"
      b"1:q9:find_node1:t2:aa1:y1:qe")
write("bencode_structure", "int", b"i42e")
write("bencode_structure", "negative_int", b"i-1e")
write("bencode_structure", "list", b"l4:spam4:eggse")
write("bencode_structure", "nested", b"lli1eeli2eee")
write("bencode_structure", "empty_string", b"0:")
write("bencode_structure", "at_depth_limit", b"l" * 32 + b"e" * 32)

# A depth bomb, deliberately committed as a seed. With the guard in place it is refused
# in microseconds and nothing downstream sees it, so it costs the fuzzer nothing. Its
# purpose is the day the guard regresses: coverage-guided fuzzing gives **no gradient
# toward nesting depth** — a deeper value exercises exactly the same basic blocks, so
# libFuzzer discards it as uninteresting and will not grow one on its own. Measured here:
# with the guard removed and no deep seed, 300 seconds and 15.5M executions found nothing;
# with this seed present the crash is immediate. A depth-class bug is found only if the
# corpus already contains depth.
#
# It also sets libFuzzer's default -max_len (which it infers from the largest corpus
# entry), so the depth needed to overflow an 8 MiB main thread is reachable at all —
# under the 4096-byte default it is not. Note the module doc's "9,215 levels" figure is
# from a debug build; measured against this optimised fuzz build 12,000 levels survived
# and 20,000 died, so the bomb is sized well past either.
write("bencode_structure", "depth_bomb", b"l" * 32768)

# --- SNMP: v2c GetRequest for sysDescr.0 (1.3.6.1.2.1.1.1.0) --------------
def ber(tag, payload):
    if len(payload) < 0x80:
        return bytes([tag, len(payload)]) + payload
    ln = len(payload).to_bytes((len(payload).bit_length() + 7) // 8, "big")
    return bytes([tag, 0x80 | len(ln)]) + ln + payload


oid_sysdescr = bytes([0x2B, 6, 1, 2, 1, 1, 1, 0])  # 1.3 packed, then the rest
varbind = ber(0x30, ber(0x06, oid_sysdescr) + ber(0x05, b""))
varbinds = ber(0x30, varbind)
pdu = ber(0xA0, ber(0x02, b"\x00\x00\x00\x01") + ber(0x02, b"\x00") +
          ber(0x02, b"\x00") + varbinds)
for community in (b"public", b"private"):
    msg = ber(0x30, ber(0x02, b"\x01") + ber(0x04, community) + pdu)
    write("snmp_ber", "getrequest_" + community.decode(), msg)
write("snmp_ber", "at_depth_limit", b"".join(bytes([0x30, 0x80]) for _ in range(15)) +
      b"\x05\x00" + b"\x00\x00" * 15)
write("snmp_ber", "depth_bomb",
      b"".join(bytes([0x30, 0x80]) for _ in range(16384)))

# --- AMQP field table: {"product": S"NetGet", "copy": t True} -------------
def amqp_shortstr(s):
    return bytes([len(s)]) + s


def amqp_longstr(s):
    return struct.pack(">I", len(s)) + s


entries = (amqp_shortstr(b"product") + b"S" + amqp_longstr(b"NetGet") +
           amqp_shortstr(b"copy") + b"t" + b"\x01" +
           amqp_shortstr(b"count") + b"I" + struct.pack(">i", 7))
write("amqp_field_table", "client_properties", struct.pack(">I", len(entries)) + entries)
nested_inner = amqp_shortstr(b"a") + b"s" + amqp_shortstr(b"b")
nested = amqp_shortstr(b"caps") + b"F" + struct.pack(">I", len(nested_inner)) + nested_inner
write("amqp_field_table", "nested_table", struct.pack(">I", len(nested)) + nested)
write("amqp_field_table", "empty", struct.pack(">I", 0))

# Nested AMQP field tables cost the peer five bytes per level, which is what made one
# 128 KiB frame worth ~26,000 levels before MAX_FIELD_TABLE_DEPTH existed.
def amqp_nest(levels):
    inner = struct.pack(">I", 0)
    for _ in range(levels):
        body = amqp_shortstr(b"n") + b"F" + inner
        inner = struct.pack(">I", len(body)) + body
    return inner


write("amqp_field_table", "depth_bomb", amqp_nest(6000))

# --- NATS: the target's first byte is max_payload, the rest is the wire ----
NATS_PREFIX = b"\x40"  # 0x40 << 24 = 1 GiB payload bound
for name, frame in [
    ("connect", b"CONNECT {\"verbose\":false}\r\n"),
    ("ping", b"PING\r\n"),
    ("sub", b"SUB foo.bar 1\r\n"),
    ("pub", b"PUB foo.bar 5\r\nhello\r\n"),
    ("hpub", b"HPUB foo 22 27\r\nNATS/1.0\r\nX-A: b\r\n\r\nhello\r\n"),
    ("unsub", b"UNSUB 1 5\r\n"),
]:
    write("nats_frame", name, NATS_PREFIX + frame)

# --- STOMP ----------------------------------------------------------------
write("stomp_frame", "connect",
      b"CONNECT\naccept-version:1.2\nhost:localhost\n\n\x00")
write("stomp_frame", "send_with_len",
      b"SEND\ndestination:/queue/a\ncontent-length:5\n\nhello\x00")
write("stomp_frame", "send_no_len", b"SEND\ndestination:/queue/a\n\nhello\x00")
write("stomp_frame", "subscribe",
      b"SUBSCRIBE\nid:0\ndestination:/queue/a\nack:client\n\n\x00")
write("stomp_frame", "escaped_header",
      b"SEND\ndestination:/queue/a\nx\\ckey:v\\nal\n\n\x00")
write("stomp_frame", "heartbeat", b"\n")

# --- RADIUS: Access-Request with User-Name and User-Password --------------
attrs = (bytes([1, 2 + 5]) + b"alice" +
         bytes([2, 2 + 16]) + bytes(range(16)) +
         bytes([4, 6]) + bytes([127, 0, 0, 1]) +
         bytes([5, 6]) + struct.pack(">I", 1))
pkt = bytes([1, 42]) + struct.pack(">H", 20 + len(attrs)) + bytes(range(16, 32)) + attrs
write("radius_packet", "access_request", pkt)
acct = (bytes([40, 6]) + struct.pack(">I", 1) + bytes([1, 2 + 5]) + b"alice")
write("radius_packet", "accounting_request",
      bytes([4, 7]) + struct.pack(">H", 20 + len(acct)) + bytes(16) + acct)

# --- DNS: a standard A query for example.com ------------------------------
def dns_name(host):
    out = b""
    for label in host.split(b"."):
        out += bytes([len(label)]) + label
    return out + b"\x00"


q = (struct.pack(">HHHHHH", 0x1234, 0x0100, 1, 0, 0, 0) +
     dns_name(b"example.com") + struct.pack(">HH", 1, 1))
write("dns_message", "a_query", q)
resp = (struct.pack(">HHHHHH", 0x1234, 0x8180, 1, 1, 0, 0) +
        dns_name(b"example.com") + struct.pack(">HH", 1, 1) +
        b"\xc0\x0c" + struct.pack(">HHIH", 1, 1, 300, 4) + bytes([93, 184, 216, 34]))
write("dns_message", "a_response", resp)
write("dns_message", "compression_pointer_loop",
      struct.pack(">HHHHHH", 1, 0x0100, 1, 0, 0, 0) + b"\xc0\x0c" +
      struct.pack(">HH", 1, 1))

# --- LLDP: chassis id + port id + TTL + end ------------------------------
def lldp_tlv(t, v):
    return struct.pack(">H", (t << 9) | len(v)) + v


lldpdu = (lldp_tlv(1, b"\x04" + bytes([0, 0x0C, 0x29, 1, 2, 3])) +
          lldp_tlv(2, b"\x05" + b"eth0") +
          lldp_tlv(3, struct.pack(">H", 120)) +
          lldp_tlv(5, b"netget") +
          lldp_tlv(0, b""))
write("lldp_frame", "lldpdu", lldpdu)
write("lldp_frame", "full_frame",
      b"\x01\x80\xc2\x00\x00\x0e" + bytes([0, 0x0C, 0x29, 1, 2, 3]) +
      b"\x88\xcc" + lldpdu)

# --- CDP: version/ttl/checksum + device-id TLV ---------------------------
def cdp_tlv(t, v):
    return struct.pack(">HH", t, len(v) + 4) + v


cdp_body = cdp_tlv(1, b"switch1") + cdp_tlv(3, b"Ethernet0/1") + cdp_tlv(5, b"IOS")
cdp_payload = bytes([2, 180]) + b"\x00\x00" + cdp_body
write("cdp_frame", "payload", cdp_payload)
write("cdp_frame", "full_frame",
      b"\x01\x00\x0c\xcc\xcc\xcc" + bytes([0, 0x0C, 0x29, 1, 2, 3]) +
      struct.pack(">H", len(cdp_payload) + 8) +
      b"\xaa\xaa\x03\x00\x00\x0c\x20\x00" + cdp_payload)

# --- Modbus: MBAP + read holding registers / write single ---------------
def mbap(tid, unit, pdu):
    return struct.pack(">HHHB", tid, 0, len(pdu) + 1, unit) + pdu


write("modbus_adu", "read_holding", mbap(1, 1, bytes([3]) + struct.pack(">HH", 0, 10)))
write("modbus_adu", "read_coils", mbap(2, 1, bytes([1]) + struct.pack(">HH", 0, 8)))
write("modbus_adu", "write_single", mbap(3, 1, bytes([6]) + struct.pack(">HH", 1, 0xABCD)))
write("modbus_adu", "two_adus",
      mbap(4, 1, bytes([3]) + struct.pack(">HH", 0, 1)) +
      mbap(5, 1, bytes([3]) + struct.pack(">HH", 1, 1)))

# --- CoAP: confirmable GET /.well-known/core ----------------------------
def coap_opt(delta, value):
    ln = len(value)
    assert delta < 13 and ln < 13
    return bytes([(delta << 4) | ln]) + value


get = (bytes([0x40 | 0x02, 0x01]) + struct.pack(">H", 0x1234) + b"\xab\xcd" +
       coap_opt(11, b".well-known") + coap_opt(0, b"core"))
write("coap_message", "get_wellknown", get)
write("coap_message", "empty_ack", bytes([0x60, 0x00]) + struct.pack(">H", 0x1234))
post = (bytes([0x40 | 0x02, 0x02]) + struct.pack(">H", 1) + b"\x01\x02" +
        coap_opt(11, b"sensor") + b"\xff" + b"23.5")
write("coap_message", "post_payload", post)

# CoAP has no nesting, so it has no depth bomb — the equivalent blind spot is the
# option delta/length *extension* encodings, and none of the three seeds above reaches
# one. Nibble 13 means "one more byte, +13"; nibble 14 means "two more bytes, +269", and
# `read_extended` saturates that addition. Every length a peer can state that the walker
# must then not run past lives behind those two nibbles, so without a seed here a
# coverage-guided run explores only the 0-12 forms and reports clean for the encoding
# that actually carries the arithmetic.
def coap_opt_ext(delta, value):
    """One option using the 13 form for the delta and, past 12 octets, for the length."""
    ln = len(value)
    assert 13 <= delta < 269 and 13 <= ln < 269
    return bytes([(13 << 4) | 13, delta - 13, ln - 13]) + value


ext13 = (bytes([0x40 | 0x02, 0x01]) + struct.pack(">H", 0x4711) + b"\xde\xad\xbe\xef" +
         coap_opt_ext(60, b"x" * 40))
write("coap_message", "option_delta_ext13", ext13)
# The 14 form on both nibbles: delta 600 (-> 331 in two bytes) and a 300-octet value.
ext14 = (bytes([0x40 | 0x02, 0x01]) + struct.pack(">H", 0x4712) + b"\xde\xad\xbe\xef" +
         bytes([(14 << 4) | 14]) + struct.pack(">H", 600 - 269) +
         struct.pack(">H", 300 - 269) + b"y" * 300)
write("coap_message", "option_delta_ext14", ext14)

# --- HSRP v1 hello and a v2 TLV ----------------------------------------
v1 = (bytes([0, 0, 3, 1, 100, 1, 10]) + b"cisco\x00\x00\x00" +
      bytes([192, 168, 1, 1]))
write("hsrp_message", "v1_hello", v1)
v2_body = (bytes([4, 0]) + bytes([1, 100, 1, 10]) + struct.pack(">H", 1) +
           bytes([0, 0x0C, 0x29, 1, 2, 3]) + bytes([192, 168, 1, 1]))
write("hsrp_message", "v2_hello", bytes([1, len(v2_body)]) + v2_body)

# --- M3UA: DATA and ASPUP ----------------------------------------------
def m3ua_param(tag, value):
    pad = (-len(value)) % 4
    return struct.pack(">HH", tag, len(value) + 4) + value + b"\x00" * pad


protocol_data = (struct.pack(">II", 1, 2) + bytes([3, 0, 0, 0]) + b"payload")
data_params = m3ua_param(0x0210, protocol_data)
write("m3ua_message", "data",
      bytes([1, 0, 1, 1]) + struct.pack(">I", 8 + len(data_params)) + data_params)
aspup = m3ua_param(0x0011, struct.pack(">I", 1))
write("m3ua_message", "aspup",
      bytes([1, 0, 3, 1]) + struct.pack(">I", 8 + len(aspup)) + aspup)

# --- BGP: first byte is asn4, then marker+length+type -------------------
MARKER = b"\xff" * 16
keepalive = MARKER + struct.pack(">HB", 19, 4)
write("bgp_message", "keepalive", b"\x01" + keepalive)
open_body = (bytes([4]) + struct.pack(">HH", 65001, 180) +
             bytes([192, 168, 1, 1]) + bytes([0]))
write("bgp_message", "open",
      b"\x01" + MARKER + struct.pack(">HB", 19 + len(open_body), 1) + open_body)
notification = MARKER + struct.pack(">HB", 21, 3) + bytes([6, 0])
write("bgp_message", "notification", b"\x00" + notification)

# --- NDEF: a text record and a URI record ------------------------------
text_payload = bytes([2]) + b"en" + b"hello"
write("ndef_message", "text_record",
      bytes([0xD1, 1, len(text_payload)]) + b"T" + text_payload)
uri_payload = bytes([0x03]) + b"netget.net"
write("ndef_message", "uri_record",
      bytes([0xD1, 1, len(uri_payload)]) + b"U" + uri_payload)
write("ndef_message", "two_records",
      bytes([0x91, 1, len(text_payload)]) + b"T" + text_payload +
      bytes([0x51, 1, len(uri_payload)]) + b"U" + uri_payload)

# --- ISO 7816 APDUs ----------------------------------------------------
write("nfc_apdu", "select_ndef", bytes([0x00, 0xA4, 0x04, 0x00, 0x07]) +
      bytes([0xD2, 0x76, 0x00, 0x00, 0x85, 0x01, 0x01]) + bytes([0x00]))
write("nfc_apdu", "read_binary", bytes([0x00, 0xB0, 0x00, 0x00, 0x0F]))
write("nfc_apdu", "case1", bytes([0x00, 0x20, 0x00, 0x00]))
write("nfc_apdu", "extended",
      bytes([0x00, 0xB0, 0x00, 0x00, 0x00, 0x01, 0x00]) + bytes(256))

# --- NFS record markers: last-fragment bit is the high bit --------------
LAST = 0x80000000
write("nfs_record_guard", "single_small", struct.pack(">I", LAST | 100))
write("nfs_record_guard", "two_fragments",
      struct.pack(">I", 1024) + struct.pack(">I", LAST | 1024))
write("nfs_record_guard", "many_fragments",
      b"".join(struct.pack(">I", 512) for _ in range(8)) +
      struct.pack(">I", LAST | 512))
write("nfs_record_guard", "oversized_announce", struct.pack(">I", LAST | 0x7FFFFFFF))


# --- RESP2: what redis-cli and redis-rs send, and the shapes utils::resp refuses ---
def resp_command(*args):
    out = b"*%d\r\n" % len(args)
    for a in args:
        out += b"$%d\r\n%s\r\n" % (len(a), a)
    return out


write("resp_frame", "ping", resp_command(b"PING"))
write("resp_frame", "set", resp_command(b"SET", b"key", b"value"))
write("resp_frame", "pipelined", resp_command(b"PING") + resp_command(b"GET", b"k"))
write("resp_frame", "reply_shapes",
      b"*5\r\n+OK\r\n:42\r\n$-1\r\n*-1\r\n*2\r\n$0\r\n\r\n-ERR no\r\n")
write("resp_frame", "incomplete_bulk", b"*1\r\n$10\r\nabc")
write("resp_frame", "at_depth_limit", b"*1\r\n" * 32 + b":1\r\n")
# A declared length the 64 MiB buffer can never satisfy, refused on the header line.
write("resp_frame", "huge_declared_len", b"*4000000000\r\n")
write("resp_frame", "huge_declared_bulk", b"*1\r\n$4000000000\r\n")
# 65,536 levels at four bytes each: enough to overflow the fuzzer's 8 MiB main thread
# when the guard is removed (verified), and the input that sets libFuzzer's -max_len.
write("resp_frame", "depth_bomb", b"*1\r\n" * 65536 + b":1\r\n")

total = sum(len(files) for _, _, files in os.walk(CORPUS))
print("seeded %d corpus files across %d targets" %
      (total, len(os.listdir(CORPUS))))
