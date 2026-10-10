"""The official anthropic Python SDK, unchanged, against an Anthropic-compatible server.
Prints one JSON object describing what it saw; every call has retries off so an error is seen
as the SDK raises it.

Usage: python sdk_peer.py BASE_URL [WRONG_KEY_BASE_URL]
"""
import json
import sys

import anthropic

base = sys.argv[1]
client = anthropic.Anthropic(base_url=base, api_key="sk-ant-netget-test", max_retries=0, timeout=60)
out = {}

m = client.messages.create(
    model="claude-netget-1",
    max_tokens=64,
    system="Be brief.",
    messages=[{"role": "user", "content": "hello there"}],
)
out["plain"] = {"id": m.id, "text": m.content[0].text, "stop_reason": m.stop_reason,
                "role": m.role, "model": m.model, "usage": [m.usage.input_tokens, m.usage.output_tokens]}

with client.messages.stream(
    model="claude-netget-1",
    max_tokens=64,
    messages=[{"role": "user", "content": "stream this please, with ünïcode ✓"}],
) as stream:
    deltas = [t for t in stream.text_stream]
    final = stream.get_final_message()
out["stream"] = {"deltas": len(deltas), "joined": "".join(deltas), "text": final.content[0].text,
                 "stop_reason": final.stop_reason}

tools = [{"name": "get_weather", "description": "Weather for a city",
          "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}}]
first = client.messages.create(
    model="claude-netget-1",
    max_tokens=128,
    tools=tools,
    messages=[{"role": "user", "content": "what is the weather in Paris?"}],
)
call = next(b for b in first.content if b.type == "tool_use")
second = client.messages.create(
    model="claude-netget-1",
    max_tokens=128,
    tools=tools,
    messages=[
        {"role": "user", "content": "what is the weather in Paris?"},
        {"role": "assistant", "content": first.content},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": call.id, "content": "sunny, 21C"}]},
    ],
)
out["tools"] = {"stop_reason": first.stop_reason, "name": call.name, "input": call.input,
                "id_prefix": call.id[:6], "answer": second.content[0].text}

with client.messages.stream(
    model="claude-netget-1",
    max_tokens=128,
    tools=tools,
    messages=[{"role": "user", "content": "streamed weather in Paris?"}],
) as stream:
    final = stream.get_final_message()
streamed_call = next(b for b in final.content if b.type == "tool_use")
out["stream_tools"] = {"name": streamed_call.name, "input": streamed_call.input, "stop_reason": final.stop_reason}

out["count"] = client.messages.count_tokens(
    model="claude-netget-1", messages=[{"role": "user", "content": "count me"}]
).input_tokens
out["models"] = [m.id for m in client.models.list()]
out["model"] = client.models.retrieve("claude-netget-1").id

try:
    client.messages.create(model="deny-me", max_tokens=8, messages=[{"role": "user", "content": "x"}])
    out["rate_limited"] = "no error"
except anthropic.RateLimitError as e:
    out["rate_limited"] = {"status": e.status_code, "type": e.body["error"]["type"], "message": e.body["error"]["message"]}

try:
    client.messages.create(model="claude-netget-1", max_tokens=8, messages=[{"role": "robot", "content": "x"}])
    out["invalid"] = "no error"
except anthropic.BadRequestError as e:
    out["invalid"] = {"status": e.status_code, "type": e.body["error"]["type"]}

try:
    client.models.retrieve("no-such-model")
    out["not_found"] = "no error"
except anthropic.NotFoundError as e:
    out["not_found"] = {"status": e.status_code, "type": e.body["error"]["type"]}

if len(sys.argv) > 2:
    locked = anthropic.Anthropic(base_url=sys.argv[2], api_key="sk-ant-wrong", max_retries=0, timeout=60)
    try:
        locked.messages.create(model="claude-netget-1", max_tokens=8, messages=[{"role": "user", "content": "x"}])
        out["auth"] = "no error"
    except anthropic.AuthenticationError as e:
        out["auth"] = {"status": e.status_code, "type": e.body["error"]["type"]}

print(json.dumps(out))
