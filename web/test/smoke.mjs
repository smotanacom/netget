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
// started through `ClientForm` must complete real exchanges over the virtual loopback: the
// model's answer to `http_connected` becomes a request, the response reaches the client and is
// reported back to the model, a request injected through the client's command channel (the
// dashboard's `[ send ]`) comes back `Executed` with its status, a non-200 is reported as a
// status, and `https://` is refused with the reason.
//
// The peer for those exchanges is NetGet's HLS server, not its HTTP server, and the reason is
// worth knowing before changing it: hyper's HTTP/1 *server* cannot answer on wasm32 at all.
// `proto::h1::dispatch::poll_inner` calls `T::update_date()` on its very first poll, which
// reaches `std::time::SystemTime::now()` — and that panics on wasm32-unknown-unknown ("time not
// implemented on this platform"), taking the whole wasm instance down with `RuntimeError:
// unreachable`. It is inside hyper's own date-header cache, so `crate::utils::clock` cannot reach
// it. The same is true of every other hyper-based server in the browser build. hyper's *client*
// role never touches the clock, and HLS reads HTTP/1.1 with its own small parser, so the pair
// runs. The `http` server is started only to prove the button resolves and connects; nothing is
// ever sent to it.

import { readFileSync } from 'node:fs';
import init, { NetGet } from '../../site/demo/pkg/netget_web.js';
import { offeredActions, defaultActionIndex, newEntry, buildReply, buildAction } from '../../site/js/composer.js';

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
        // The HLS server: a playlist for .m3u8, a 404 for anything else.
        if (offered.has('hls_playlist_response')) {
            return answer([{ type: 'hls_playlist_response', target_duration: 6, segments: [{ uri: 'seg0.ts', duration: 6.0 }] }]);
        }
        if (offered.has('hls_segment_response')) {
            return answer([{ type: 'hls_segment_response', status_code: 404, content_type: 'text/plain', content: 'no such segment' }]);
        }
        // The HTTP client: fetch the playlist when connected, and report every response.
        if (offered.has('send_http_request')) {
            if (context.status_code !== undefined) {
                httpResponses.push(context);
                return answer([]);
            }
            if (context.base_url !== undefined) {
                httpConnected.push(context);
                return answer([{ type: 'send_http_request', method: 'GET', path: '/live.m3u8' }]);
            }
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

    // HTTP, the exchanges, against NetGet's HLS server (see the header for why not http).
    const HLS_PORT = 8090;
    let hlsStarted = null;
    netget.start_server(JSON.stringify({ protocol: 'hls', port: HLS_PORT, instruction: 'Serve a live playlist.' }), (json) => { hlsStarted = JSON.parse(json); });
    await waitFor(() => hlsStarted !== null, 'start_server (hls) to answer');
    if (hlsStarted.error) fail('start_server hls: ' + hlsStarted.error);
    await waitFor(() => netget.listening_ports().includes(HLS_PORT), `port ${HLS_PORT} to be listening`);

    let refused = null;
    netget.start_client(JSON.stringify({ protocol: 'http', remote_addr: `https://127.0.0.1:${HLS_PORT}`, instruction: 'Fetch the playlist.' }), (json) => { refused = JSON.parse(json); });
    await waitFor(() => refused !== null, 'start_client (https) to answer');
    if (!refused.error || !refused.error.includes('https:// is not available')) fail('an https:// client was not refused with the reason: ' + JSON.stringify(refused));

    let httpClient = null;
    netget.start_client(JSON.stringify({ protocol: 'http', remote_addr: `127.0.0.1:${HLS_PORT}`, instruction: 'Fetch /live.m3u8 and read the playlist.' }), (json) => { httpClient = JSON.parse(json); });
    await waitFor(() => httpClient !== null, 'start_client (http) to answer');
    if (httpClient.error) fail('start_client http: ' + httpClient.error);
    await waitFor(() => httpConnected.length > 0, 'the http_connected model request', 20000);
    await waitFor(() => httpResponses.length > 0, 'the playlist response reported to the model', 20000);
    const first = httpResponses[0];
    if (first.status_code !== 200) fail('the playlist came back ' + JSON.stringify(first));
    if (!String(first.body).includes('#EXTM3U') || !String(first.body).includes('seg0.ts')) fail('the playlist body did not reach the client: ' + JSON.stringify(first));
    const contentType = Object.entries(first.headers || {}).find(([k]) => k.toLowerCase() === 'content-type');
    if (!contentType) fail('the response headers did not reach the client: ' + JSON.stringify(first.headers));

    // The dashboard's [ send ] on that client, and a status that is not 200.
    let injected = null;
    netget.send_to_client(httpClient.id, JSON.stringify({ type: 'send_http_request', method: 'GET', path: '/missing.ts' }), (json) => { injected = JSON.parse(json); });
    await waitFor(() => injected !== null, 'send_to_client to answer', 45000);
    const detail = injected.Executed && injected.Executed.detail;
    if (!detail || !detail.includes('GET /missing.ts -> 404')) fail('send_to_client did not report the exchange: ' + JSON.stringify(injected));
    await waitFor(() => httpResponses.length > 1, 'the injected request\'s response reported to the model', 20000);
    if (httpResponses[1].status_code !== 404 || !String(httpResponses[1].body).includes('no such segment')) fail('the 404 did not reach the client intact: ' + JSON.stringify(httpResponses[1]));

    console.log('ok: dashboard painted, tcp, udp, http and hls servers started, virtual connections round-tripped through the model bridge');
    console.log('    composer: ' + composed[0].actions.length + ' actions offered, default ' + composed[0].entry.name + ', example bytes ' + JSON.stringify(composerReceived.join('')) + ' reached the peer');
    console.log('    http: [ + http client ] connected client #' + viaButton.id + ' to :' + HTTP_PORT + '; client #' + httpClient.id + ' read ' + first.status_code + ' (' + String(first.body).length + ' byte playlist) and, via [ send ], ' + detail.split('-> ')[1] + '; https refused');
    console.log('    requests:', requests.length, '| tcp received:', JSON.stringify(received.join('')), '| udp received:', JSON.stringify(datagrams), '| closed:', closed);
    process.exit(0);
} catch (e) {
    fail(e.message || String(e));
}
