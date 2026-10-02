// Pure browser-composer checks, independent of the browser and WASM bundle.
import assert from 'node:assert/strict';
import test from 'node:test';
import { newEntry, buildAction, entriesFromEnvelope } from '../../site/js/composer.js';

const action = {
    name: 'reply', parameters: [
        { name: 'count', type: 'u64', required: true },
        { name: 'enabled', type: 'bool', required: false },
        { name: 'text', type: 'string', required: false },
    ], example: { type: 'reply', count: 1 },
};

test('numeric fields reject integers that JavaScript silently rounds', () => {
    const entry = newEntry(action);
    entry.fields[0].raw = '9007199254740993';
    assert.equal(buildAction(entry).ok, false);
    entry.fields[0].raw = '9007199254740991';
    assert.equal(buildAction(entry).value.count, 9007199254740991);
});

test('raw-to-form conversion refuses values it would change', () => {
    for (const item of [
        { type: 'reply', count: 3, text: '' },
        { type: 'reply', count: 3, enabled: null },
        { type: 'reply' },
    ]) assert.equal(entriesFromEnvelope([action], { actions: [item] }), null);
    assert.equal(entriesFromEnvelope([action], { actions: null }), null);
    assert.equal(entriesFromEnvelope([action], { actions: false }), null);
    assert.equal(entriesFromEnvelope([action], { tools: [{ type: 'reply', count: 3 }] }), null);
    assert.ok(entriesFromEnvelope([action], { actions: [{ count: 3, type: 'reply', enabled: false }] }));
    // A source string stays a text field; the form must preserve its exact JSON type.
    assert.ok(entriesFromEnvelope([action], { actions: [{ count: '3', type: 'reply' }] }));
});

test('literal __proto__ JSON fields remain ordinary data through the form', () => {
    const source = JSON.parse('{"type":"reply","count":2,"__proto__":{"injected":true}}');
    const entry = newEntry(action, source);
    const built = buildAction(entry);
    assert.equal(built.ok, true);
    assert.deepEqual(JSON.parse(JSON.stringify(built.value)), source);
    assert.equal(Object.hasOwn(built.value, '__proto__'), true);
    assert.equal(built.value.injected, undefined);
});
