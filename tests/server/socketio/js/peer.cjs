// The reference socket.io-client, unchanged, against a Socket.IO server. Run with NODE_PATH
// pointing at the installed node_modules:  node peer.cjs URL [websocket-only]
// Prints one JSON object per step and exits 0 when every step completed.
const { io } = require("socket.io-client");
const url = process.argv[2];
const transports = process.argv[3] === "websocket-only" ? ["websocket"] : ["polling", "websocket"];
const out = (o) => console.log(JSON.stringify(o));
const fail = (step, e) => { out({ step, error: String(e) }); process.exit(1); };
const timer = setTimeout(() => fail("timeout", "peer did not finish in 30 s"), 30000);
const once = (s, ev) => new Promise((resolve) => s.once(ev, (...a) => resolve(a)));

(async () => {
  const socket = io(url, { transports, auth: { token: "anyone" }, reconnection: false });
  const welcome = once(socket, "welcome");
  await once(socket, "connect");
  out({ step: "connect", id: socket.id, transport: socket.io.engine.transport.name });
  out({ step: "welcome", args: await welcome });
  if (transports.length === 2 && socket.io.engine.transport.name !== "websocket") {
    await once(socket.io.engine, "upgrade");
  }
  out({ step: "transport", name: socket.io.engine.transport.name });
  const broadcast = once(socket, "chat message");
  const ack = await socket.timeout(10000).emitWithAck("chat message", "hi from js");
  out({ step: "ack", args: ack });
  out({ step: "broadcast", args: await broadcast });
  // The server asks us something and waits for our acknowledgement.
  socket.once("ping me", (arg, callback) => callback("pong from js", arg));
  const answered = once(socket, "pong received");
  socket.emit("ask me");
  out({ step: "server_ack", args: await answered });
  // A second namespace with the right auth, then one the server refuses.
  const admin = io(url + "admin", { transports, auth: { token: "secret" }, reconnection: false });
  await once(admin, "connect");
  out({ step: "admin", id: admin.id });
  const denied = io(url + "admin", { transports, auth: { token: "wrong" }, reconnection: false });
  out({ step: "admin_denied", message: (await once(denied, "connect_error"))[0].message });
  const unknown = io(url + "nowhere", { transports, reconnection: false });
  out({ step: "unknown_namespace", message: (await once(unknown, "connect_error"))[0].message });
  admin.disconnect();
  denied.close();
  unknown.close();
  const gone = once(socket, "disconnect");
  socket.emit("bye");
  out({ step: "server_disconnect", reason: (await gone)[0] });
  socket.close();
  clearTimeout(timer);
  process.exit(0);
})().catch((e) => fail("exception", e));
