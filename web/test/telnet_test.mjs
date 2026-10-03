// CPU-only parser tests: no browser, WASM bundle, network, or model needed.
import assert from 'node:assert/strict';
import test from 'node:test';
import { TelnetDecoder } from '../../site/js/telnet.js';

test('negotiation, UTF-8 and CRLF survive every possible TCP split', () => {
    const bytes = new Uint8Array([
        255, 251, 1, 255, 253, 3,
        ...new TextEncoder().encode('hello 世界\r\nnext\n'),
        255, 250, 24, 65, 255, 255, 66, 255, 240,
        ...new TextEncoder().encode('done'),
    ]);
    for (let split = 0; split <= bytes.length; split += 1) {
        const decoder = new TelnetDecoder();
        const first = decoder.push(bytes.slice(0, split));
        const second = decoder.push(bytes.slice(split));
        assert.equal(first.text + second.text + decoder.finish(), 'hello 世界\r\nnext\r\ndone', `split ${split}`);
        assert.deepEqual([...first.replies, ...second.replies], [255, 254, 1, 255, 252, 3], `split ${split}`);
    }
});

test('one-byte chunks and empty chunks preserve parser state', () => {
    const decoder = new TelnetDecoder();
    const bytes = [255, 251, 1, ...new TextEncoder().encode('é🙂\r\n')];
    let text = '';
    const replies = [];
    for (const byte of bytes) {
        const result = decoder.push([byte]);
        text += result.text;
        replies.push(...result.replies);
        assert.equal(decoder.push([]).text, '');
    }
    assert.equal(text + decoder.finish(), 'é🙂\r\n');
    assert.deepEqual(replies, [255, 254, 1]);
});

test('incomplete negotiations never reply with an invented option', () => {
    const decoder = new TelnetDecoder();
    assert.deepEqual([...decoder.push([255, 253]).replies], []);
    assert.deepEqual([...decoder.push([42]).replies], [255, 252, 42]);
});
