// Browser NNTP regression: both roles, bounded transactions and article dot stuffing.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import init, { NetGet } from '../../site/demo/pkg/netget_web.js';
await init({ module_or_path: readFileSync(new URL('../../site/demo/pkg/netget_web_bg.wasm', import.meta.url)) });
const events = [];
let painted = 0;
const netget = new NetGet({ cols: 100, rows: 30, model: 'nntp-browser-test', log: 'warn', onOutput(bytes) { painted += bytes.length; },
    onLlm: async (json) => {
        const request = JSON.parse(json);
        const message = request.messages.find(m => m.content.includes('Context data:'));
        const event = message ? JSON.parse(message.content.slice(message.content.indexOf('Context data:') + 'Context data:'.length)) : {};
        events.push(event);
        const action = event.command === 'GREETING' ? { type: 'send_nntp_response', code: 200, text: 'Browser NNTP ready' }
            : event.command === 'QUIT' ? { type: 'send_nntp_response', code: 205, text: 'Bye' }
            : { type: event.answer_with, accepted: true };
        return JSON.stringify({ content: JSON.stringify({ actions: [action] }), prompt_tokens: 1, completion_tokens: 1 });
    },
});
function call(method, ...args) {
    return Promise.race([
        new Promise(resolve => netget[method](...args, json => resolve(JSON.parse(json)))),
        new Promise((_, reject) => setTimeout(() => reject(new Error(`${method} deadline`)), 15000)),
    ]);
}
async function send(id, action) {
    const outcome = await call('send_to_client', id, JSON.stringify(action));
    assert.ok(outcome.Executed, JSON.stringify(outcome));
    return JSON.parse(outcome.Executed.detail);
}
try {
    const deadline = Date.now() + 15000;
    while (painted < 500 && Date.now() < deadline) await new Promise(resolve => setTimeout(resolve, 10));
    assert.ok(painted >= 500, 'dashboard starts');
    const server = await call('start_server', JSON.stringify({ protocol: 'nntp', port: 8119, instruction: 'Accept these test articles.' }));
    assert.ok(!server.error, JSON.stringify(server));
    const client = await call('start_client', JSON.stringify({ protocol: 'nntp', remote_addr: '127.0.0.1:8119', instruction: '', event_handlers: [{ event_pattern: '*', handler: { type: 'static', actions: [] } }] }));
    assert.ok(!client.error, JSON.stringify(client));
    const caps = await call('send_to_client', client.id, JSON.stringify({ type: 'nntp_capabilities' }));
    assert.ok(caps.Sent, JSON.stringify(caps));
    const headers = { 'Message-ID': '<browser@netget.test>', From: 'browser@netget.test', Newsgroups: 'misc.test', Subject: 'Browser test' };
    assert.equal((await send(client.id, { type: 'nntp_post', headers, body: '.hello\nworld' })).code, 240);
    assert.ok(events.some(e => e.operation === 'post' && JSON.stringify(e.article).includes('.hello') && !JSON.stringify(e.article).includes('..hello')));
    await call('send_to_client', client.id, JSON.stringify({ type: 'nntp_mode_stream' }));
    assert.equal((await send(client.id, { type: 'nntp_takethis', message_id: headers['Message-ID'], headers, body: 'feed' })).code, 239);
    assert.equal(await call('send_to_client', client.id, JSON.stringify({ type: 'nntp_quit' })), 'Disconnected');
    console.log('ok: browser NNTP server/client, capabilities, dot-stuffed POST, sequential feed and QUIT');
    process.exit(0);
} catch (error) {
    console.error(error);
    process.exit(1);
}
