// The ActivityPub peer for NetGet's tests: one actor built on the Fedify library (the same
// code the Fedify CLI and every Fedify-based server run). Fedify verifies every inbox
// request's HTTP Signature itself (skipSignatureVerification stays false), signs everything
// it sends, and fetches and parses NetGet's documents. allowPrivateAddress is on because
// both run on this machine, as Fedify's own tests do.
//
//   node peer.mjs follow <actor-url>   follow that actor; print what arrives
//   node peer.mjs serve                accept every Follow; print what arrives
//
// The first line printed is {"actor": <this actor's id>}. Every activity that reaches the
// inbox (after Fedify verified it) is one JSON line. Runs until stdin closes.
import { createFederation, MemoryKvStore, generateCryptoKeyPair } from "@fedify/fedify";
import { Accept, Activity, Create, Endpoints, Follow, Note, Person, Undo, isActor } from "@fedify/vocab";
import http from "node:http";

const [mode, target] = process.argv.slice(2);
const out = (v) => process.stdout.write(JSON.stringify(v) + "\n");

const federation = createFederation({ kv: new MemoryKvStore(), allowPrivateAddress: true });
let keys;
federation
  .setActorDispatcher("/users/{identifier}", async (ctx, identifier) => {
    if (identifier !== "peer") return null;
    const pairs = await ctx.getActorKeyPairs(identifier);
    return new Person({
      id: ctx.getActorUri(identifier),
      preferredUsername: identifier,
      name: "Fedify peer",
      inbox: ctx.getInboxUri(identifier),
      endpoints: new Endpoints({ sharedInbox: ctx.getInboxUri() }),
      publicKey: pairs[0].cryptographicKey,
      assertionMethods: pairs.map((p) => p.multikey),
    });
  })
  .setKeyPairsDispatcher(async (_ctx, identifier) => {
    if (identifier !== "peer") return [];
    keys ??= [await generateCryptoKeyPair("RSASSA-PKCS1-v1_5"), await generateCryptoKeyPair("Ed25519")];
    return keys;
  });

federation
  .setInboxListeners("/users/{identifier}/inbox", "/inbox")
  .on(Activity, async (ctx, activity) => {
    const object = await activity.getObject({ crossOrigin: "trust" }).catch(() => null);
    const line = {
      received: activity.constructor.name,
      id: activity.id?.href,
      actor: activity.actorId?.href,
      object_type: object?.constructor.name,
      object_id: object?.id?.href ?? activity.objectId?.href,
    };
    if (object instanceof Note) {
      line.content = object.content?.toString();
      line.to = object.toIds.map((u) => u.href);
    }
    if (object instanceof Follow) line.follow_object = object.objectId?.href;
    out(line);
    if (activity instanceof Follow && mode === "serve") {
      const follower = await activity.getActor();
      if (!isActor(follower) || activity.id == null) return;
      await ctx.sendActivity({ identifier: "peer" }, follower, new Accept({
        id: new URL(`#accept/${Date.now()}`, ctx.getActorUri("peer")),
        actor: ctx.getActorUri("peer"),
        object: activity.id,
      }));
      out({ sent: "Accept", to: follower.id?.href });
    }
  });

const server = http.createServer(async (req, res) => {
  const chunks = [];
  for await (const c of req) chunks.push(c);
  const body = Buffer.concat(chunks);
  const request = new Request(`http://${req.headers.host}${req.url}`, {
    method: req.method,
    headers: req.headers,
    body: req.method === "GET" || req.method === "HEAD" ? undefined : body,
  });
  const response = await federation.fetch(request, { contextData: undefined });
  res.writeHead(response.status, Object.fromEntries(response.headers));
  res.end(Buffer.from(await response.arrayBuffer()));
});

server.listen(0, "127.0.0.1", async () => {
  const origin = `http://127.0.0.1:${server.address().port}`;
  const ctx = federation.createContext(new URL(origin), undefined);
  out({ actor: ctx.getActorUri("peer").href });
  if (mode === "follow") {
    const actor = await ctx.lookupObject(target);
    if (!isActor(actor)) {
      out({ error: `not an actor: ${target}` });
      process.exit(1);
    }
    out({ looked_up: actor.id?.href, name: actor.preferredUsername?.toString(), inbox: actor.inboxId?.href });
    await ctx.sendActivity({ identifier: "peer" }, actor, new Follow({
      id: new URL(`#follow/${Date.now()}`, ctx.getActorUri("peer")),
      actor: ctx.getActorUri("peer"),
      object: actor.id,
    }), { immediate: true });
    out({ sent: "Follow", to: actor.id?.href });
  }
});
process.stdin.on("end", () => process.exit(0));
process.stdin.resume();
