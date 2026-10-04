// rhea 3.0.5, unchanged, as an AMQP 1.0 broker for NetGet's client. SASL PLAIN for bob/pw (and
// ANONYMOUS). A receiver on queue.out gets one message; a sender to forbidden is refused; every
// message is printed, and one whose body has "bad" is rejected.
// Usage: node broker.cjs PORT. One JSON line per observation.
const rhea = require('rhea');
const port = Number(process.argv[2]);
const out = (o) => console.log(JSON.stringify(o));
const container = rhea.create_container({ id: 'rhea-broker' });
container.sasl_server_mechanisms.enable_anonymous();
container.sasl_server_mechanisms.enable_plain((user, password) => user === 'bob' && password === 'pw');
container.on('connection_open', (ctx) => out({ event: 'open', container: ctx.connection.remote.open.container_id }));
container.on('sender_open', (ctx) => {
  const address = ctx.sender.source && ctx.sender.source.address;
  out({ event: 'consumer', address });
  if (address === 'queue.out') ctx.sender.once('sendable', () => ctx.sender.send({ subject: 'greeting', body: 'from rhea', application_properties: { n: 7 } }));
});
container.on('receiver_open', (ctx) => {
  const address = ctx.receiver.target && ctx.receiver.target.address;
  if (address === 'forbidden') ctx.receiver.close({ condition: 'amqp:unauthorized-access', description: 'not here' });
  else out({ event: 'producer', address });
});
container.on('message', (ctx) => {
  const m = ctx.message;
  const body = m.body && m.body.typecode === 0x75 ? m.body.content.toString() : m.body;
  out({ event: 'message', address: ctx.receiver.target.address, body, subject: m.subject, message_id: m.message_id, application_properties: m.application_properties });
  if (body && typeof body === 'object' && body.bad) ctx.delivery.reject({ condition: 'amqp:precondition-failed', description: 'bad message' });
});
container.listen({ port, host: '127.0.0.1' }).on('listening', () => out({ event: 'listening', port }));
