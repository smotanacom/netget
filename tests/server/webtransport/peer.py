"""aioquic 1.3.0 as an independent WebTransport (draft-02) peer for NetGet, unchanged library code.

  peer.py cert DIR                      write an ECDSA P-256 cert.pem/key.pem valid 7 days, print its sha-256
  peer.py client PORT CAFILE SCENARIO   open sessions against a server and print one JSON result
  peer.py server DIR                    serve WebTransport with DIR's certificate until stdin closes

Every line printed is one JSON object.
"""
import asyncio
import datetime
import hashlib
import ipaddress
import json
import ssl
import sys
from pathlib import Path

import aioquic
from aioquic.asyncio import QuicConnectionProtocol, connect, serve
from aioquic.h3.connection import H3Connection
from aioquic.h3.events import DatagramReceived, HeadersReceived, WebTransportStreamDataReceived
from aioquic.quic.configuration import QuicConfiguration
from aioquic.quic.events import (ConnectionTerminated, StopSendingReceived, StreamDataReceived,
                                 StreamReset)

assert aioquic.__version__ == "1.3.0", aioquic.__version__


def emit(**kw):
    print(json.dumps(kw), flush=True)


def make_cert(directory):
    from cryptography import x509
    from cryptography.hazmat.primitives import hashes, serialization
    from cryptography.hazmat.primitives.asymmetric import ec
    from cryptography.x509.oid import NameOID

    root = Path(directory)
    key = ec.generate_private_key(ec.SECP256R1())
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "localhost")])
    now = datetime.datetime.now(datetime.timezone.utc)
    cert = (
        x509.CertificateBuilder().subject_name(name).issuer_name(name)
        .public_key(key.public_key()).serial_number(x509.random_serial_number())
        .not_valid_before(now - datetime.timedelta(minutes=1))
        .not_valid_after(now + datetime.timedelta(days=7))
        .add_extension(x509.SubjectAlternativeName([
            x509.DNSName("localhost"),
            x509.IPAddress(ipaddress.ip_address("127.0.0.1")),
            x509.IPAddress(ipaddress.ip_address("::1")),
        ]), False)
        .add_extension(x509.BasicConstraints(ca=True, path_length=None), True)
        .sign(key, hashes.SHA256())
    )
    der = cert.public_bytes(serialization.Encoding.DER)
    (root / "cert.pem").write_bytes(cert.public_bytes(serialization.Encoding.PEM))
    (root / "key.pem").write_bytes(key.private_bytes(
        serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8,
        serialization.NoEncryption()))
    emit(sha256=hashlib.sha256(der).hexdigest())


class Peer(QuicConnectionProtocol):
    """Both roles: H3 with WebTransport, every event queued for the scenario to await."""

    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.http = H3Connection(self._quic, enable_webtransport=True)
        self.events = asyncio.Queue()
        self.streams = {}
        self.local = set()
        self.terminated = asyncio.get_running_loop().create_future()

    def quic_event_received(self, event):
        if isinstance(event, ConnectionTerminated) and not self.terminated.done():
            self.terminated.set_result({"code": event.error_code, "reason": event.reason_phrase})
        if isinstance(event, (StreamReset, StopSendingReceived)):
            self.events.put_nowait(("reset", event.stream_id, type(event).__name__))
        # aioquic's H3 layer does not mark the bidirectional WebTransport streams it opens, and
        # would parse the answers on them as HTTP/3 frames, so those are read here directly.
        if isinstance(event, StreamDataReceived) and event.stream_id in self.local:
            buf = self.streams.setdefault(event.stream_id, bytearray())
            buf.extend(event.data)
            if event.end_stream:
                self.events.put_nowait(("stream", event.stream_id, bytes(buf)))
            self.on_events()
            self.transmit()
            return
        for item in self.http.handle_event(event):
            if isinstance(item, HeadersReceived):
                self.events.put_nowait(("headers", item.stream_id,
                                        {k.decode(): v.decode() for k, v in item.headers}))
            elif isinstance(item, WebTransportStreamDataReceived):
                buf = self.streams.setdefault(item.stream_id, bytearray())
                buf.extend(item.data)
                if item.stream_ended:
                    self.events.put_nowait(("stream", item.stream_id, bytes(buf)))
            elif isinstance(item, DatagramReceived):
                self.events.put_nowait(("datagram", item.stream_id, item.data))
        self.on_events()
        self.transmit()

    def open_bi(self, session):
        stream = self.http.create_webtransport_stream(session)
        self.local.add(stream)
        return stream

    def on_events(self):
        pass

    async def next(self, kind, want=lambda sid, v: True, timeout=15):
        """The next queued event of this kind that matches; others are kept."""
        kept = []
        try:
            while True:
                k, sid, v = await asyncio.wait_for(self.events.get(), timeout)
                if k == kind and want(sid, v):
                    return sid, v
                kept.append((k, sid, v))
        finally:
            for e in kept:
                self.events.put_nowait(e)


# ---------------------------------------------------------------- client role

async def open_session(port, cafile, path, headers=()):
    config = QuicConfiguration(is_client=True, alpn_protocols=["h3"], max_datagram_frame_size=65536,
                               server_name="localhost")
    config.load_verify_locations(cafile)
    ctx = connect("127.0.0.1", port, configuration=config, create_protocol=Peer, wait_connected=True)
    peer = await ctx.__aenter__()
    sid = peer._quic.get_next_available_stream_id()
    peer.http.send_headers(sid, [
        (b":method", b"CONNECT"), (b":protocol", b"webtransport"), (b":scheme", b"https"),
        (b":authority", f"localhost:{port}".encode()), (b":path", path.encode()),
        (b"origin", b"https://peer.example"), (b"sec-webtransport-http3-draft02", b"1"),
        *headers,
    ])
    peer.transmit()
    _, response = await peer.next("headers", lambda s, v: s == sid)
    return ctx, peer, sid, response


async def bi(peer, session, data):
    stream = peer.open_bi(session)
    peer._quic.send_stream_data(stream, data, end_stream=True)
    peer.transmit()
    return stream


async def client(port, cafile, scenario):
    result = {}
    if scenario == "full":
        ctx, peer, session, response = await open_session(port, cafile, "/echo")
        result["status"] = response.get(":status")
        result["response_headers"] = response
        stream = await bi(peer, session, b"ping")
        _, reply = await peer.next("stream", lambda s, v: s == stream)
        result["bi"] = reply.decode()
        uni = peer.http.create_webtransport_stream(session, is_unidirectional=True)
        peer._quic.send_stream_data(uni, b"uni-hello", end_stream=True)
        peer.transmit()
        _, echoed = await peer.next("stream", lambda s, v: s % 4 == 3)
        result["uni"] = echoed.decode()
        peer.http.send_datagram(session, b"dg")
        peer.transmit()
        _, dg = await peer.next("datagram")
        result["datagram"] = dg.decode()
        # The handler answers "ask me" by opening a stream of its own; our answer comes back
        # as a datagram.
        await bi(peer, session, b"ask me")
        sid, question = await peer.next("stream", lambda s, v: s % 4 == 1)
        result["question"] = question.decode()
        peer._quic.send_stream_data(sid, b"answer!", end_stream=True)
        peer.transmit()
        _, dg = await peer.next("datagram")
        result["after_answer"] = dg.decode()
        # Binary data travels as hex both ways.
        stream = await bi(peer, session, bytes([0, 159, 255]))
        _, reply = await peer.next("stream", lambda s, v: s == stream)
        result["binary"] = reply.hex()
        # Past 1 MiB the stream is refused and its answer side reset.
        stream = await bi(peer, session, b"x" * (1024 * 1024 + 1))
        _, how = await peer.next("reset", lambda s, v: s == stream, timeout=30)
        result["oversized"] = how
        # An unanswered bidirectional stream is finished empty.
        stream = await bi(peer, session, b"silence")
        _, reply = await peer.next("stream", lambda s, v: s == stream)
        result["silent"] = reply.decode()
        await bi(peer, session, b"bye")
        result["closed"] = await asyncio.wait_for(peer.terminated, 15)
        await ctx.__aexit__(None, None, None)
    elif scenario.startswith("path:"):
        ctx, peer, session, response = await open_session(port, cafile, scenario[5:])
        result["status"] = response.get(":status")
        peer.close()
        await ctx.__aexit__(None, None, None)
    elif scenario == "get":
        # A plain HTTP/3 request is not a WebTransport session.
        config = QuicConfiguration(is_client=True, alpn_protocols=["h3"], max_datagram_frame_size=65536,
                                   server_name="localhost")
        config.load_verify_locations(cafile)
        async with connect("127.0.0.1", port, configuration=config, create_protocol=Peer) as peer:
            sid = peer._quic.get_next_available_stream_id()
            peer.http.send_headers(sid, [(b":method", b"GET"), (b":scheme", b"https"),
                                         (b":authority", b"localhost"), (b":path", b"/")], end_stream=True)
            peer.transmit()
            got = asyncio.ensure_future(peer.next("headers", lambda s, v: s == sid))
            done, _ = await asyncio.wait([got, peer.terminated], timeout=15,
                                         return_when=asyncio.FIRST_COMPLETED)
            if peer.terminated in done:
                result["terminated"] = peer.terminated.result()
            elif got in done:
                result["status"] = got.result()[1].get(":status")
            else:
                result["timeout"] = True
            got.cancel()
    emit(**result)


# ---------------------------------------------------------------- server role

class EchoServer(Peer):
    """/echo sessions: streams echoed with "echo:", uni streams answered on a new uni stream,
    datagrams echoed, and one stream opened to the client at start. Anything else is 403."""

    def on_events(self):
        while not self.events.empty():
            kind, sid, v = self.events.get_nowait()
            if kind == "headers" and v.get(":method") == "CONNECT":
                emit(request=v)
                if v.get(":path") != "/echo" or v.get(":protocol") != "webtransport":
                    self.http.send_headers(sid, [(b":status", b"403")], end_stream=True)
                    continue
                self.session = sid
                self.http.send_headers(sid, [(b":status", b"200"),
                                             (b"sec-webtransport-http3-draft", b"draft02")])
                self.asked = self.open_bi(sid)
                self._quic.send_stream_data(self.asked, b"server-question", end_stream=True)
            elif kind == "stream":
                emit(stream=sid, data=v.decode(errors="replace"))
                if sid == getattr(self, "asked", None):
                    emit(answer=v.decode(errors="replace"))
                elif sid % 4 == 0:
                    self._quic.send_stream_data(sid, b"echo:" + v, end_stream=True)
                elif sid % 4 == 2:
                    out = self.http.create_webtransport_stream(self.session, is_unidirectional=True)
                    self._quic.send_stream_data(out, b"echo:" + v, end_stream=True)
            elif kind == "datagram":
                emit(datagram=v.decode(errors="replace"))
                self.http.send_datagram(self.session, b"echo:" + v)
            elif kind == "reset":
                emit(reset=sid)

    def quic_event_received(self, event):
        if isinstance(event, ConnectionTerminated):
            emit(terminated={"code": event.error_code, "reason": event.reason_phrase})
        super().quic_event_received(event)


async def server(directory):
    config = QuicConfiguration(is_client=False, alpn_protocols=["h3"], idle_timeout=30,
                               max_datagram_frame_size=65536)
    config.load_cert_chain(str(Path(directory) / "cert.pem"), str(Path(directory) / "key.pem"))
    srv = await serve("127.0.0.1", 0, configuration=config, create_protocol=EchoServer)
    emit(ready=True, port=srv._transport.get_extra_info("sockname")[1])
    await asyncio.get_running_loop().run_in_executor(None, sys.stdin.read)
    srv.close()


if __name__ == "__main__":
    mode = sys.argv[1]
    if mode == "cert":
        make_cert(sys.argv[2])
    elif mode == "client":
        asyncio.run(client(int(sys.argv[2]), sys.argv[3], sys.argv[4]))
    elif mode == "server":
        asyncio.run(server(sys.argv[2]))
    else:
        sys.exit(f"unknown mode {mode}")
