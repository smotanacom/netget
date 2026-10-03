// Launcher-only tests: the child is a tiny fixture, never the NetGet binary.
const assert = require('node:assert/strict');
const test = require('node:test');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');
const launcher = path.resolve(__dirname, '../npm/netget/bin/netget.js');
const version = require('../npm/netget/package.json').version;

test('NETGET_BINARY preserves arguments, stdout and child exit status', () => {
    const result = spawnSync(process.execPath, [launcher, '-e', 'process.stdout.write(JSON.stringify(process.argv.slice(1))); process.exit(7)', 'literal argument'], {
        env: { ...process.env, NETGET_BINARY: process.execPath }, encoding: 'utf8',
    });
    assert.equal(result.status, 7);
    assert.equal(result.stdout, '["literal argument"]');
    assert.equal(result.stderr, '');
});

test('download cache is separated by platform and architecture', { skip: process.platform === 'win32' }, () => {
    const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'netget-launcher-test-'));
    try {
        let key = `${process.platform}-${process.arch}`;
        if (key === 'linux-x64' && !process.report.getReport().header.glibcVersionRuntime) key += '-musl';
        const versionDir = path.join(temp, 'netget', version);
        const platformDir = path.join(versionDir, key);
        fs.mkdirSync(platformDir, { recursive: true });
        fs.writeFileSync(path.join(versionDir, 'netget'), '#!/bin/sh\nexit 99\n', { mode: 0o755 });
        fs.writeFileSync(path.join(platformDir, 'netget'), '#!/bin/sh\nprintf platform-cache\n', { mode: 0o755 });
        const result = spawnSync(process.execPath, [launcher], {
            env: { ...process.env, NETGET_BINARY: '', XDG_CACHE_HOME: temp, NETGET_DOWNLOAD_BASE: 'http://127.0.0.1:9' }, encoding: 'utf8', timeout: 10000,
        });
        assert.equal(result.status, 0, result.stderr);
        assert.equal(result.stdout, 'platform-cache');
        assert.equal(result.stderr, '');
    } finally {
        fs.rmSync(temp, { recursive: true, force: true });
    }
});
