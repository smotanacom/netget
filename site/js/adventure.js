// The landing-page adventure owns its map and session state. Models (and the visitor in
// manual mode) describe the result of a command; they do not have to remember to save it.
const ROOMS = {
    Gate: { description: 'A rusty lamp hangs by the gate.', exits: { north: 'Hall' } },
    Hall: { description: 'A sleeping dragon guards the hall.', exits: { south: 'Gate', east: 'Vault' } },
    Vault: { description: 'A heap of gold fills the vault.', exits: { west: 'Hall' } },
};
const DIRECTIONS = { n: 'north', s: 'south', e: 'east', w: 'west' };

export class Adventure {
    constructor() { this.sessions = new Map(); }

    // Called once for each bridged request, before routing it to any model or the composer.
    // An event's token stays the same through model/format retries, so a retry gets the same
    // room snapshot and can never apply its movement twice.
    prepare(req, serverId) {
        const event = req.event;
        if (!event || event.server_id !== serverId || event.protocol.toLowerCase() !== 'telnet'
            || event.connection_id == null || !event.token) return req;
        const key = `${event.server_id}:${event.connection_id}`;
        if (event.event_type !== 'telnet_message_received') return req;
        const command = event.data?.message;
        if (typeof command !== 'string') return req;
        let session = this.sessions.get(key);
        if (!session) {
            session = { server: event.server_id, connection: event.connection_id, room: 'Gate', playing: false, turns: new Map() };
            this.sessions.set(key, session);
        }
        let state = session.turns.get(event.token);
        if (!state) {
            state = this.turn(session, command);
            session.turns.set(event.token, state);
            // Only recent in-flight retries matter. Pruning also bounds a long-lived session.
            if (session.turns.size > 16) session.turns.delete(session.turns.keys().next().value);
        }
        if (!state.playing) return req;
        const context = '\n\nDemo adventure state:\n' + JSON.stringify(state)
            + '\nThe demo has already applied this command. For a game command describe its result '
            + 'and the current room with send_telnet_line; do not move again or call set_memory '
            + 'to track the room. For chat, answer the visitor normally. Keep the answer under 200 characters.';
        const messages = req.messages.map((m) => ({ ...m }));
        const last = messages.findLast((m) => m.role === 'user') || messages.at(-1);
        if (last) last.content += context;
        return { ...req, messages, adventure: state };
    }

    turn(session, input) {
        const command = input.trim().toLowerCase().replace(/\s+/g, ' ');
        let result = 'chat';
        let detail = 'Chat; the room stays the same.';
        if (['play', 'reset', 'restart'].includes(command)) {
            session.playing = true;
            session.room = 'Gate';
            result = 'start';
            detail = 'Start again at the Gate.';
        } else if (command === 'look') {
            session.playing = true;
            result = 'look';
            detail = 'Describe the current room.';
        } else if (/^(?:go\s+\S+|north|south|east|west|up|down|n|s|e|w)$/.test(command)) {
            session.playing = true;
            const token = command.replace(/^go\s+/, '');
            const direction = Object.hasOwn(DIRECTIONS, token) ? DIRECTIONS[token] : token;
            const from = session.room;
            const exits = ROOMS[from].exits;
            const destination = Object.hasOwn(exits, direction) ? exits[direction] : null;
            if (destination) {
                session.room = destination;
                result = 'move';
                detail = `Moved ${direction} from ${from} to ${destination}.`;
            } else {
                result = 'blocked';
                detail = `There is no exit ${direction} from ${from}; stay in ${from}.`;
            }
        } else if (/^take\s+/.test(command)) {
            session.playing = true;
            result = 'take';
            detail = 'Describe the attempt without changing rooms; this tiny demo has no inventory.';
        }
        const room = ROOMS[session.room];
        return { playing: session.playing, room: session.room, description: room.description, exits: room.exits, result, detail };
    }

    // The wasm API supplies active connection ids. Closed peers and restarted/stopped
    // servers lose their game state even when they never send another model request.
    prune(servers) {
        const active = new Set();
        for (const server of servers) {
            if (server.status !== 'Running') continue;
            for (const connection of server.active_connection_ids || []) active.add(`${server.id}:${connection}`);
        }
        for (const key of this.sessions.keys()) if (!active.has(key)) this.sessions.delete(key);
    }
}
