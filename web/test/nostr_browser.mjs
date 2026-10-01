// Real Chromium WebSocket + fetch interoperability with the native Nostr server.
// NETGET_BIN must name a build with nostr; nak signs a published fixture and verifies the
// relay's returned signature independently. No model or external network is used.
//
// NETGET_BIN=target/debug/netget node web/test/nostr_browser.mjs
// Install Playwright normally, or set PLAYWRIGHT_MODULE to its index.mjs in another prefix.
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { resolve, join } from 'node:path';

const { chromium } = await import(process.env.PLAYWRIGHT_MODULE || 'playwright');
const netgetBin = resolve(process.env.NETGET_BIN || 'target/debug/netget');
const nak = process.env.NAK_BIN || 'nak';
const relaySecret = '0f1e2d3c4b5a69788796a5b4c3d2e1f00112233445566778899aabbccddeeff0';
const sign = spawnSync(nak, ['event', '--sec', '1'.repeat(64), '-c', 'hello from Chromium'], { encoding: 'utf8' });
assert.equal(sign.status, 0, `nak must generate the fixture: ${sign.error || sign.stderr}`);
const published = JSON.parse(sign.stdout.trim());
const temp = mkdtempSync(join(tmpdir(), 'netget-nostr-browser-'));
const config = join(temp, 'relay.netget');
writeFileSync(config, JSON.stringify({ actions: [{
    type: 'open_server', protocol: 'nostr', host: '127.0.0.1', port: 0,
    startup_params: { relay_name: 'Chromium interop', relay_secret_key: relaySecret },
    event_handlers: [
        { event_pattern: 'nostr_event', handler: { type: 'static', actions: [{ type: 'accept_nostr_event' }] } },
        { event_pattern: 'nostr_req', handler: { type: 'static', actions: [{ type: 'send_nostr_events', events: [{ kind: 1, content: 'hello from NetGet', tags: [['t', 'browser']], created_at: 1700000100 }] }] } },
    ],
}] }));
let log = '';
let launchError;
const child = spawn(netgetBin, ['--load', config, '--ollama-url', 'http://127.0.0.1:1', '--model', 'unused', '--log-level', 'info', '--run-for', '90'], { cwd: temp, stdio: ['ignore', 'pipe', 'pipe'] });
child.stdout.on('data', (bytes) => { log += bytes; });
child.stderr.on('data', (bytes) => { log += bytes; });
child.on('error', (error) => { launchError = error; });
const origin = createServer((_req, res) => { res.writeHead(200, { 'content-type': 'text/html' }); res.end('<!doctype html><title>Nostr browser interoperability</title>'); });
let browser;
try {
    const deadline = Date.now() + 20000;
    let match;
    while (!(match = log.match(/Nostr relay listening on 127\.0\.0\.1:(\d+)/))) {
        if (launchError) throw launchError;
        if (child.exitCode !== null || Date.now() > deadline) throw new Error(`Nostr server did not start:\n${log}`);
        await new Promise((done) => setTimeout(done, 25));
    }
    const relayUrl = `http://127.0.0.1:${match[1]}`;
    await new Promise((done) => origin.listen(0, '127.0.0.1', done));
    browser = await chromium.launch({ headless: true, ...(process.env.BROWSER_EXECUTABLE_PATH ? { executablePath: process.env.BROWSER_EXECUTABLE_PATH } : {}) });
    const page = await browser.newPage();
    const errors = [];
    page.on('pageerror', (error) => errors.push(String(error)));
    await page.goto(`http://127.0.0.1:${origin.address().port}`);
    const result = await page.evaluate(async ({ relayUrl, published }) => {
        const need = (condition, message) => { if (!condition) throw new Error(message); };
        // A separate origin makes the browser enforce the relay's NIP-11 CORS headers.
        const infoResponse = await fetch(relayUrl, { headers: { Accept: 'application/nostr+json' } });
        need(infoResponse.ok, `NIP-11 HTTP ${infoResponse.status}`);
        need(infoResponse.headers.get('content-type')?.startsWith('application/nostr+json'), 'NIP-11 content type');
        const info = await infoResponse.json();
        need(info.name === 'Chromium interop' && info.supported_nips.includes(1) && info.supported_nips.includes(11), 'NIP-11 document');
        const socket = new WebSocket(relayUrl.replace('http:', 'ws:'));
        const frames = [];
        socket.addEventListener('message', (event) => frames.push(JSON.parse(event.data)));
        await new Promise((done, reject) => {
            const timer = setTimeout(() => reject(new Error('browser WebSocket upgrade timed out')), 10000);
            socket.addEventListener('open', () => { clearTimeout(timer); done(); }, { once: true });
            socket.addEventListener('error', () => { clearTimeout(timer); reject(new Error('browser WebSocket upgrade failed')); }, { once: true });
        });
        const waitFor = async (predicate, label) => {
            const deadline = Date.now() + 10000;
            while (!frames.some(predicate)) {
                if (Date.now() > deadline) throw new Error(`${label}: ${JSON.stringify(frames)}`);
                await new Promise((done) => setTimeout(done, 10));
            }
            return frames.find(predicate);
        };
        socket.send(JSON.stringify(['REQ', 'browser', { kinds: [1] }]));
        const eventFrame = await waitFor((frame) => frame[0] === 'EVENT' && frame[1] === 'browser', 'subscription EVENT');
        await waitFor((frame) => frame[0] === 'EOSE' && frame[1] === 'browser', 'subscription EOSE');
        need(eventFrame[2].content === 'hello from NetGet' && eventFrame[2].pubkey === info.self, 'relay event content and author');
        socket.send(JSON.stringify(['EVENT', published]));
        await waitFor((frame) => frame[0] === 'OK' && frame[1] === published.id && frame[2] === true, 'published EVENT accepted');
        await waitFor((frame) => frame[0] === 'EVENT' && frame[1] === 'browser' && frame[2].id === published.id, 'live event delivered');
        socket.send(JSON.stringify(['EVENT', { ...published, content: 'tampered content' }]));
        await waitFor((frame) => frame[0] === 'OK' && frame[1] === published.id && frame[2] === false, 'tampered EVENT rejected');
        socket.send(JSON.stringify(['CLOSE', 'browser']));
        const closed = new Promise((done, reject) => {
            const timer = setTimeout(() => reject(new Error('close handshake timed out')), 10000);
            socket.addEventListener('close', (event) => { clearTimeout(timer); done({ code: event.code, clean: event.wasClean }); }, { once: true });
        });
        socket.close(1000, 'browser test finished');
        return { info, relayEvent: eventFrame[2], frames, closed: await closed };
    }, { relayUrl, published });
    assert.deepEqual(errors, [], 'the page must not raise errors');
    assert.deepEqual(result.closed, { code: 1000, clean: true }, 'Chromium completed a clean WebSocket closing handshake');
    const verify = spawnSync(nak, ['verify'], { input: JSON.stringify(result.relayEvent), encoding: 'utf8' });
    assert.equal(verify.status, 0, `nak must verify NetGet's returned event: ${verify.error || verify.stderr}`);
    console.log(`ok: Chromium ${browser.version()} fetched NIP-11 across origins, subscribed, published, received live delivery, rejected tampering and closed cleanly; nak verified the relay signature`);
} catch (error) {
    console.error(log);
    throw error;
} finally {
    if (browser) await browser.close();
    await new Promise((done) => origin.close(done));
    if (child.exitCode === null && !launchError) {
        child.kill('SIGTERM');
        await new Promise((done) => child.once('exit', done));
    }
    rmSync(temp, { recursive: true, force: true });
}
