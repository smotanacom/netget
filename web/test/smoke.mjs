// Headless smoke test for the browser bundle, run under Node.
//
// Drives site/demo/pkg/netget_web.js the way the page does, minus the DOM: boots NetGet,
// checks the dashboard paints into the fake terminal, starts a TCP server through
// `start_server`, connects to it over the virtual network, sends a line, answers the
// model request the page receives with a `send_tcp_data` action, and asserts the bytes come
// back on the connection. Exit code 0 means the whole path — dashboard, ServerForm, virtual
// sockets, protocol server, LLM bridge, action executor — works in wasm.
//
// It then answers one more TCP request the way the page's "you are the model" composer
// (site/js/composer.js) does before anyone edits a field: the request's offered `actions`,
// the composer's default pick, every field prefilled from that action's example. That reply
// must be accepted and the example's bytes must reach the peer.
//
//   ./web/build.sh && node web/test/smoke.mjs
//
// Then HTTP, from the client side. `[ + http client ]` on an HTTP server card must resolve and
// connect — the same form the button applies (`connect_client_to_server`) — and an HTTP client
// started through `ClientForm` must complete real exchanges over the virtual loopback against
// NetGet's own `http` server: the model's answer to `http_connected` becomes a request, the
// server's model answers it, the response reaches the client and is reported back to the model,
// a request injected through the client's command channel (the dashboard's `[ send ]`) comes
// back `Executed` with its status, a non-200 is reported as a status, and `https://` is refused
// with the reason.
//
// Then the hyper servers, from outside NetGet: Node's own HTTP/1.1 client (`node:http`, llhttp)
// and HTTP/2 client (`node:http2`, nghttp2) are handed a Duplex over `NetGet.connect()` and talk
// to `http`, `openapi`, `jsonrpc` and `rss` (hyper's HTTP/1 server) and `http2` (h2c, the `h2`
// crate). Each request is answered by the model bridge, and each response is checked for its
// status, its headers, its body and — for the hyper servers — a `Date` header within a day of
// now. That last check is the point: hyper's HTTP/1 dispatcher reads the clock on its first
// poll, `SystemTime::now()` panics on wasm32-unknown-unknown, and until vendor/hyper patched the
// date cache to read `Date.now()` the first request to any hyper server killed the page. A
// panic anywhere fails the run by name (see `panics` below), not as a timeout.
//
// Then the other HTTP-family clients, which in the browser speak through the same transport
// (src/client/http_fetch) as the http client: jsonrpc and elasticsearch through `[ + client ]`
// on NetGet's own server of their protocol, `[ send ]`, and the response found parked on the
// client (`intercepts()`), since a dashboard-created client routes it to the human; openapi
// (which needs the server's spec, so it goes through ClientForm) and bitcoin (whose RPC NetGet
// serves only as plain HTTP) routed to the model, which runs the first request itself and is
// shown each response. An https:// jsonrpc endpoint is refused with the reason.
//
// The model's thinking: the TCP echo's reply carries a `reasoning` field, as the page sends a
// thinking model's `<think>` text, and it must reach the dashboard. Before any of that,
// site/js/thinking.js — the page's split of a streamed answer into the "Thinking…" block and
// the answer — is checked at each point of a `<think>` stream, and a model that does not think
// is checked to have nothing split out of its text.

import { readFileSync } from 'node:fs';
import { Duplex } from 'node:stream';
import http from 'node:http';
import http2 from 'node:http2';
import init, { NetGet } from '../../site/demo/pkg/netget_web.js';
import { offeredActions, defaultActionIndex, newEntry, buildReply, buildAction } from '../../site/js/composer.js';
import { splitThinking } from '../../site/js/thinking.js';

// A Rust panic in the bundle is written by console_error_panic_hook and then aborts the wasm
// instance (`RuntimeError: unreachable`), after which every later call into it throws. Record
// either, so a waiting step fails naming the panic rather than timing out behind it.
const panics = [];
const consoleError = console.error.bind(console);
console.error = (...args) => {
    const text = args.map((a) => (a && a.stack) || String(a)).join(' ');
    if (/panicked at|RuntimeError/.test(text)) panics.push(text);
    consoleError(...args);
};
process.on('uncaughtException', (e) => { panics.push((e && e.stack) || String(e)); fail('uncaught exception: ' + ((e && e.stack) || e)); });
process.on('unhandledRejection', (e) => { panics.push((e && e.stack) || String(e)); fail('unhandled rejection: ' + ((e && e.stack) || e)); });

const wasm = readFileSync(new URL('../../site/demo/pkg/netget_web_bg.wasm', import.meta.url));
await init({ module_or_path: wasm });

const dec = new TextDecoder();
const enc = new TextEncoder();
let screen = '';
const requests = [];
const received = [];
// When set, requests are answered by the composer's default reply instead of the echo below.
let answerLikeTheComposer = false;
const composed = [];
// What the HTTP client reported to the model: its connect event and each response it read.
const httpConnected = [];
const httpResponses = [];
// Every request a hyper server reported to the model: { protocol, context }.
const serverRequests = [];
// What the model-routed HTTP-family clients reported to the model, by protocol: each
// response event's data.
const clientResponses = { openapi: [], bitcoin: [] };

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
            if (panics.length) return reject(new Error('the wasm instance panicked while waiting for ' + what + ':\n' + panics.join('\n')));
            if (pred()) return resolve();
            if (Date.now() > deadline) return reject(new Error('timed out waiting for ' + what));
            setTimeout(tick, 50);
        };
        tick();
    });
}

const PORT = 7000;
const SMOKE_REASONING = 'smokethought: the peer wants its line shouted back';  // a <think> block's text

// site/js/thinking.js, at each point of a stream.
function checkThinkingSplit() {
    const same = (got, want, what) => {
        if (JSON.stringify(got) !== JSON.stringify(want)) fail(`thinking.js: ${what}: got ${JSON.stringify(got)}, want ${JSON.stringify(want)}`);
    };
    const pick = (s) => ({ thinking: s.thinking, answer: s.answer, done: s.thinkingDone });
    const answer = '{"actions":[{"type":"send_telnet_line","line":"hi"}]}';
    const think = '<think>\nThey said hi.\nGreet them.\n</think>\n\n' + answer;
    same(pick(splitThinking('<thi', { thinks: true })), { thinking: '', answer: '', done: false }, 'a think tag opening');
    same(pick(splitThinking(think.slice(0, 22), { thinks: true })), { thinking: 'They said hi.', answer: '', done: false }, 'mid-thought');
    same(splitThinking(think.slice(0, think.indexOf('</think>') + 4), { thinks: true }).thinking, 'They said hi.\nGreet them.', 'a closing tag half written');
    same(pick(splitThinking(think.slice(0, think.indexOf('</think>') + 12), { thinks: true })), { thinking: 'They said hi.\nGreet them.', answer: '{"', done: true }, 'the answer begun');
    same(pick(splitThinking(think, { thinks: true, final: true })), { thinking: 'They said hi.\nGreet them.', answer, done: true }, 'the whole answer');
    // A template that opened the block in the prompt: only the closing tag is written.
    same(splitThinking('Hmm.\n</think>\n{"actions":[]}', { thinks: true, final: true }).answer, '{"actions":[]}', 'no opening tag');
    same(splitThinking('Hmm, still', { thinks: true }).thinking, 'Hmm, still', 'a thinking model before its closing tag');
    // Thinking switched off (`enable_thinking: false` prefills an empty block), or skipped.
    same(pick(splitThinking('<think>\n\n</think>\n\n' + answer, { thinks: true, final: true })), { thinking: '', answer, done: true }, 'an empty think block');
    same(splitThinking(answer.slice(0, 9), { thinks: true }).answer, answer.slice(0, 9), 'a thinking model answering directly');
    same(splitThinking('plain words', { thinks: true, final: true }).answer, 'plain words', 'unterminated, untagged text at the end is the answer');
    same(pick(splitThinking('<think>out of tokens', { thinks: true, final: true })), { thinking: 'out of tokens', answer: '', done: true }, 'a think block that never closed');
    // A model that does not think: nothing is split out, whatever the text holds.
    same(pick(splitThinking(think.slice(0, 30))), { thinking: '', answer: think.slice(0, 30), done: true }, 'a model that does not think');
}

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
        if (answerLikeTheComposer) {
            const actions = offeredActions(req);
            const entry = actions.length ? newEntry(actions[defaultActionIndex(actions)]) : null;
            const built = entry ? buildReply(req, actions, [entry]) : { ok: false };
            composed.push({ req, actions, entry, built });
            return JSON.stringify(built.ok ? built.reply : { error: 'the composer could not build a reply' });
        }
        // The network-event path sends one flattened prompt whose user turn carries
        // "Context data:\n{...}" with the connection id and the received text.
        const user = req.messages.find((m) => m.role === 'user') || req.messages[req.messages.length - 1];
        const ctx = user.content.indexOf('Context data:');
        let context = {};
        if (ctx >= 0) {
            try { context = JSON.parse(user.content.slice(ctx + 'Context data:'.length)); } catch (e) { /* prompt-only */ }
        }
        // A client's prompt carries its event as "Event: <id>\nData: {...}" instead.
        const withEvent = req.messages.map((m) => m.content).find((c) => c.includes('\nData: {'));
        if (ctx < 0 && withEvent) {
            try { context = JSON.parse(withEvent.slice(withEvent.lastIndexOf('\nData: ') + '\nData: '.length)); } catch (e) { /* prompt-only */ }
        }
        const offered = new Set((req.actions || []).map((a) => a.name));
        const answer = (actions) => JSON.stringify({ content: JSON.stringify({ actions }), prompt_tokens: 10, completion_tokens: 5 });
        // The hyper servers. Each answer is built from the request the server reported, so a
        // response that reaches the client proves the request's fields reached the model.
        if (offered.has('send_http_response')) {
            serverRequests.push({ protocol: 'http', context });
            if (context.path === '/hello.txt') {
                return answer([{ type: 'send_http_response', status: 200, headers: { 'Content-Type': 'text/plain' }, body: 'hello from the http server' }]);
            }
            if (context.method === 'POST' && context.path === '/echo') {
                return answer([{ type: 'send_http_response', status: 201, headers: { 'Content-Type': 'text/plain', 'X-Smoke': 'echoed' }, body: String(context.body).toUpperCase() }]);
            }
            // Bitcoin Core's RPC is JSON-RPC 1.0 over HTTP POST; NetGet has no server of it
            // (its `bitcoin` server speaks the P2P wire protocol), so the http server answers
            // the shape bitcoind would.
            if (context.method === 'POST' && String(context.body).includes('"jsonrpc":"1.0"')) {
                const call = JSON.parse(context.body);
                return answer([{ type: 'send_http_response', status: 200, headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({ result: { chain: 'regtest', blocks: 101, called: call.method }, error: null, id: call.id }) }]);
            }
            if (context.path === '/') {
                return answer([{ type: 'send_http_response', status: 200, headers: { 'Content-Type': 'text/html' }, body: '<h1>smoke</h1><p>accept=' + (context.headers || {}).accept + '</p>' }]);
            }
            return answer([{ type: 'send_http_response', status: 404, headers: { 'Content-Type': 'text/plain' }, body: 'no such page: ' + context.path }]);
        }
        if (offered.has('send_openapi_response')) {
            serverRequests.push({ protocol: 'openapi', context });
            return answer([{ type: 'send_openapi_response', status_code: 200, headers: { 'content-type': 'application/json' }, body: JSON.stringify([{ id: 1, title: 'Buy milk', path: context.path }]) }]);
        }
        if (offered.has('jsonrpc_success')) {
            serverRequests.push({ protocol: 'jsonrpc', context });
            if (context.method === 'add' && Array.isArray(context.params)) {
                return answer([{ type: 'jsonrpc_success', result: context.params.reduce((a, b) => a + b, 0) }]);
            }
            return answer([{ type: 'jsonrpc_error', code: -32601, message: 'Method not found: ' + context.method }]);
        }
        if (offered.has('generate_rss_feed')) {
            serverRequests.push({ protocol: 'rss', context });
            return answer([{ type: 'generate_rss_feed', title: 'Smoke News', link: 'http://127.0.0.1/news.xml', description: 'Served from ' + context.path,
                items: [{ title: 'First post', link: 'http://127.0.0.1/1', description: 'Hello from the browser build' }] }]);
        }
        if (offered.has('send_http2_response')) {
            serverRequests.push({ protocol: 'http2', context });
            return answer([{ type: 'send_http2_response', status: 200, headers: { 'content-type': 'text/plain', 'x-smoke': 'h2' }, body: 'h2 says hi to ' + context.uri }]);
        }
        if (offered.has('send_elasticsearch_response')) {
            serverRequests.push({ protocol: 'elasticsearch', context });
            return answer([{ type: 'send_elasticsearch_response', status_code: 200,
                body: JSON.stringify({ took: 1, timed_out: false, hits: { total: { value: 1, relation: 'eq' },
                    hits: [{ _index: context.index, _id: 'dune', _source: { title: 'Dune', asked: context.operation } }] } }) }]);
        }
        // The OpenAPI client: run the spec's operation when connected, report its response.
        if (offered.has('execute_operation')) {
            if (context.status_code !== undefined) {
                clientResponses.openapi.push(context);
                return answer([]);
            }
            if (context.operations !== undefined) {
                return answer([{ type: 'execute_operation', operation_id: 'listTodos', path_params: {}, query_params: { owner: 'smoke test' } }]);
            }
        }
        // The Bitcoin RPC client: nothing on connect; report every response.
        if (offered.has('get_blockchain_info')) {
            if (context.status_code !== undefined) clientResponses.bitcoin.push(context);
            return answer([]);
        }
        // The HTTP client: fetch a page when connected, and report every response.
        if (offered.has('send_http_request')) {
            if (context.status_code !== undefined) {
                httpResponses.push(context);
                return answer([]);
            }
            if (context.base_url !== undefined) {
                httpConnected.push(context);
                return answer([{ type: 'send_http_request', method: 'GET', path: '/hello.txt' }]);
            }
        }
        const text = String(context.data ?? context.data_preview ?? context.content ?? context.text ?? '');
        // Answer in the vocabulary of whichever server asked: TCP echoes uppercased, UDP
        // reverses.
        const isUdp = req.messages.some((m) => m.content.includes('send_udp_response'));
        if (isUdp) {
            const reply = { actions: [{ type: 'send_udp_response', data: [...text.trim()].reverse().join(''), encoding: 'text' }] };
            return JSON.stringify({ content: JSON.stringify(reply), prompt_tokens: 10, completion_tokens: 5 });
        }
        const reply = { actions: [{ type: 'send_tcp_data', data: text.toUpperCase() }] };
        return JSON.stringify({ content: JSON.stringify(reply), reasoning: SMOKE_REASONING, prompt_tokens: 10, completion_tokens: 5 });
    },
});

// An OpenAPI document with one route, for the openapi server's `spec` startup parameter.
const TODO_SPEC = `openapi: 3.1.0
info:
  title: Todo API
  version: 1.0.0
paths:
  /todos:
    get:
      operationId: listTodos
      responses:
        '200':
          description: the todo list
`;

// Start a server through the page's start_server and wait until its port listens.
async function startServer(spec) {
    let started = null;
    netget.start_server(JSON.stringify(spec), (json) => { started = JSON.parse(json); });
    await waitFor(() => started !== null, `start_server (${spec.protocol}) to answer`);
    if (started.error) fail(`start_server ${spec.protocol}: ` + started.error);
    await waitFor(() => netget.listening_ports().includes(spec.port), `${spec.protocol} to listen on ${spec.port}`);
    return started;
}

// A Node Duplex over one virtual-loopback connection, for Node's own HTTP clients.
function virtualSocket(port) {
    let id = null;
    const sock = new Duplex({
        read() {},
        write(chunk, _encoding, callback) {
            callback(netget.send(id, chunk) ? null : new Error(`the virtual connection to :${port} is gone`));
        },
        destroy(err, callback) {
            if (id !== null) netget.close(id);
            callback(err);
        },
    });
    id = netget.connect(port, (bytes) => sock.push(Buffer.from(bytes)), () => sock.push(null));
    return sock;
}

// One HTTP/1.1 exchange through node:http over the virtual loopback.
function httpRequest(port, { method = 'GET', path = '/', headers = {}, body } = {}) {
    return new Promise((resolve, reject) => {
        const timer = setTimeout(() => { req.destroy(new Error(`no response to ${method} ${path} on :${port} within 20s`)); }, 20000);
        const req = http.request({ host: '127.0.0.1', port, method, path, headers, createConnection: () => virtualSocket(port) }, (res) => {
            const chunks = [];
            res.on('data', (c) => chunks.push(c));
            res.on('end', () => { clearTimeout(timer); resolve({ status: res.statusCode, headers: res.headers, body: Buffer.concat(chunks).toString('utf8') }); });
            res.on('error', (e) => { clearTimeout(timer); reject(e); });
        });
        req.on('error', (e) => { clearTimeout(timer); reject(new Error(`${method} ${path} on :${port}: ${e.message}`)); });
        if (body !== undefined) req.write(body);
        req.end();
    });
}

// One HTTP/2 exchange (prior-knowledge h2c) through node:http2 over the virtual loopback.
function http2Request(port, path) {
    return new Promise((resolve, reject) => {
        const session = http2.connect(`http://127.0.0.1:${port}`, { createConnection: () => virtualSocket(port) });
        const timer = setTimeout(() => { session.destroy(new Error(`no HTTP/2 response to ${path} on :${port} within 20s`)); }, 20000);
        const done = (err, value) => { clearTimeout(timer); session.close(); err ? reject(err) : resolve(value); };
        session.on('error', (e) => done(new Error(`h2 session to :${port}: ${e.message}`)));
        const stream = session.request({ ':method': 'GET', ':path': path });
        let headers = null;
        const chunks = [];
        stream.on('response', (h) => { headers = h; });
        stream.on('data', (c) => chunks.push(c));
        stream.on('end', () => done(null, { status: headers && headers[':status'], headers: headers || {}, body: Buffer.concat(chunks).toString('utf8') }));
        stream.on('error', (e) => done(new Error(`h2 stream ${path} on :${port}: ${e.message}`)));
        stream.end();
    });
}

// A hyper server's Date header: present, an HTTP date, and today's — the clock the vendored
// hyper reads on wasm32 is the page's, not a placeholder.
const dates = [];
function checkDate(value, what) {
    if (!value) fail(what + ': no Date header');
    const at = Date.parse(value);
    if (!/^[A-Z][a-z]{2}, \d{2} [A-Z][a-z]{2} \d{4} \d{2}:\d{2}:\d{2} GMT$/.test(value) || Number.isNaN(at)) fail(what + ': Date is not an HTTP date: ' + JSON.stringify(value));
    if (Math.abs(at - Date.now()) > 24 * 3600 * 1000) fail(what + ': Date is not today: ' + value);
    dates.push(value);
}

try {
    checkThinkingSplit();
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
    // The reply's thinking reached the dashboard (its stream column).
    await waitFor(() => screen.includes('smokethought'), 'the model thinking on the dashboard', 10000);

    // The dashboard should have repainted with the server card by now.
    await waitFor(() => /tcp/i.test(screen.slice(-20000)), 'the tcp card on screen');

    // The page switches who answers with set_model: the next request names that model.
    netget.set_models(JSON.stringify(['smoke-test', 'smoke-switched']));
    netget.set_model('smoke-switched');

    netget.close(conn);

    // The composer: answer a fresh TCP request with its default, example-prefilled action.
    answerLikeTheComposer = true;
    const composerReceived = [];
    const conn2 = netget.connect(PORT, (bytes) => composerReceived.push(dec.decode(bytes)), () => {});
    if (!netget.send(conn2, enc.encode('compose me\n'))) fail('send() on the composer connection failed');
    await waitFor(() => composed.length > 0, 'the composer-answered request', 20000);
    const c = composed[0];
    if (!c.actions.length) fail('the request offers no actions: ' + JSON.stringify(Object.keys(c.req)));
    if (!c.actions.some((a) => a.example && typeof a.example === 'object' && Object.keys(a.example).length)) fail('no offered action carries an example');
    if (!c.actions.every((a) => Array.isArray(a.parameters))) fail('an offered action has no parameter list');
    if (c.req.model !== 'smoke-switched') fail('set_model did not reach the request: model is ' + JSON.stringify(c.req.model));
    if (c.entry.name !== 'send_tcp_data') fail('the composer would preselect ' + c.entry.name + ', not send_tcp_data; offered: ' + c.actions.map((a) => a.name).join(', '));
    if (!c.built.ok) fail('the composer could not build a reply from the example: ' + JSON.stringify(c.built.errors));
    const sent = buildAction(c.entry).value;
    const example = c.actions.find((a) => a.name === 'send_tcp_data').example;
    if (JSON.stringify(sent) !== JSON.stringify(example)) fail('the prefilled action differs from its example: ' + JSON.stringify(sent) + ' vs ' + JSON.stringify(example));
    await waitFor(() => composerReceived.join('') === sent.data, 'the example bytes (' + JSON.stringify(sent.data) + ') on the wire', 20000);
    answerLikeTheComposer = false;
    netget.close(conn2);

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

    // HTTP, the button: an http server's card offers [ + http client ], and pressing it (the
    // same form, through connect_client_to_server) connects a client to it.
    const HTTP_PORT = 8080;
    let httpStarted = null;
    netget.start_server(JSON.stringify({ protocol: 'http', port: HTTP_PORT, instruction: 'Serve a tiny site.' }), (json) => { httpStarted = JSON.parse(json); });
    await waitFor(() => httpStarted !== null, 'start_server (http) to answer');
    if (httpStarted.error) fail('start_server http: ' + httpStarted.error);
    await waitFor(() => netget.listening_ports().includes(HTTP_PORT), `port ${HTTP_PORT} to be listening`);
    // Tall enough that every card, with its open peers section, is on screen at once.
    netget.resize(160, 200);
    const screenBefore = screen.length;
    await waitFor(() => screen.slice(screenBefore).includes('+ http client'), 'the [ + http client ] button on the http card');
    let viaButton = null;
    netget.connect_client_to_server(httpStarted.id, (json) => { viaButton = JSON.parse(json); });
    await waitFor(() => viaButton !== null, 'connect_client_to_server to answer');
    if (viaButton.error) fail('[ + http client ]: ' + viaButton.error);
    if (viaButton.protocol !== 'HTTP' || viaButton.remote_addr !== `127.0.0.1:${HTTP_PORT}`) fail('[ + http client ] made the wrong client: ' + JSON.stringify(viaButton));
    let clients = null;
    netget.clients((json) => { clients = JSON.parse(json); });
    await waitFor(() => clients !== null, 'clients()');
    const buttonClient = clients.find((c) => c.id === viaButton.id);
    if (!buttonClient || buttonClient.status !== 'Connected') fail('the [ + http client ] client is not connected: ' + JSON.stringify(clients));

    // HTTP, the exchanges: NetGet's HTTP client against NetGet's own hyper-based http server,
    // both driven by the model bridge.
    let refused = null;
    netget.start_client(JSON.stringify({ protocol: 'http', remote_addr: `https://127.0.0.1:${HTTP_PORT}`, instruction: 'Fetch /hello.txt.' }), (json) => { refused = JSON.parse(json); });
    await waitFor(() => refused !== null, 'start_client (https) to answer');
    if (!refused.error || !refused.error.includes('https:// is not available')) fail('an https:// client was not refused with the reason: ' + JSON.stringify(refused));

    let httpClient = null;
    netget.start_client(JSON.stringify({ protocol: 'http', remote_addr: `127.0.0.1:${HTTP_PORT}`, instruction: 'Fetch /hello.txt and read it.' }), (json) => { httpClient = JSON.parse(json); });
    await waitFor(() => httpClient !== null, 'start_client (http) to answer');
    if (httpClient.error) fail('start_client http: ' + httpClient.error);
    await waitFor(() => httpConnected.length > 0, 'the http_connected model request', 20000);
    await waitFor(() => httpResponses.length > 0, 'the /hello.txt response reported to the model', 20000);
    const first = httpResponses[0];
    if (first.status_code !== 200) fail('/hello.txt came back ' + JSON.stringify(first));
    if (String(first.body) !== 'hello from the http server') fail('the http server\'s body did not reach the client: ' + JSON.stringify(first));
    const headerOf = (headers, name) => Object.entries(headers || {}).find(([k]) => k.toLowerCase() === name)?.[1];
    if (headerOf(first.headers, 'content-type') !== 'text/plain') fail('the response headers did not reach the client: ' + JSON.stringify(first.headers));
    checkDate(headerOf(first.headers, 'date'), 'the http server, read by NetGet\'s http client');

    // The dashboard's [ send ] on that client, and a status that is not 200.
    let injected = null;
    netget.send_to_client(httpClient.id, JSON.stringify({ type: 'send_http_request', method: 'GET', path: '/missing' }), (json) => { injected = JSON.parse(json); });
    await waitFor(() => injected !== null, 'send_to_client to answer', 45000);
    const detail = injected.Executed && injected.Executed.detail;
    if (!detail || !detail.includes('GET /missing -> 404')) fail('send_to_client did not report the exchange: ' + JSON.stringify(injected));
    await waitFor(() => httpResponses.length > 1, 'the injected request\'s response reported to the model', 20000);
    if (httpResponses[1].status_code !== 404 || !String(httpResponses[1].body).includes('no such page: /missing')) fail('the 404 did not reach the client intact: ' + JSON.stringify(httpResponses[1]));

    // The hyper servers, spoken to by Node's own HTTP clients over the virtual loopback.
    const hyper = {};

    // http: a GET and a POST with a body, through node:http (llhttp).
    const root = await httpRequest(HTTP_PORT, { path: '/', headers: { accept: 'text/html' } });
    if (root.status !== 200 || root.headers['content-type'] !== 'text/html' || root.body !== '<h1>smoke</h1><p>accept=text/html</p>') fail('http GET /: ' + JSON.stringify(root));
    checkDate(root.headers.date, 'http GET /');
    const echo = await httpRequest(HTTP_PORT, { method: 'POST', path: '/echo', headers: { 'content-type': 'text/plain' }, body: 'shout this' });
    if (echo.status !== 201 || echo.headers['x-smoke'] !== 'echoed' || echo.body !== 'SHOUT THIS') fail('http POST /echo: ' + JSON.stringify(echo));
    checkDate(echo.headers.date, 'http POST /echo');
    hyper.http = `GET / ${root.status} (${root.body.length}B), POST /echo ${echo.status} ${JSON.stringify(echo.body)}`;

    // openapi: a spec-routed GET.
    const OPENAPI_PORT = 8081;
    await startServer({ protocol: 'openapi', port: OPENAPI_PORT, instruction: 'Serve the todo API.', startup_params: { spec: TODO_SPEC } });
    const todos = await httpRequest(OPENAPI_PORT, { path: '/todos', headers: { accept: 'application/json' } });
    if (todos.status !== 200 || !String(todos.headers['content-type']).startsWith('application/json')) fail('openapi GET /todos: ' + JSON.stringify(todos));
    const todoList = JSON.parse(todos.body);
    if (todoList[0]?.title !== 'Buy milk' || todoList[0]?.path !== '/todos') fail('openapi GET /todos body: ' + todos.body);
    if (!serverRequests.some((r) => r.protocol === 'openapi' && r.context.matched_route)) fail('the openapi request reached the model without its matched route: ' + JSON.stringify(serverRequests.filter((r) => r.protocol === 'openapi')));
    checkDate(todos.headers.date, 'openapi GET /todos');
    hyper.openapi = `GET /todos ${todos.status} ${todos.body}`;

    // jsonrpc: a call answered with a result, and one answered with an error.
    const JSONRPC_PORT = 8082;
    const jsonrpcServer = await startServer({ protocol: 'jsonrpc', port: JSONRPC_PORT, instruction: 'Implement add(a, b).' });
    const rpc = (payload) => httpRequest(JSONRPC_PORT, { method: 'POST', path: '/', headers: { 'content-type': 'application/json' }, body: JSON.stringify(payload) });
    const sum = await rpc({ jsonrpc: '2.0', method: 'add', params: [2, 3], id: 7 });
    const sumBody = JSON.parse(sum.body);
    if (sum.status !== 200 || sumBody.jsonrpc !== '2.0' || sumBody.result !== 5 || sumBody.id !== 7) fail('jsonrpc add: ' + JSON.stringify(sum));
    checkDate(sum.headers.date, 'jsonrpc add');
    const missing = await rpc({ jsonrpc: '2.0', method: 'nope', id: 'x' });
    const missingBody = JSON.parse(missing.body);
    if (missingBody.error?.code !== -32601 || missingBody.id !== 'x') fail('jsonrpc error: ' + JSON.stringify(missing));
    hyper.jsonrpc = `add(2,3) -> ${sum.body}; nope -> error ${missingBody.error.code}`;

    // rss: the feed the model described, rendered to XML by the server.
    const RSS_PORT = 8083;
    await startServer({ protocol: 'rss', port: RSS_PORT, instruction: 'Serve a news feed.' });
    const feed = await httpRequest(RSS_PORT, { path: '/news.xml', headers: { accept: 'application/rss+xml' } });
    if (feed.status !== 200 || !/xml/.test(String(feed.headers['content-type']))) fail('rss GET /news.xml: ' + JSON.stringify(feed));
    for (const needle of ['<rss', '<title>Smoke News</title>', '<description>Served from /news.xml</description>', '<title>First post</title>']) {
        if (!feed.body.includes(needle)) fail('rss feed lacks ' + needle + ': ' + feed.body);
    }
    checkDate(feed.headers.date, 'rss GET /news.xml');
    hyper.rss = `GET /news.xml ${feed.status} (${feed.body.length}B of RSS)`;

    // http2: prior-knowledge h2c through node:http2 (nghttp2). NetGet's http2 server is the
    // `h2` crate rather than hyper, and it sets no Date header.
    const HTTP2_PORT = 8084;
    await startServer({ protocol: 'http2', port: HTTP2_PORT, instruction: 'Greet every request.' });
    const h2 = await http2Request(HTTP2_PORT, '/greet?who=smoke');
    if (h2.status !== 200 || h2.headers['x-smoke'] !== 'h2' || h2.body !== 'h2 says hi to /greet?who=smoke') fail('http2 GET /greet: ' + JSON.stringify(h2));
    hyper.http2 = `GET /greet?who=smoke ${h2.status} ${JSON.stringify(h2.body)}`;

    // The HTTP-family clients in the browser: each speaks through the shared transport
    // (src/client/http_fetch) over the virtual loopback to NetGet's own server of its protocol.
    const webClients = {};
    // An event parked for the human on a client, as the dashboard's "waiting for YOUR answer"
    // rows show it. A client the dashboard creates routes everything after its connect event
    // to `manual`, so that is where the response lands.
    async function parkedOn(clientId, eventType) {
        let found = null;
        await waitFor(() => {
            netget.intercepts((json) => {
                const rows = JSON.parse(json);
                found = rows.find((r) => r.owner.kind === 'client' && r.owner.id === clientId && r.event_type === eventType) || found;
            });
            return found !== null;
        }, `a ${eventType} parked on client #${clientId}`, 20000);
        return found;
    }
    async function connectViaButton(serverId, protocol) {
        let made = null;
        netget.connect_client_to_server(serverId, (json) => { made = JSON.parse(json); });
        await waitFor(() => made !== null, `[ + ${protocol} client ] to answer`);
        if (made.error) fail(`[ + ${protocol} client ]: ` + made.error);
        let rows = null;
        netget.clients((json) => { rows = JSON.parse(json); });
        await waitFor(() => rows !== null, 'clients()');
        const row = rows.find((c) => c.id === made.id);
        if (!row || row.status !== 'Connected') fail(`the [ + ${protocol} client ] client is not connected: ` + JSON.stringify(rows));
        return made;
    }
    async function send(clientId, action) {
        let outcome = null;
        netget.send_to_client(clientId, JSON.stringify(action), (json) => { outcome = JSON.parse(json); });
        await waitFor(() => outcome !== null, `send_to_client(${action.type}) to answer`, 45000);
        const detail = outcome.Executed && outcome.Executed.detail;
        if (!detail) fail(`[ send ] ${action.type} was not executed: ` + JSON.stringify(outcome));
        return detail;
    }

    // jsonrpc: [ + JSON-RPC client ] on the jsonrpc server's card, then [ send ] add(2, 3). The
    // server's model computes 5; the client reads it and parks the response for the human.
    const rpcClient = await connectViaButton(jsonrpcServer.id, 'jsonrpc');
    if (rpcClient.remote_addr !== `127.0.0.1:${JSONRPC_PORT}`) fail('[ + jsonrpc client ] made the wrong client: ' + JSON.stringify(rpcClient));
    const rpcDetail = await send(rpcClient.id, { type: 'send_jsonrpc_request', method: 'add', params: [2, 3], id: 41 });
    if (!rpcDetail.includes('HTTP 200') || !rpcDetail.includes('JSON-RPC response received')) fail('jsonrpc [ send ]: ' + rpcDetail);
    const rpcParked = await parkedOn(rpcClient.id, 'jsonrpc_response_received');
    if (rpcParked.event_data?.result !== 5 || rpcParked.event_data?.id !== 41) fail('the jsonrpc response did not reach the client intact: ' + JSON.stringify(rpcParked));
    webClients.jsonrpc = `[ + client ] #${rpcClient.id}, [ send ] add(2,3) -> ${rpcDetail}; parked result ${rpcParked.event_data.result}`;

    // The transport has no TLS: an https:// endpoint is refused at connect, naming why.
    let rpcRefused = null;
    netget.start_client(JSON.stringify({ protocol: 'jsonrpc', remote_addr: `https://127.0.0.1:${JSONRPC_PORT}`, instruction: 'Call add.' }), (json) => { rpcRefused = JSON.parse(json); });
    await waitFor(() => rpcRefused !== null, 'start_client (jsonrpc, https) to answer');
    if (!rpcRefused.error || !rpcRefused.error.includes('https:// is not available')) fail('an https:// jsonrpc client was not refused with the reason: ' + JSON.stringify(rpcRefused));

    // elasticsearch: NetGet's Elasticsearch server, [ + client ], [ send ] a search.
    const ES_PORT = 8085;
    const esServer = await startServer({ protocol: 'elasticsearch', port: ES_PORT, instruction: 'Serve a small library index.' });
    const esClient = await connectViaButton(esServer.id, 'elasticsearch');
    const esDetail = await send(esClient.id, { type: 'search', index: 'books', query: { match: { title: 'dune' } } });
    if (!esDetail.includes('HTTP 200')) fail('elasticsearch [ send ]: ' + esDetail);
    const esAsked = serverRequests.filter((r) => r.protocol === 'elasticsearch');
    if (!esAsked.some((r) => r.context.index === 'books' && String(r.context.request_body).includes('dune'))) fail('the search did not reach the Elasticsearch server\'s model: ' + JSON.stringify(esAsked));
    const esParked = await parkedOn(esClient.id, 'elasticsearch_response_received');
    if (!JSON.stringify(esParked.event_data).includes('"title":"Dune"')) fail('the Elasticsearch hits did not reach the client: ' + JSON.stringify(esParked));
    webClients.elasticsearch = `[ + client ] #${esClient.id}, [ send ] search -> ${esDetail}; parked hit "Dune"`;

    // openapi: the client needs the spec, which `[ + client ]` cannot know (the dashboard's form
    // asks for it), so it is started through ClientForm with the server's spec and routed to the
    // model: connected -> the model runs listTodos -> the openapi server's model answers -> the
    // response is reported back to the model. Then [ send ] the same operation.
    let oaClient = null;
    netget.start_client(JSON.stringify({ protocol: 'openapi', remote_addr: `127.0.0.1:${OPENAPI_PORT}`, instruction: 'List the todos.', startup_params: { spec: TODO_SPEC } }), (json) => { oaClient = JSON.parse(json); });
    await waitFor(() => oaClient !== null, 'start_client (openapi) to answer');
    if (oaClient.error) fail('start_client openapi: ' + oaClient.error);
    await waitFor(() => clientResponses.openapi.length > 0, 'the openapi client\'s response reported to the model', 20000);
    const oaFirst = clientResponses.openapi[0];
    if (oaFirst.status_code !== 200 || JSON.parse(oaFirst.body)[0]?.title !== 'Buy milk') fail('the openapi response did not reach the client: ' + JSON.stringify(oaFirst));
    if (!serverRequests.some((r) => r.protocol === 'openapi' && JSON.stringify(r.context).includes('smoke'))) fail('the model\'s query parameter did not reach the openapi server: ' + JSON.stringify(serverRequests.filter((r) => r.protocol === 'openapi')));
    const oaDetail = await send(oaClient.id, { type: 'execute_operation', operation_id: 'listTodos', path_params: {}, query_params: {} });
    await waitFor(() => clientResponses.openapi.length > 1, 'the injected operation\'s response reported to the model', 20000);
    webClients.openapi = `ClientForm #${oaClient.id}: model ran listTodos -> ${oaFirst.status_code} ${oaFirst.body}; [ send ] -> ${oaDetail}`;

    // bitcoin: Bitcoin Core RPC is JSON-RPC over HTTP, and NetGet's `bitcoin` server is the P2P
    // protocol, so the peer is NetGet's http server answering as bitcoind would. The RPC
    // credentials go out as Basic auth, which the server's request must show.
    let btcClient = null;
    netget.start_client(JSON.stringify({ protocol: 'bitcoin', remote_addr: `127.0.0.1:${HTTP_PORT}`, instruction: 'Watch the chain.', startup_params: { rpc_user: 'smoke', rpc_password: 'pw' } }), (json) => { btcClient = JSON.parse(json); });
    await waitFor(() => btcClient !== null, 'start_client (bitcoin) to answer');
    if (btcClient.error) fail('start_client bitcoin: ' + btcClient.error);
    const btcDetail = await send(btcClient.id, { type: 'get_blockchain_info' });
    if (!btcDetail.includes("'getblockchaininfo' -> HTTP 200 (result)")) fail('bitcoin [ send ]: ' + btcDetail);
    await waitFor(() => clientResponses.bitcoin.length > 0, 'the bitcoin RPC response reported to the model', 20000);
    const btc = clientResponses.bitcoin[0];
    if (btc.result?.blocks !== 101 || btc.result?.called !== 'getblockchaininfo') fail('the RPC result did not reach the client: ' + JSON.stringify(btc));
    const rpcSeen = serverRequests.find((r) => r.protocol === 'http' && String(r.context.body).includes('getblockchaininfo'));
    const auth = rpcSeen && headerOf(rpcSeen.context.headers, 'authorization');
    if (auth !== 'Basic ' + Buffer.from('smoke:pw').toString('base64')) fail('the RPC credentials did not arrive as Basic auth: ' + JSON.stringify(rpcSeen && rpcSeen.context.headers));
    webClients.bitcoin = `ClientForm #${btcClient.id} -> http :${HTTP_PORT}: [ send ] ${btcDetail}; blocks ${btc.result.blocks}; Basic auth arrived`;

    if (panics.length) fail('the wasm instance panicked:\n' + panics.join('\n'));

    console.log('ok: dashboard painted, tcp, udp, http, openapi, jsonrpc, rss and http2 servers started, virtual connections round-tripped through the model bridge');
    console.log('    thinking: a reply\'s reasoning reached the dashboard; thinking.js split <think> streams and left other models\' text whole');
    console.log('    composer: ' + composed[0].actions.length + ' actions offered, default ' + composed[0].entry.name + ', example bytes ' + JSON.stringify(composerReceived.join('')) + ' reached the peer');
    console.log('    http client: [ + http client ] connected client #' + viaButton.id + ' to :' + HTTP_PORT + '; client #' + httpClient.id + ' read ' + first.status_code + ' ' + JSON.stringify(first.body) + ' and, via [ send ], ' + detail.split('-> ')[1] + '; https refused');
    for (const [name, line] of Object.entries(hyper)) console.log(`    ${name} (node client): ${line}`);
    for (const [name, line] of Object.entries(webClients)) console.log(`    ${name} client: ${line}`);
    console.log('    Date headers checked:', dates.length, '| latest:', dates[dates.length - 1]);
    console.log('    requests:', requests.length, '| tcp received:', JSON.stringify(received.join('')), '| udp received:', JSON.stringify(datagrams), '| closed:', closed);
    process.exit(0);
} catch (e) {
    fail(e.message || String(e));
}
