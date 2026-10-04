// Headless Chrome's own WebTransport API against a NetGet server, pinned by serverCertificateHashes.
//   node browser.mjs PORT SHA256HEX      (Chrome from $NETGET_CHROME)
// Prints one JSON line: {"ok": true, ...} or {"ok": false, "error": ...}. Node 22+ (global WebSocket).
import {spawn} from 'node:child_process';
import {createServer} from 'node:http';
import {mkdtemp, readFile, rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';

const [port, hash] = process.argv.slice(2);
const chromePath = process.env.NETGET_CHROME;
if (!chromePath) throw new Error('NETGET_CHROME must name a Chrome or Chromium binary');
const bytes = hash.match(/../g).map(h => parseInt(h, 16));

// http://127.0.0.1 is a secure context, which WebTransport requires.
const page = createServer((_, res) => {res.writeHead(200, {'content-type': 'text/html'}); res.end('<!doctype html><title>NetGet WebTransport</title>')});
await new Promise(resolve => page.listen(0, '127.0.0.1', resolve));
const profile = await mkdtemp(join(tmpdir(), 'netget-webtransport-chrome-'));
const chrome = spawn(chromePath, ['--headless', '--no-sandbox', '--disable-gpu', '--disable-background-networking', '--remote-debugging-port=0',
  '--no-first-run', '--no-default-browser-check', `--user-data-dir=${profile}`, 'about:blank'], {stdio: ['ignore', 'ignore', 'pipe']});
let diagnostic = '';
chrome.stderr.on('data', chunk => {diagnostic = (diagnostic + chunk).slice(-8192)});
let socket;
const result = {ok: false};
try {
  let active;
  for (let i = 0; i < 200 && !active; i++) {
    try {active = (await readFile(join(profile, 'DevToolsActivePort'), 'utf8')).split('\n')} catch {await new Promise(r => setTimeout(r, 100))}
  }
  if (!active) throw new Error(`Chrome did not start: ${diagnostic}`);
  const targets = await (await fetch(`http://127.0.0.1:${active[0]}/json/list`)).json();
  socket = new WebSocket(targets.find(t => t.type === 'page').webSocketDebuggerUrl);
  await new Promise((resolve, reject) => {socket.onopen = resolve; socket.onerror = reject});
  const pending = new Map(); let id = 0;
  socket.onmessage = event => {const x = JSON.parse(event.data); if (x.id) {const p = pending.get(x.id); pending.delete(x.id); x.error ? p.reject(new Error(JSON.stringify(x.error))) : p.resolve(x.result)}};
  const rpc = (method, params = {}) => new Promise((resolve, reject) => {const key = ++id; pending.set(key, {resolve, reject}); socket.send(JSON.stringify({id: key, method, params}))});
  await rpc('Page.enable');
  const loaded = new Promise(resolve => {const prev = socket.onmessage; socket.onmessage = e => {prev(e); if (JSON.parse(e.data).method === 'Page.loadEventFired') resolve()}});
  await rpc('Page.navigate', {url: `http://127.0.0.1:${page.address().port}/`});
  await loaded;
  const expression = `(async () => {
    const enc = new TextEncoder(), dec = new TextDecoder();
    const readAll = async readable => {const r = readable.getReader(); let t = ''; for (;;) {const {value, done} = await r.read(); if (done) return t; t += dec.decode(value, {stream: true})}};
    const wt = new WebTransport('https://127.0.0.1:${port}/echo', {serverCertificateHashes: [{algorithm: 'sha-256', value: new Uint8Array(${JSON.stringify(bytes)})}]});
    await wt.ready;
    const bi = await wt.createBidirectionalStream();
    const w = bi.writable.getWriter(); await w.write(enc.encode('browser-bi')); await w.close();
    const biText = await readAll(bi.readable);
    const incoming = wt.incomingUnidirectionalStreams.getReader();
    const uni = await wt.createUnidirectionalStream();
    const uw = uni.getWriter(); await uw.write(enc.encode('browser-uni')); await uw.close();
    const uniText = await readAll((await incoming.read()).value);
    const dr = wt.datagrams.readable.getReader(), dw = wt.datagrams.writable.getWriter();
    let datagram;
    for (let i = 0; i < 20 && !datagram; i++) {
      await dw.write(enc.encode('browser-datagram'));
      datagram = await Promise.race([dr.read().then(x => dec.decode(x.value)), new Promise(r => setTimeout(r, 500))]);
    }
    let refused;
    try {const no = new WebTransport('https://127.0.0.1:${port}/forbidden', {serverCertificateHashes: [{algorithm: 'sha-256', value: new Uint8Array(${JSON.stringify(bytes)})}]}); await no.ready; refused = 'opened'} catch (e) {refused = e.name}
    wt.close({closeCode: 0, reason: 'browser done'});
    return {userAgent: navigator.userAgent, biText, uniText, datagram, refused};
  })()`;
  const timeout = new Promise((_, reject) => setTimeout(() => reject(new Error('browser session timed out')), 30000));
  const evaluated = await Promise.race([rpc('Runtime.evaluate', {expression, awaitPromise: true, returnByValue: true}), timeout]);
  if (evaluated.exceptionDetails) throw new Error(JSON.stringify(evaluated.exceptionDetails));
  Object.assign(result, evaluated.result.value, {ok: true});
} catch (e) {
  result.error = String(e && e.stack || e);
} finally {
  socket?.close();
  page.close();
  chrome.kill('SIGTERM');
  await Promise.race([new Promise(resolve => chrome.once('exit', resolve)), new Promise(resolve => setTimeout(resolve, 3000))]);
  if (chrome.exitCode === null && chrome.signalCode === null) chrome.kill('SIGKILL');
  await rm(profile, {recursive: true, force: true}).catch(() => {});
}
console.log(JSON.stringify(result));
process.exit(0);
