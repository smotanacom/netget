"""Independent aioquic 1.3.0 integration peer for HTTP/3 and raw QUIC.

Library owns TLS, QUIC, HTTP/3 and QPACK. The wrapper only maps test requests to
its public APIs. All network addresses are loopback and all bodies are bounded.
"""
import argparse
import asyncio
import json
from aioquic.asyncio import QuicConnectionProtocol, connect, serve
from aioquic.h3.connection import H3Connection
from aioquic.h3.events import HeadersReceived, DataReceived
from aioquic.quic.configuration import QuicConfiguration
from aioquic.quic.events import StreamDataReceived, ConnectionTerminated

MAX_BODY = 8 * 1024 * 1024

class H3Peer(QuicConnectionProtocol):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.http = H3Connection(self._quic)
        self.is_client = self._quic.configuration.is_client
        self.messages = {}
        self.waiters = {}

    def quic_event_received(self, event):
        if isinstance(event, ConnectionTerminated):
            for waiter in self.waiters.values():
                if not waiter.done():
                    waiter.set_exception(ConnectionError(event.reason_phrase))
        for http_event in self.http.handle_event(event):
            if not isinstance(http_event, (HeadersReceived, DataReceived)):
                continue
            message = self.messages.setdefault(http_event.stream_id, {"headers": [], "body": bytearray()})
            if isinstance(http_event, HeadersReceived):
                message["headers"].extend(http_event.headers)
            else:
                message["body"].extend(http_event.data)
            if len(message["body"]) > MAX_BODY:
                self._quic.reset_stream(http_event.stream_id, 0x107)
                continue
            if http_event.stream_ended:
                self.messages.pop(http_event.stream_id)
                if self.is_client:
                    future = self.waiters.pop(http_event.stream_id)
                    future.set_result({"stream_id": http_event.stream_id,
                                       "headers": [(k.decode(), v.decode()) for k, v in message["headers"]],
                                       "body": message["body"].decode()})
                else:
                    headers = dict((k.decode(), v.decode()) for k, v in message["headers"])
                    if headers.get(":path") == "/silent":
                        continue
                    if headers.get(":path") == "/oversized":
                        self.http.send_headers(http_event.stream_id, [(b":status", b"200")])
                        self.http.send_data(http_event.stream_id, b"x" * (MAX_BODY + 1), end_stream=True)
                        self.transmit()
                        continue
                    if headers.get(":path") == "/large-header":
                        self.http.send_headers(http_event.stream_id, [(b":status", b"200")] + [(b"accept", b"*/*") for i in range(900)], end_stream=True)
                        self.transmit()
                        continue
                    if headers.get(":path") in ("/te-response", "/te-trailer"):
                        bad = [(b"te", b"trailers")]
                        self.http.send_headers(http_event.stream_id, [(b":status", b"200")] + (bad if headers[":path"] == "/te-response" else []))
                        if headers[":path"] == "/te-trailer":
                            self.http.send_headers(http_event.stream_id, bad, end_stream=True)
                        else:
                            self.http.send_data(http_event.stream_id, b"", end_stream=True)
                        self.transmit()
                        continue
                    body = json.dumps({"method": headers.get(":method"), "path": headers.get(":path"),
                                       "headers": headers, "body": message["body"].decode()}).encode()
                    self.http.send_headers(http_event.stream_id, [(b":status", b"200"),
                       (b"content-type", b"application/json"), (b"x-peer", b"aioquic"),
                       (b"content-length", str(len(body)).encode())])
                    self.http.send_data(http_event.stream_id, body, end_stream=True)
                    self.transmit()

    async def request(self, request):
        stream = self._quic.get_next_available_stream_id()
        future = asyncio.get_running_loop().create_future()
        self.waiters[stream] = future
        body = request.get("body", "").encode()
        headers = [(b":method", request.get("method", "GET").encode()),
                   (b":scheme", b"https"), (b":authority", b"localhost"),
                   (b":path", request.get("path", "/").encode())]
        headers += [(k.lower().encode(), item.encode())
                    for k, v in request.get("headers", {}).items()
                    for item in (v if isinstance(v, list) else [v])]
        self.http.send_headers(stream, headers, end_stream=not body and not request.get("trailers"))
        trailers = request.get("trailers", {})
        if body:
            self.http.send_data(stream, body, end_stream=not trailers)
        if trailers:
            self.http.send_headers(stream, [(k.encode(), v.encode()) for k,v in trailers.items()], end_stream=True)
        self.transmit()
        return await asyncio.wait_for(future, 10)

class RawPeer(QuicConnectionProtocol):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.is_client = self._quic.configuration.is_client
        self.messages = {}
        self.waiters = {}

    def quic_event_received(self, event):
        if isinstance(event, StreamDataReceived):
            if self.is_client:
                body = self.messages.setdefault(event.stream_id, bytearray())
                body.extend(event.data)
                if event.end_stream:
                    self.waiters.pop(event.stream_id).set_result({"stream_id": event.stream_id, "hex": bytes(body).hex()})
            else:
                if event.data == b"oversized":
                    self._quic.send_stream_data(event.stream_id, b"x" * (1024 * 1024 + 1), end_stream=True)
                elif event.data == b"no-fin":
                    self._quic.send_stream_data(event.stream_id, b"waiting", end_stream=False)
                elif event.data == b"reset":
                    self._quic.reset_stream(event.stream_id, 3)
                else:
                    self._quic.send_stream_data(event.stream_id, event.data, end_stream=event.end_stream)
                self.transmit()

    async def request(self, request):
        stream = self._quic.get_next_available_stream_id()
        future = asyncio.get_running_loop().create_future()
        self.waiters[stream] = future
        self._quic.send_stream_data(stream, (bytes.fromhex(request["hex"]) if "hex" in request else request["body"].encode()), end_stream=True)
        self.transmit()
        return await asyncio.wait_for(future, 10)

async def main(args):
    config = QuicConfiguration(is_client=args.mode == "client", alpn_protocols=[args.alpn or ("h3" if args.protocol == "http3" else "netget-quic")], server_name="localhost", idle_timeout=15)
    protocol = H3Peer if args.protocol == "http3" else RawPeer
    if args.mode == "server":
        config.load_cert_chain(args.cert, args.key)
        server = await serve("127.0.0.1", args.port, configuration=config, create_protocol=protocol)
        # aioquic 1.3.0's serve API does not expose the bound address. Read it
        # from its live transport so port 0 is allocated only once, by the peer.
        port = server._transport.get_extra_info("sockname")[1]
        print(json.dumps({"ready": True, "port": port}), flush=True)
        try:
            await asyncio.Event().wait()
        finally:
            server.close()
    else:
        config.load_verify_locations(args.ca)
        async with connect("127.0.0.1", args.port, configuration=config, create_protocol=protocol) as client:
            requests = json.loads(args.requests)
            results = await asyncio.gather(*(client.request(request) for request in requests))
            print(json.dumps(results), flush=True)

if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=["server", "client"])
    parser.add_argument("protocol", choices=["http3", "quic"])
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--alpn")
    parser.add_argument("--cert")
    parser.add_argument("--key")
    parser.add_argument("--ca")
    parser.add_argument("--requests", default='[{"path":"/"}]')
    asyncio.run(main(parser.parse_args()))
