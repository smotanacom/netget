#!/usr/bin/env python3
"""Capture real NetGet network prompts against a local mock, then optionally replay them.

Capture never contacts a model. Replay is an explicit separate subcommand.
Uses only Python's standard library. All protocol endpoints bind to loopback.
"""

import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import re
import signal
import socket
import subprocess
import struct
import threading
import time
import urllib.request


MODEL = "llama3.1:8b"
INSTRUCTIONS = {
    "imap": 'You are mail.example.com. Greet with IMAP4rev1 capability. Accept LOGIN. '
            'Exactly two mailboxes exist: INBOX and Archive. For LIST, return exactly '
            'these two mailboxes with delimiter "/" and no flags.',
    "http": 'Return HTTP 200, Content-Type text/plain, and the exact body: Hello "NetGet" & friends.',
    "telnet": 'On connection, send the line Welcome to NetGet. When the visitor sends '
              'hello, respond with the line Hello "NetGet" & friends.',
}


def request_text(payload):
    return payload.get("prompt", "") + "\n" + "\n".join(
        message.get("content", "") for message in payload.get("messages", [])
    )


def event_name(payload):
    events = re.findall(r"Event ID:\s*([a-z_]+)", request_text(payload))
    return events[-1] if events else "unknown"


def mock_actions(protocol, payload):
    event = event_name(payload)
    text = request_text(payload)
    if protocol == "imap":
        if event == "imap_connection":
            return [{"type": "send_imap_greeting", "hostname": "mail.example.com", "capabilities": ["IMAP4rev1"]}]
        if event == "imap_auth":
            return [{"type": "send_imap_response", "tag": "A001", "status": "OK", "message": "LOGIN completed"}]
        return [{"type": "send_imap_list", "mailboxes": [
            {"name": name, "delimiter": "/", "flags": []} for name in ["INBOX", "Archive"]
        ]}]
    if protocol == "http":
        return [{"type": "send_http_response", "status": 200,
                 "headers": {"Content-Type": "text/plain"}, "body": 'Hello "NetGet" & friends.'}]
    if protocol == "telnet":
        line = 'Hello "NetGet" & friends.' if event == "telnet_message_received" else "Welcome to NetGet."
        return [{"type": "send_telnet_line", "line": line}]
    raise ValueError((protocol, event, text[-100:]))


class CaptureServer(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, directory):
        super().__init__(("127.0.0.1", 0), CaptureHandler)
        self.directory = directory
        self.protocol = ""
        self.records = []
        self.lock = threading.Lock()


class CaptureHandler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def send_json(self, payload):
        body = json.dumps(payload).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/api/tags":
            self.send_json({"models": [{"name": MODEL, "model": MODEL, "size": 1000000,
                                        "modified_at": "2026-10-01T00:00:00Z"}]})
        else:
            self.send_json({"version": "0.0.1-prompt-capture"})

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        payload = json.loads(raw)
        if self.path not in ["/api/generate", "/api/chat"]:
            self.send_json({"model_info": {"llama.context_length": 8192}, "capabilities": ["completion"]})
            return
        # Exact request bytes are kept independently of the pretty-printed metadata.
        with self.server.lock:
            ordinal = len(self.server.records) + 1
            stem = f"{ordinal:02d}-{self.server.protocol}-{event_name(payload)}"
            (self.server.directory / f"{stem}.request.json").write_bytes(raw)
            (self.server.directory / f"{stem}.prompt.txt").write_text(request_text(payload))
            record = {"ordinal": ordinal, "protocol": self.server.protocol,
                      "event": event_name(payload), "endpoint": self.path,
                      "request_file": f"{stem}.request.json", "sha256": hashlib.sha256(raw).hexdigest(),
                      "prompt_chars": len(request_text(payload))}
            self.server.records.append(record)
        content = json.dumps({"actions": mock_actions(self.server.protocol, payload)})
        response = {"model": MODEL, "created_at": "2026-10-01T00:00:00Z", "done": True,
                    "done_reason": "stop", "total_duration": 1, "eval_count": 1}
        if self.path == "/api/chat":
            response["message"] = {"role": "assistant", "content": content}
        else:
            response["response"] = content
        self.send_json(response)


def connect(port, local_port, process):
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"NetGet exited early: {process.returncode}")
        peer = socket.socket()
        peer.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        peer.settimeout(15)
        peer.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
        peer.bind(("127.0.0.1", local_port))
        try:
            peer.connect(("127.0.0.1", port))
            return peer
        except ConnectionRefusedError:
            peer.close()
            time.sleep(0.1)
    raise TimeoutError("NetGet listener did not start")


def read_until(peer, terminator):
    chunks = bytearray()
    while terminator not in chunks:
        part = peer.recv(65536)
        if not part:
            raise EOFError(bytes(chunks))
        chunks.extend(part)
    return chunks.decode("utf-8", "replace")


def capture(args):
    output = Path(args.output).resolve() / args.label
    output.mkdir(parents=True, exist_ok=False)
    server = CaptureServer(output)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    exchanges = {}
    try:
        for index, protocol in enumerate(INSTRUCTIONS):
            server.protocol = protocol
            port, local_port = args.port_base + index, args.port_base + 100 + index
            directory = output / protocol
            directory.mkdir()
            command = [str(Path(args.binary).resolve()), "--server", protocol, "--port", str(port),
                       "--listen-addr", "127.0.0.1", "--ollama-url",
                       f"http://127.0.0.1:{server.server_port}", "--model", MODEL,
                       "--llm-seed", "42", "--env", "off", "--handler", "llm",
                       "--run-for", "45", "--log-level", "info", "--", INSTRUCTIONS[protocol]]
            with (directory / "process.log").open("wb") as log:
                process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=log,
                                           stderr=subprocess.STDOUT, cwd=directory, start_new_session=True)
                try:
                    with connect(port, local_port, process) as peer:
                        if protocol == "imap":
                            replies = [read_until(peer, b"\r\n")]
                            peer.sendall(b"A001 LOGIN alice secret\r\n")
                            replies.append(read_until(peer, b"A001 OK"))
                            peer.sendall(b'A002 LIST "" "*"\r\n')
                            replies.append(read_until(peer, b"A002 OK"))
                        elif protocol == "http":
                            peer.sendall(b"GET /hello?name=NetGet HTTP/1.0\r\n\r\n")
                            replies = [read_until(peer, b'Hello "NetGet" & friends.')]
                        else:
                            replies = [read_until(peer, b"\r\n")]
                            peer.sendall(b"hello\r\n")
                            replies.append(read_until(peer, b"\r\n"))
                        exchanges[protocol] = {"command": command, "wire_replies": replies}
                finally:
                    if process.poll() is None:
                        os.killpg(process.pid, signal.SIGTERM)
                        try:
                            process.wait(timeout=5)
                        except subprocess.TimeoutExpired:
                            os.killpg(process.pid, signal.SIGKILL)
                            process.wait()
    finally:
        server.shutdown()
        server.server_close()
        manifest = {"binary": str(Path(args.binary).resolve()), "binary_sha256": hashlib.sha256(Path(args.binary).read_bytes()).hexdigest(), "label": args.label,
                    "model": MODEL, "seed": 42, "instructions": INSTRUCTIONS,
                    "requests": server.records, "exchanges": exchanges}
        (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    assert len(server.records) == 6, server.records
    print(json.dumps({"capture": str(output), "requests": server.records}, indent=2))


TOOL_NAMES = {"read_file", "web_search", "read_base_stack_docs", "read_server_documentation",
              "read_client_documentation", "read_documentation", "list_models", "generate_random",
              "list_tasks", "execute_sql", "list_databases"}


def answer_object(content):
    """Count names in an answer-shaped JSON value; this does not validate execution."""
    decoder = json.JSONDecoder()
    for match in re.finditer(r"[\[{]", content):
        try:
            value, _ = decoder.raw_decode(content[match.start():])
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict) and ("actions" in value or "tools" in value):
            if not all(isinstance(value.get(k, []), list) for k in ("actions", "tools")):
                return None
            entries = value.get("actions", []) + value.get("tools", [])
        elif isinstance(value, dict) and ("tool_calls" in value or "tool_call" in value):
            calls = value.get("tool_calls", value.get("tool_call"))
            entries = calls if isinstance(calls, list) else [calls]
        elif isinstance(value, dict) and any(isinstance(value.get(k), str) for k in ("type", "function", "name")):
            entries = [value]
        elif isinstance(value, list) and all(isinstance(item, dict) for item in value):
            entries = value
        elif value == {}:
            entries = []
        elif isinstance(value, dict):
            return None
        else:
            continue
        if not all(isinstance(item, dict) for item in entries):
            return None
        normalized = {"tools": [], "actions": []}
        for item in entries:
            function = item.get("function")
            name = function.get("name") if isinstance(function, dict) else None
            if not isinstance(name, str):
                keys = ("function", "name") if item.get("type") == "function" else ("type", "function", "name")
                name = next((item[k] for k in keys if isinstance(item.get(k), str)), None)
            if name is None:
                return None
            normalized["tools" if name in TOOL_NAMES else "actions"].append(item)
        return normalized
    return None


def replay(args):
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with opener.open(args.ollama_url.rstrip("/") + "/api/tags", timeout=30) as response:
        tags = json.load(response)
    model_record = next(model for model in tags["models"] if model["name"] == args.model)
    with opener.open(args.ollama_url.rstrip("/") + "/api/version", timeout=30) as response:
        ollama_version = json.load(response)
    assert model_record["digest"] == "46e0c10c039e019119339687c3c1757cc81b9da49709a3b3924863ba87ca666e"
    captures = [(Path(name), json.loads((Path(name) / "manifest.json").read_text())) for name in args.captures]
    events = [record["event"] for record in captures[0][1]["requests"]]
    assert all([record["event"] for record in manifest["requests"]] == events for _, manifest in captures)
    started_at = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    rows = []
    for request_index, event in enumerate(events):
        for repeat in range(args.repeats):
            # Alternate first/second order across both requests and repetitions.
            ordered = captures if (request_index + repeat) % 2 == 0 else list(reversed(captures))
            for directory, manifest in ordered:
                record = manifest["requests"][request_index]
                payload = json.loads((directory / record["request_file"]).read_bytes())
                assert payload["model"] == args.model and payload["options"]["seed"] == args.seed
                assert payload["stream"] is True
                payload["model"] = args.model
                payload["stream"] = False
                payload.setdefault("options", {})["seed"] = args.seed
                request = urllib.request.Request(args.ollama_url.rstrip("/") + record["endpoint"],
                                                 data=json.dumps(payload).encode(),
                                                 headers={"Content-Type": "application/json"})
                started = time.monotonic()
                with opener.open(request, timeout=300) as response:
                    answer = json.load(response)
                elapsed = time.monotonic() - started
                content = answer.get("response", answer.get("message", {}).get("content", ""))
                parsed = answer_object(content)
                row = {"label": manifest["label"], "request": record["request_file"],
                       "repeat": repeat + 1, "sequence": len(rows) + 1, "event": event,
                       "elapsed_seconds": round(elapsed, 3),
                       "original_request_sha256": record["sha256"],
                       "replayed_payload_sha256": hashlib.sha256(json.dumps(payload).encode()).hexdigest(),
                       "endpoint": record["endpoint"],
                       "counter_note": "First answer-shaped JSON value; raw response retained for review. This is a counter, not NetGet validation.",
                       "tools": None if parsed is None else len(parsed.get("tools", [])),
                       "actions": None if parsed is None else len(parsed.get("actions", [])),
                       "parsed": parsed, "response": answer}
                rows.append(row)
                Path(args.output).write_text(json.dumps({"model": args.model, "model_record": model_record, "seed": args.seed,
                                                        "ollama_version": ollama_version, "started_at": started_at,
                                                        "updated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                                                        "order": "Paired by event and repeat, alternating first/second variant order across requests and repetitions.",
                                                        "repeats": args.repeats, "captures": args.captures, "results": rows}, indent=2) + "\n")
                print(json.dumps({k: row[k] for k in ["label", "request", "repeat", "elapsed_seconds", "tools", "actions"]}), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    cap = commands.add_parser("capture")
    cap.add_argument("--binary", required=True)
    cap.add_argument("--label", required=True)
    cap.add_argument("--output", default=str(Path(__file__).resolve().parent / "captures"))
    cap.add_argument("--port-base", type=int, default=19440)
    rep = commands.add_parser("replay")
    rep.add_argument("captures", nargs="+")
    rep.add_argument("--ollama-url", required=True)
    rep.add_argument("--model", default=MODEL)
    rep.add_argument("--seed", type=int, default=42)
    rep.add_argument("--repeats", type=int, default=1)
    rep.add_argument("--output", default=str(Path(__file__).resolve().parent / "replay-results.json"))
    args = parser.parse_args()
    capture(args) if args.command == "capture" else replay(args)


if __name__ == "__main__":
    main()
