// The official @anthropic-ai/sdk (TypeScript), unchanged, against an Anthropic-compatible
// server. Prints one JSON object describing what it saw; retries are off so an error is seen as
// the SDK raises it.
//
// Usage: node peer.mjs BASE_URL [WRONG_KEY_BASE_URL]
import Anthropic from "@anthropic-ai/sdk";

const [base, lockedBase] = process.argv.slice(2);
const client = new Anthropic({ baseURL: base, apiKey: "sk-ant-netget-test", maxRetries: 0, timeout: 60000 });
const out = {};

const m = await client.messages.create({
  model: "claude-netget-1",
  max_tokens: 64,
  system: "Be brief.",
  messages: [{ role: "user", content: "hello there" }],
});
out.plain = { id: m.id, text: m.content[0].text, stop_reason: m.stop_reason, role: m.role, model: m.model,
  usage: [m.usage.input_tokens, m.usage.output_tokens] };

const deltas = [];
const stream = client.messages.stream({
  model: "claude-netget-1",
  max_tokens: 64,
  messages: [{ role: "user", content: "stream this please, with ünïcode ✓" }],
});
stream.on("text", (t) => deltas.push(t));
const final = await stream.finalMessage();
out.stream = { deltas: deltas.length, joined: deltas.join(""), text: final.content[0].text, stop_reason: final.stop_reason };

const tools = [{ name: "get_weather", description: "Weather for a city",
  input_schema: { type: "object", properties: { city: { type: "string" } }, required: ["city"] } }];
const first = await client.messages.create({
  model: "claude-netget-1", max_tokens: 128, tools,
  messages: [{ role: "user", content: "what is the weather in Paris?" }],
});
const call = first.content.find((b) => b.type === "tool_use");
const second = await client.messages.create({
  model: "claude-netget-1", max_tokens: 128, tools,
  messages: [
    { role: "user", content: "what is the weather in Paris?" },
    { role: "assistant", content: first.content },
    { role: "user", content: [{ type: "tool_result", tool_use_id: call.id, content: "sunny, 21C" }] },
  ],
});
out.tools = { stop_reason: first.stop_reason, name: call.name, input: call.input, id_prefix: call.id.slice(0, 6),
  answer: second.content[0].text };

const toolStream = client.messages.stream({
  model: "claude-netget-1", max_tokens: 128, tools,
  messages: [{ role: "user", content: "streamed weather in Paris?" }],
});
const toolFinal = await toolStream.finalMessage();
const streamedCall = toolFinal.content.find((b) => b.type === "tool_use");
out.stream_tools = { name: streamedCall.name, input: streamedCall.input, stop_reason: toolFinal.stop_reason };

out.count = (await client.messages.countTokens({
  model: "claude-netget-1", messages: [{ role: "user", content: "count me" }],
})).input_tokens;
out.models = [];
for await (const model of client.models.list()) out.models.push(model.id);
out.model = (await client.models.retrieve("claude-netget-1")).id;

const refused = async (fn, cls) => {
  try {
    await fn();
    return "no error";
  } catch (e) {
    if (!(e instanceof cls)) throw e;
    return { status: e.status, type: e.error?.error?.type, message: e.error?.error?.message };
  }
};
out.rate_limited = await refused(() => client.messages.create({
  model: "deny-me", max_tokens: 8, messages: [{ role: "user", content: "x" }] }), Anthropic.RateLimitError);
out.invalid = await refused(() => client.messages.create({
  model: "claude-netget-1", max_tokens: 8, messages: [{ role: "robot", content: "x" }] }), Anthropic.BadRequestError);
delete out.invalid.message;
out.not_found = await refused(() => client.models.retrieve("no-such-model"), Anthropic.NotFoundError);
delete out.not_found.message;
if (lockedBase) {
  const locked = new Anthropic({ baseURL: lockedBase, apiKey: "sk-ant-wrong", maxRetries: 0, timeout: 60000 });
  out.auth = await refused(() => locked.messages.create({
    model: "claude-netget-1", max_tokens: 8, messages: [{ role: "user", content: "x" }] }), Anthropic.AuthenticationError);
  delete out.auth.message;
}
console.log(JSON.stringify(out));
