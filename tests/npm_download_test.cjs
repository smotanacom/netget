// Fault injection against local bytes and harmless fixture archives; never executes netget.
const assert = require('node:assert/strict');
const test = require('node:test');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');
const { spawnSync, spawn } = require('node:child_process');
const { once } = require('node:events');
const { download, fetchBinary, expectedChecksum } = require('../npm/netget/bin/netget.js');

function fixture() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'netget-download-test-'));
  const cache = path.join(root, 'cache');
  const stages = path.join(root, 'stages'); fs.mkdirSync(stages);
  const bin = process.platform === 'win32' ? 'netget.exe' : 'netget';
  fs.writeFileSync(path.join(root, bin), 'harmless fixture bytes');
  const archivePath = path.join(root, 'fixture.tar.gz');
  const result = spawnSync('tar', ['-czf', archivePath, '-C', root, bin]);
  assert.equal(result.status, 0, String(result.stderr));
  const archive = fs.readFileSync(archivePath);
  const name = 'netget-aarch64-apple-darwin.tar.gz';
  const checksum = crypto.createHash('sha256').update(archive).digest('hex');
  const fetchImpl = async url => new Response(url.endsWith('/SHA256SUMS') ? `${checksum}  ${name}\n` : archive);
  return { root, stages, cache, bin, archive, name, checksum, fetchImpl, cleanup() { fs.rmSync(root, { recursive: true, force: true }); } };
}

test('checksum selection rejects absent and ambiguous entries', () => {
  const hash = 'a'.repeat(64);
  assert.equal(expectedChecksum(`${hash} *archive.tar.gz\n`, 'archive.tar.gz'), hash);
  assert.throws(() => expectedChecksum('', 'archive.tar.gz'), /exactly one/);
  assert.throws(() => expectedChecksum(`${hash}  a\n${hash}  a`, 'a'), /exactly one/);
});

test('verified archive installs atomically and removes all staging files', async () => {
  const f = fixture();
  try {
    const binary = await fetchBinary('darwin-arm64', { cache: f.cache, temporaryRoot: f.stages, fetchImpl: f.fetchImpl });
    assert.equal(fs.readFileSync(binary, 'utf8'), 'harmless fixture bytes');
    assert.deepEqual(fs.readdirSync(f.stages), []);
    assert.deepEqual(fs.readdirSync(f.cache), [f.bin]);
  } finally { f.cleanup(); }
});

test('corrupt archives and absent manifests fail before extraction', async () => {
  const f = fixture();
  try {
    for (const fetchImpl of [
      async url => url.endsWith('SHA256SUMS') ? f.fetchImpl(url) : new Response('corrupted archive'),
      async () => new Response('missing', { status: 404 }),
    ]) {
      await assert.rejects(fetchBinary('darwin-arm64', { cache: f.cache, temporaryRoot: f.stages, fetchImpl }), /mismatch|manifest unavailable/);
      assert.deepEqual(fs.readdirSync(f.stages), []);
      assert.deepEqual(fs.readdirSync(f.cache), []);
    }
  } finally { f.cleanup(); }
});

test('rename failure removes cache staging and extracted temporary files', async () => {
  const f = fixture();
  const original = fs.renameSync;
  try {
    fs.renameSync = () => { throw new Error('injected rename failure'); };
    await assert.rejects(fetchBinary('darwin-arm64', { cache: f.cache, temporaryRoot: f.stages, fetchImpl: f.fetchImpl }), /injected rename/);
    assert.deepEqual(fs.readdirSync(f.stages), []);
    assert.deepEqual(fs.readdirSync(f.cache), []);
  } finally { fs.renameSync = original; f.cleanup(); }
});

test('download rejects oversized and stalled bodies with bounded waiting', async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'netget-stream-test-'));
  try {
    await assert.rejects(download('fixture', path.join(root, 'large'), { maxBytes: 4, fetchImpl: async () => new Response('12345') }), /exceeds/);
    // A held timer keeps the Node test process alive while AbortSignal.timeout fires.
    const hold = setTimeout(() => {}, 1000);
    try {
      await assert.rejects(download('fixture', path.join(root, 'stall'), {
        timeoutMs: 30,
        fetchImpl: async () => new Response(new ReadableStream({ start() {} })),
      }), /abort/i);
    } finally { clearTimeout(hold); }
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
});

test('launcher forwards a second termination signal to a still-running child', { skip: process.platform === 'win32', timeout: 5000 }, async () => {
  const launcher = path.resolve(__dirname, '../npm/netget/bin/netget.js');
  const code = "let n=0; process.on('SIGTERM',()=>{console.log('signal'+(++n));if(n===2)process.exit(0)}); console.log('ready'); setInterval(()=>{},1000);";
  const child = spawn(process.execPath, [launcher, '-e', code], { env: { ...process.env, NETGET_BINARY: process.execPath }, stdio: ['ignore', 'pipe', 'pipe'] });
  let text = '';
  const exited = once(child, 'exit');
  child.stdout.on('data', chunk => {
    text += chunk;
    if (text.includes('ready') && !text.includes('first-sent')) { text += 'first-sent'; child.kill('SIGTERM'); }
    if (text.includes('signal1') && !text.includes('second-sent')) { text += 'second-sent'; child.kill('SIGTERM'); }
  });
  try {
    const [status, signal] = await exited;
    assert.equal(status, 0); assert.equal(signal, null); assert.match(text, /signal2/);
  } finally { if (child.exitCode === null) child.kill('SIGKILL'); }
});
