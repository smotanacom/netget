// rhea 3.0.5, unchanged, as an AMQP 1.0 client of NetGet's container: SASL PLAIN, receivers on
// orders.confirmed, news and chat, a sender on orders (accepted and rejected), a data message on
// chat received back, a refused sender on forbidden, and a refused password.
// Usage: node client.cjs HOST PORT. One JSON line per observation.
const rhea = require('rhea');
const [host, port] = [process.argv[2], Number(process.argv[3])];
const out = (o) => console.log(JSON.stringify(o));
const container = rhea.create_container({ id: 'rhea-peer' });
const wait = (emitter, ok, bad) => new Promise((resolve, reject) => {
  emitter.once(ok, resolve);
  for (const b of bad || []) emitter.once(b, (ctx) => reject(ctx));
});

async function main() {
  const conn = container.connect({ host, port, username: 'alice', password: 'secret', reconnect: false });
  await wait(conn, 'connection_open', ['connection_error', 'disconnected']);
  out({ step: 'open', container: conn.remote.open.container_id });
  const received = [];
  for (const address of ['orders.confirmed', 'news', 'chat']) {
    const r = conn.open_receiver({ source: { address }, credit_window: 10 });
    r.on('message', (ctx) => {
      const body = ctx.message.body;
      const text = body && body.typecode === 0x75 ? body.content.toString() : body;
      received.push({ address, body: text, subject: ctx.message.subject });
    });
    await wait(r, 'receiver_open', ['receiver_error', 'receiver_close']);
  }
  const s = conn.open_sender({ target: { address: 'orders' } });
  await wait(s, 'sendable', ['sender_error', 'sender_close']);
  const settle = (delivery) => new Promise((resolve) => {
    s.once('accepted', (ctx) => resolve({ outcome: 'accepted', id: ctx.delivery.id }));
    s.once('rejected', (ctx) => resolve({ outcome: 'rejected', error: ctx.delivery.remote_state.error }));
    s.once('released', () => resolve({ outcome: 'released' }));
  });
  let p = settle(); s.send({ message_id: 'o-1', body: { order: 1 } }); out({ step: 'order', ...(await p) });
  p = settle(); s.send({ message_id: 'o-2', body: { nope: true } }); out({ step: 'no_order', ...(await p) });
  const chat = conn.open_sender({ target: { address: 'chat' } });
  await wait(chat, 'sendable', ['sender_error', 'sender_close']);
  chat.send({ subject: 'hello', body: rhea.message.data_section(Buffer.from('hello from rhea')) });
  const refused = conn.open_sender({ target: { address: 'forbidden' } });
  const why = await new Promise((resolve) => {
    refused.once('sender_open', () => {});
    refused.once('sender_error', (ctx) => resolve(ctx.sender.error));
    refused.once('sender_close', (ctx) => resolve(ctx.sender.error));
  });
  out({ step: 'forbidden', condition: why && why.condition, description: why && why.description });
  await new Promise((r) => setTimeout(r, 1500));
  out({ step: 'received', messages: received });
  conn.close();
  await wait(conn, 'connection_close', ['disconnected']).catch(() => {});
  const bad = container.connect({ host, port, username: 'alice', password: 'wrong', reconnect: false });
  const err = await new Promise((resolve) => {
    bad.once('connection_open', () => resolve('opened'));
    bad.once('connection_error', (ctx) => resolve((ctx.connection.get_error() || {}).condition || 'error'));
    bad.once('disconnected', (ctx) => resolve((ctx.error && ctx.error.message) || 'disconnected'));
  });
  out({ step: 'bad_password', result: String(err) });
  process.exit(0);
}
main().catch((e) => { out({ step: 'failed', error: String(e && (e.message || e.error || e)) }); process.exit(1); });
