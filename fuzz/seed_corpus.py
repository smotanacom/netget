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
this repository's eight stack overflows is in that class, so a corpus without depth in it
cannot find the next one.

With the guards in place the bombs are refused in microseconds and nothing downstream
sees them, so they cost the running fuzzer nothing. They exist for the day a guard
regresses.

This file is the provenance for 116 otherwise-opaque binary blobs; edit it rather than
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
# NATS does not nest either. Its stack overflow was `parse_frame` recursing once per blank line
# (8 KB of newlines, one `read`, killed the process); `blank_line_prefix_len` is now a loop.
# The blank-line run is that class's depth bomb: 32 Ki of them ahead of a PING.
write("nats_frame", "blank_line_bomb", NATS_PREFIX + b"\r\n" * 32768 + b"PING\r\n")

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
# STOMP does not nest: headers are a flat, MAX_HEADERS-bounded list and the inter-frame EOL
# drain is a loop. The recursion this framer once had was per blank line, so the equivalent of
# a depth bomb is a long run of them — 32 Ki heart-beats ahead of one frame. It costs the
# iterative drain nothing and is here for the day someone makes it recursive again.
write("stomp_frame", "blank_line_bomb", b"\r\n" * 32768 + b"SEND\ndestination:/q\n\nx\x00")

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
# Modbus has no nesting — an ADU is a flat header and a flat PDU, and no decoder here calls
# itself — so there is no depth bomb to plant. What a peer controls instead is every length
# it declares, and these seeds sit on each one. `max_adu` is exactly MAX_ADU_LEN (MBAP length
# 254); `over_max_adu` is the same frame one octet longer (length 255, refused);
# `declared_longer_than_sent` announces 254 and carries 5 (must ask for more, never read past
# the end); `zero_length` and `not_modbus` are the two framing refusals.
write("modbus_adu", "max_adu", mbap(6, 1, bytes([0x41]) + b"\xaa" * 252))
write("modbus_adu", "over_max_adu", mbap(7, 1, bytes([0x41]) + b"\xaa" * 253))
write("modbus_adu", "declared_longer_than_sent",
      struct.pack(">HHHB", 8, 0, 254, 1) + bytes([3]) + struct.pack(">HH", 0, 1))
write("modbus_adu", "zero_length", struct.pack(">HHHB", 9, 0, 0, 1))
write("modbus_adu", "not_modbus", struct.pack(">HHHB", 10, 7, 6, 1) + bytes([3, 0, 0, 0, 1]))
# The PDU-level lengths: each quantity limit at its maximum, and a write whose byte count
# disagrees with its quantity.
write("modbus_adu", "read_coils_max", mbap(11, 1, bytes([1]) + struct.pack(">HH", 0, 2000)))
write("modbus_adu", "read_registers_max",
      mbap(12, 1, bytes([3]) + struct.pack(">HH", 0xFFFF - 124, 125)))
write("modbus_adu", "write_coils_max",
      mbap(13, 1, bytes([0x0F]) + struct.pack(">HHB", 0, 1968, 246) + b"\x55" * 246))
write("modbus_adu", "write_registers_max",
      mbap(14, 1, bytes([0x10]) + struct.pack(">HHB", 0, 123, 246) + b"\x00\x01" * 123))
write("modbus_adu", "byte_count_mismatch",
      mbap(15, 1, bytes([0x10]) + struct.pack(">HHB", 0, 2, 3) + b"\x00\x0a\x01"))

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


# BGP path attributes do not nest in anything netgauze 0.7 decodes: every PathAttributeValue
# variant is a flat value or a flat list, and ATTR_SET (RFC 6368), the one attribute that would
# contain attributes, is not implemented. So there is no depth bomb; the lengths are what a
# peer controls, and the corpus had no UPDATE at all. These seed one ordinary UPDATE and one at
# BGP's 4096-octet maximum, carried by a long AS_PATH.
def bgp_attr(flags, code, value):
    if len(value) > 255:
        return bytes([flags | 0x10, code]) + struct.pack(">H", len(value)) + value
    return bytes([flags, code, len(value)]) + value


def bgp_update(attrs, nlri):
    body = struct.pack(">H", 0) + struct.pack(">H", len(attrs)) + attrs + nlri
    return MARKER + struct.pack(">HB", 19 + len(body), 2) + body


update_attrs = (bgp_attr(0x40, 1, b"\x00") +
                bgp_attr(0x40, 2, bytes([2, 2]) + struct.pack(">II", 65001, 65002)) +
                bgp_attr(0x40, 3, bytes([192, 0, 2, 1])))
write("bgp_message", "update_ipv4",
      b"\x01" + bgp_update(update_attrs, bytes([24, 198, 51, 100])))
# 4096 = header 19 + two length fields 4 + ORIGIN 4 + AS_PATH (4-byte extended header + P) +
# NEXT_HOP 7 + NLRI 4, so the AS_PATH value is P = 4054 octets: five AS_SEQUENCE segments
# (a segment holds at most 255 four-byte ASNs, and 2k + 4N = 4054 needs k odd) of 1011 ASNs.
as_path = b""
for n in (255, 255, 255, 245, 1):
    as_path += bytes([2, n]) + b"".join(struct.pack(">I", 64512 + i) for i in range(n))
assert len(as_path) == 4054
max_attrs = (bgp_attr(0x40, 1, b"\x00") + bgp_attr(0x40, 2, as_path) +
             bgp_attr(0x40, 3, bytes([192, 0, 2, 1])))
max_update = bgp_update(max_attrs, bytes([24, 198, 51, 100]))
assert len(max_update) == 4096
write("bgp_message", "update_max_len", b"\x01" + max_update)

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


# --- BSON: MongoDB command documents, and the shapes utils::bson_depth refuses ---
def bson_doc(elements):
    return struct.pack("<i", 4 + len(elements) + 1) + elements + b"\x00"


def bson_el(kind, key, value):
    return bytes([kind]) + key + b"\x00" + value


def bson_str(s):
    return struct.pack("<i", len(s) + 1) + s + b"\x00"


def bson_nested(levels):
    """`levels` documents in all, `{a: {a: ... {} ...}}`, built without quadratic copying."""
    sizes = [5 + 8 * i for i in range(levels)]  # innermost first
    head = b"".join(struct.pack("<i", sizes[i]) + b"\x03a\x00"
                    for i in range(levels - 1, 0, -1))
    return head + bson_doc(b"") + b"\x00" * (levels - 1)


write("bson_document", "hello", bson_doc(
    bson_el(0x10, b"hello", struct.pack("<i", 1)) + bson_el(0x02, b"$db", bson_str(b"admin"))))
write("bson_document", "find_with_filter", bson_doc(
    bson_el(0x02, b"find", bson_str(b"users")) +
    bson_el(0x03, b"filter", bson_doc(bson_el(0x03, b"age", bson_doc(
        bson_el(0x10, b"$gte", struct.pack("<i", 25)))))) +
    bson_el(0x02, b"$db", bson_str(b"test"))))
write("bson_document", "insert_array", bson_doc(
    bson_el(0x02, b"insert", bson_str(b"users")) +
    bson_el(0x04, b"documents", bson_doc(
        bson_el(0x03, b"0", bson_doc(bson_el(0x02, b"name", bson_str(b"Alice")))) +
        bson_el(0x03, b"1", bson_doc(bson_el(0x02, b"name", bson_str(b"Bob")))))) +
    bson_el(0x02, b"$db", bson_str(b"test"))))
write("bson_document", "every_type", bson_doc(
    bson_el(0x01, b"d", struct.pack("<d", 1.5)) +
    bson_el(0x05, b"bin", struct.pack("<i", 3) + b"\x00abc") +
    bson_el(0x06, b"u", b"") +
    bson_el(0x07, b"oid", bytes(range(12))) +
    bson_el(0x08, b"t", b"\x01") +
    bson_el(0x09, b"dt", struct.pack("<q", 0)) +
    bson_el(0x0A, b"n", b"") +
    bson_el(0x0B, b"re", b"^a\x00i\x00") +
    bson_el(0x0C, b"ptr", bson_str(b"db.c") + bytes(12)) +
    bson_el(0x0D, b"js", bson_str(b"x")) +
    bson_el(0x0E, b"sym", bson_str(b"s")) +
    bson_el(0x11, b"ts", struct.pack("<q", 1)) +
    bson_el(0x12, b"i64", struct.pack("<q", -1)) +
    bson_el(0x13, b"dec", bytes(16)) +
    bson_el(0x7F, b"max", b"") +
    bson_el(0xFF, b"min", b"")))
code = bson_str(b"f()")
scope = bson_doc(bson_el(0x10, b"x", struct.pack("<i", 1)))
write("bson_document", "code_with_scope", bson_doc(
    bson_el(0x0F, b"cws", struct.pack("<i", 4 + len(code) + len(scope)) + code + scope)))
write("bson_document", "at_depth_limit", bson_nested(64))
# A document declaring 2 GiB: bson's reader_to_vec would reserve that before finding out.
write("bson_document", "huge_declared_len", struct.pack("<i", 0x7FFFFFFF) + b"\x00")
# 16,384 levels at eight bytes each (~128 KiB): past the 4,861 that overflow the fuzzer's
# 8 MiB main thread in a release build when the guard is removed (verified).
write("bson_document", "depth_bomb", bson_nested(16384))


# --- LDAP: SearchRequests, and the filter nesting MAX_FILTER_DEPTH bounds ---
def ber_len(n):
    if n < 0x80:
        return bytes([n])
    if n < 0x100:
        return bytes([0x81, n])
    if n < 0x10000:
        return b"\x82" + struct.pack(">H", n)
    return b"\x83" + n.to_bytes(3, "big")


def tlv(tag, value):
    return bytes([tag]) + ber_len(len(value)) + value


def ldap_search(filter_bytes, msg_id=1):
    body = (tlv(0x04, b"dc=example,dc=com") + tlv(0x0A, b"\x02") + tlv(0x0A, b"\x00") +
            tlv(0x02, b"\x00") + tlv(0x02, b"\x00") + tlv(0x01, b"\x00") +
            filter_bytes + tlv(0x30, tlv(0x04, b"cn") + tlv(0x04, b"mail")))
    return tlv(0x30, tlv(0x02, bytes([msg_id])) + tlv(0x63, body))


def ldap_nested_and(levels, inner):
    """`levels` nested `&` filters around `inner`, built without quadratic copying."""
    headers = []
    size = len(inner)
    for _ in range(levels):
        header = b"\xa0" + ber_len(size)
        headers.append(header)
        size += len(header)
    return b"".join(reversed(headers)) + inner


ldap_eq = tlv(0xA3, tlv(0x04, b"uid") + tlv(0x04, b"alice"))
write("ldap_filter", "search_equality", ldap_search(ldap_eq))
write("ldap_filter", "search_present", ldap_search(tlv(0x87, b"objectClass")))
write("ldap_filter", "search_and_or_not", ldap_search(tlv(0xA0,
      ldap_eq + tlv(0xA1, tlv(0xA3, tlv(0x04, b"ou") + tlv(0x04, b"eng")) +
                    tlv(0xA2, tlv(0x87, b"disabled"))))))
write("ldap_filter", "search_substrings", ldap_search(tlv(0xA4,
      tlv(0x04, b"cn") + tlv(0x30, tlv(0x80, b"al") + tlv(0x81, b"ic") + tlv(0x82, b"e")))))
write("ldap_filter", "bind_simple",
      tlv(0x30, tlv(0x02, b"\x01") + tlv(0x60, tlv(0x02, b"\x03") + tlv(0x04, b"cn=admin") +
                                         tlv(0x80, b"secret"))))
# render_filter stops at depth 32: the equality inside 32 `&`s is the first thing it elides.
write("ldap_filter", "at_depth_limit", ldap_search(ldap_nested_and(32, ldap_eq)))
# 60,000 levels at ~5 bytes each (~300 KiB, under the 1 MiB MAX_LDAP_MESSAGE, so it is framed
# and reaches the renderer): with the depth check removed this overflows the fuzzer's 8 MiB
# main thread (verified).
write("ldap_filter", "depth_bomb", ldap_search(ldap_nested_and(60000, ldap_eq)))


# --- ra_svn: tuples, and the nesting MAX_TUPLE_DEPTH bounds ---
write("svn_tuple", "client_greeting",
      b"( 2 ( edit-pipeline svndiff1 accepts-svndiff2 absent-entries depth mergeinfo log-revprops"
      b" ) 32:svn://127.0.0.1/repo/trunk/proj 10:SVN/1.14.2 ( ) ) ")
write("svn_tuple", "auth_anonymous", b"( ANONYMOUS ( 0: ) ) ")
write("svn_tuple", "get_latest_rev", b"( get-latest-rev ( ) ) ")
write("svn_tuple", "get_dir", b"( get-dir ( 0: ( ) true false ( kind size ) ) ) ")
write("svn_tuple", "counted_string_with_newline", b"( check-path ( 5:a\nb c ( 12 ) ) ) ")
write("svn_tuple", "two_commands", b"( get-latest-rev ( ) ) ( stat ( 0: ( ) ) ) ")
# The reader refuses a 65th open list, so 64 closed lists is the deepest item it accepts.
write("svn_tuple", "at_depth_limit", b"(" * 64 + b")" * 64)
# 32,000 closed lists: two bytes a level, so it fits MAX_COMMAND_BYTES (64 KiB) and is a size
# the server really admits. The reader is iterative, but the Item it builds is walked
# recursively (Display, to_json, Drop); with MAX_TUPLE_DEPTH removed this overflows the
# fuzzer's 8 MiB main thread (verified).
write("svn_tuple", "depth_bomb", b"(" * 32000 + b")" * 32000)


# --- XML-RPC: methodCalls, and the nesting MAX_VALUE_DEPTH bounds ---
def xmlrpc_call(params):
    return (b'<?xml version="1.0"?><methodCall><methodName>examples.getStateName</methodName>'
            b"<params>" + b"".join(b"<param>" + p + b"</param>" for p in params) +
            b"</params></methodCall>")


def xmlrpc_nested_array(levels, inner):
    return (b"<value><array><data>" * levels + inner +
            b"</data></array></value>" * levels)


write("xmlrpc_value", "int_param", xmlrpc_call([b"<value><i4>41</i4></value>"]))
write("xmlrpc_value", "every_scalar", xmlrpc_call([
    b"<value><int>-7</int></value>", b"<value><i8>9007199254740993</i8></value>",
    b"<value><boolean>1</boolean></value>", b"<value><string>a &amp; b</string></value>",
    b"<value><double>2.5</double></value>",
    b"<value><dateTime.iso8601>19980717T14:08:55</dateTime.iso8601></value>",
    b"<value><base64>aGVsbG8=</base64></value>", b"<value><nil/></value>",
    b"<value>untyped</value>", b"<value/>"]))
write("xmlrpc_value", "struct_and_array", xmlrpc_call([
    b"<value><struct><member><name>id</name><value><int>1</int></value></member>"
    b"<member><name>tags</name><value><array><data><value>a</value><value>b</value>"
    b"</data></array></value></member></struct></value>"]))
# 31 levels of <value><array> plus the innermost <value> is 63 frames, one under the guard.
write("xmlrpc_value", "at_depth_limit",
      xmlrpc_call([xmlrpc_nested_array(31, b"<value><i4>1</i4></value>")]))
# 20,000 closed levels at 42 bytes each (~860 KB): the parser is iterative, but the
# XmlRpcValue it builds is walked recursively (to JSON, and on drop); with MAX_VALUE_DEPTH
# removed this overflows the fuzzer's 8 MiB main thread (verified; 16,000 already does).
# It must stay under 1 MiB: libFuzzer caps the -max_len it infers from a corpus at 1 MiB and
# silently TRUNCATES larger seeds, so a 40,000-level bomb (1.7 MB) arrived as malformed XML,
# never reached the recursion, and ran 60 seconds "clean" with the guard removed.
write("xmlrpc_value", "depth_bomb",
      xmlrpc_call([xmlrpc_nested_array(20000, b"<value><i4>1</i4></value>")]))

total =sum(len(files) for _, _, files in os.walk(CORPUS))
print("seeded %d corpus files across %d targets" %
      (total, len(os.listdir(CORPUS))))
