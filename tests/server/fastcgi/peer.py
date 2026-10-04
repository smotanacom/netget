"""flup 1.0.3's threaded WSGI FastCGI server, unchanged, on 127.0.0.1:0 for NetGet's FastCGI
client. Prints {"port": N}, serves until stdin closes.

Routes on REQUEST_URI: /big (200 000 bytes), /stderr (writes wsgi.errors, i.e. FCGI_STDERR),
/slow (3 s), /missing (404), /redirect (302 with Location); anything else echoes the request
as JSON.
"""
import json, sys, threading, time
from flup.server.fcgi import WSGIServer


def app(environ, start_response):
    uri = environ.get("REQUEST_URI", "")
    path = uri.split("?")[0]
    if path == "/big":
        start_response("200 OK", [("Content-Type", "application/octet-stream")])
        return [b"x" * 200_000]
    if path == "/stderr":
        environ["wsgi.errors"].write("flup stderr line\n")
        start_response("200 OK", [("Content-Type", "text/plain")])
        return [b"logged"]
    if path == "/slow":
        time.sleep(3)
        start_response("200 OK", [("Content-Type", "text/plain")])
        return [b"slow done"]
    if path == "/missing":
        start_response("404 Not Found", [("Content-Type", "text/plain")])
        return [b"no such page"]
    if path == "/redirect":
        start_response("302 Found", [("Location", "/elsewhere"), ("Content-Type", "text/plain")])
        return [b""]
    length = int(environ.get("CONTENT_LENGTH") or 0)
    body = environ["wsgi.input"].read(length).decode("utf-8", "replace") if length else ""
    echo = {
        "method": environ.get("REQUEST_METHOD"),
        "uri": uri,
        "query": environ.get("QUERY_STRING"),
        "script_filename": environ.get("SCRIPT_FILENAME"),
        "content_type": environ.get("CONTENT_TYPE"),
        "x_test": environ.get("HTTP_X_TEST"),
        "custom": environ.get("NETGET_CUSTOM"),
        "body_length": len(body),
        "body_head": body[:32],
    }
    start_response("200 OK", [("Content-Type", "application/json"), ("X-Flup", "yes")])
    return [json.dumps(echo).encode()]


class Server(WSGIServer):
    def _setupSocket(self):
        sock = super()._setupSocket()
        print(json.dumps({"port": sock.getsockname()[1]}), flush=True)
        return sock


if __name__ == "__main__":
    server = Server(app, bindAddress=("127.0.0.1", 0))
    threading.Thread(target=lambda: (sys.stdin.read(), server._exit()), daemon=True).start()
    server.run()
