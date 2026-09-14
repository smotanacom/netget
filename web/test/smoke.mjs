// Headless smoke test for the browser bundle, run under Node.
//
// Drives site/demo/pkg/netget_web.js the way the page does, minus the DOM: boots NetGet,
// checks the dashboard paints into the fake terminal, starts a TCP server through
// `start_server`, connects to it over the virtual network, sends a line, answers the
// model request the page receives with a `send_tcp_data` action, and asserts the bytes come
// back on the connection. Exit code 0 means the whole path — dashboard, ServerForm, virtual
// sockets, protocol server, LLM bridge, action executor — works in wasm.
//
//   ./web/build.sh && node web/test/smoke.mjs

import { readFileSync } from 'node:fs';
import init, { NetGet } from '../../site/demo/pkg/netget_web.js';

const wasm = readFileSync(new URL('../../site/demo/pkg/netget_web_bg.wasm', import.meta.url));
await init({ module_or_path: wasm });

const dec = new TextDecoder();
const enc = new TextEncoder();
let screen = '';
const requests = [];
const received = [];

function fail(msg) {
    console.error('FAIL:', msg);
    console.error('--- last terminal output ---');
    console.error(screen.slice(-2000));
    process.exit(1);
}

function waitFor(pred, what, ms = 15000) {
    const deadline = Date.now() + ms;
    return new Promise((resolve, reject) => {
        const tick = () => {
            if (pred()) return resolve();
            if (Date.now() > deadline) return reject(new Error('timed out waiting for ' + what));
            setTimeout(tick, 50);
        };
        tick();
    });
}

const PORT = 7000;

const netget = new NetGet({
    cols: 120,
    rows: 36,
    model: 'smoke-test',
    theme: 'dark',
    log: 'warn',
    onOutput: (bytes) => { screen += dec.decode(bytes); },
    onLlm: async (json) => {
        const req = JSON.parse(json);
        requests.push(req);
        // The network-event path sends one flattened prompt whose user turn carries
        // "Context data:\n{...}" with the connection id and the received text.
        const user = req.messages.find((m) => m.role === 'user') || req.messages[req.messages.length - 1];
        const ctx = user.content.indexOf('Context data:');
        let context = {};
        if (ctx >= 0) {
            try { context = JSON.parse(user.content.slice(ctx + 'Context data:'.length)); } catch (e) { /* prompt-only */ }
        }
        const text = String(context.data ?? context.data_preview ?? context.content ?? context.text ?? '');
        // Answer in the vocabulary of whichever server asked: TCP echoes uppercased, UDP
        // reverses.
        const isUdp = req.messages.some((m) => m.content.includes('send_udp_response'));
        const reply = isUdp
            ? { actions: [{ type: 'send_udp_response', data: [...text.trim()].reverse().join(''), encoding: 'text' }] }
            : { actions: [{ type: 'send_tcp_data', data: text.toUpperCase() }] };
        return JSON.stringify({ content: JSON.stringify(reply), prompt_tokens: 10, completion_tokens: 5 });
    },
});

try {
    await waitFor(() => screen.length > 500, 'the dashboard to paint');
    if (!/\x1b\[\d+;\d+H/.test(screen)) fail('output has no cursor-positioning sequences; not a rendered frame');

    let started = null;
    netget.start_server(JSON.stringify({
        protocol: 'tcp', port: PORT,
        instruction: 'Echo every line back to the client, uppercased.',
    }), (json) => { started = JSON.parse(json); });
    await waitFor(() => started !== null, 'start_server to answer');
    if (started.error) fail('start_server: ' + started.error);

    await waitFor(() => netget.listening_ports().includes(PORT), `port ${PORT} to be listening`);

    let servers = null;
    netget.servers((json) => { servers = JSON.parse(json); });
    await waitFor(() => servers !== null, 'servers()');
    if (!servers.some((s) => s.protocol === 'tcp' && s.port === PORT)) fail('servers() does not list the tcp server: ' + JSON.stringify(servers));

    let closed = null;
    const conn = netget.connect(PORT, (bytes) => received.push(dec.decode(bytes)), (reason) => { closed = reason ?? 'eof'; });
    if (!netget.send(conn, enc.encode('hello netget\n'))) fail('send() reported the connection is gone');

    await waitFor(() => requests.length > 0, 'the LLM request', 20000);
    const req = requests[0];
    if (req.kind !== 'generate') fail('expected a generate request, got ' + req.kind);
    if (!req.messages.some((m) => m.content.includes('hello netget'))) fail('the prompt does not carry the received bytes');

    await waitFor(() => received.join('').includes('HELLO NETGET'), 'the echoed bytes', 20000);

    // The dashboard should have repainted with the server card by now.
    await waitFor(() => /tcp/i.test(screen.slice(-20000)), 'the tcp card on screen');

    netget.close(conn);

    // UDP: a datagram server on the virtual network, a page-side socket, one round trip.
    const UDP_PORT = 5555;
    let udpStarted = null;
    netget.start_server(JSON.stringify({
        protocol: 'udp', port: UDP_PORT,
        instruction: 'Reply to every datagram with its text reversed.',
    }), (json) => { udpStarted = JSON.parse(json); });
    await waitFor(() => udpStarted !== null, 'start_server (udp) to answer');
    if (udpStarted.error) fail('start_server udp: ' + udpStarted.error);
    await waitFor(() => Array.from(netget.bound_udp_ports()).includes(UDP_PORT), `udp port ${UDP_PORT} to be bound`);

    const datagrams = [];
    const sock = netget.udp_open((bytes, fromPort) => datagrams.push([dec.decode(bytes), fromPort]));
    const before = requests.length;
    if (!netget.udp_send(sock, UDP_PORT, enc.encode('abc'))) fail('udp_send() reported the socket is gone');
    await waitFor(() => requests.length > before, 'the UDP model request', 20000);
    await waitFor(() => datagrams.some(([t]) => t.includes('cba')), 'the reversed datagram', 20000);
    if (datagrams[0][1] !== UDP_PORT) fail('reply came from port ' + datagrams[0][1] + ', expected ' + UDP_PORT);
    netget.udp_close(sock);

    console.log('ok: dashboard painted, tcp and udp servers started, virtual connections round-tripped through the model bridge');
    console.log('    requests:', requests.length, '| tcp received:', JSON.stringify(received.join('')), '| udp received:', JSON.stringify(datagrams), '| closed:', closed);
    process.exit(0);
} catch (e) {
    fail(e.message || String(e));
}
