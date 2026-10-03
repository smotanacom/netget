import assert from 'node:assert/strict';
import { Adventure } from '../../site/js/adventure.js';

let sequence = 0;
const request = (message, connection = 7, token = String(++sequence)) => ({
    id: sequence, kind: 'generate', messages: [{ role: 'user', content: 'Original model prompt.' }],
    event: { token, server_id: 1, connection_id: connection, protocol: 'telnet', event_type: 'telnet_message_received', data: { message } },
});
const game = new Adventure();
const play = (message, connection = 7) => game.prepare(request(message, connection), 1).adventure;
assert.equal(game.prepare(request('hello'), 1).adventure, undefined);
for (const [command, room] of [['play', 'Gate'], ['north', 'Hall'], ['east', 'Vault'], ['west', 'Hall'], ['south', 'Gate']]) {
    assert.equal(play(command).room, room, command);
}
assert.equal(play('east').result, 'blocked');
assert.equal(play('look').room, 'Gate');
for (const invalid of ['go nowhere', 'go constructor', 'go __proto__', 'go toString']) {
    assert.equal(play(invalid).result, 'blocked', invalid);
    assert.equal(play('look').room, 'Gate', invalid);
}
assert.equal(play('go n').room, 'Hall');
assert.equal(play('go east').room, 'Vault');
assert.equal(play('reset').room, 'Gate');
assert.equal(play(' N ').room, 'Hall');
assert.equal(play('restart').room, 'Gate');

const move = request('north');
const first = game.prepare(move, 1);
assert.equal(first.adventure.room, 'Hall');
assert.equal(play('east').room, 'Vault');
assert.deepEqual(game.prepare({ ...move, id: ++sequence }, 1).adventure, first.adventure, 'retries receive the same event snapshot');
assert.equal(play('look').room, 'Vault', 'a retry never applies a move a second time');
assert.match(first.messages[0].content, /Demo adventure state:\n.*"room":"Hall"/);
assert.equal(move.messages[0].content, 'Original model prompt.', 'the original bridge request is not mutated');
assert.equal(play('look', 8).room, 'Gate', 'connections have independent rooms');
assert.equal(play('hello').room, 'Vault', 'chat does not move the player');
assert.equal(play('hello').result, 'chat');

const unrelated = request('north');
assert.equal(game.prepare(unrelated, 99), unrelated, 'other servers are not decorated');
assert.equal(game.prepare({ ...unrelated, event: undefined }, 1).adventure, undefined);
game.prune([{ id: 1, status: 'Running', active_connection_ids: [8] }]);
assert.equal(play('look').room, 'Gate', 'a closed connection loses its state');
play('north');
game.prune([{ id: 1, status: 'Stopped', active_connection_ids: [7] }]);
assert.equal(play('look').room, 'Gate', 'a stopped/restarted server loses its state');
for (let i = 0; i < 100; i++) play('look');
assert.equal(game.sessions.get('1:7').turns.size, 16, 'retry storage stays bounded');
console.log('Adventure: map, invalid moves, reset, retries and session cleanup passed.');
