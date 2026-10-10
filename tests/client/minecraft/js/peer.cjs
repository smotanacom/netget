// node-minecraft-protocol 1.68.0, unchanged, driven against NetGet. Run with
// NODE_PATH=<node_modules> (tests/client/minecraft/install_peers.py prints it).
//   node peer.cjs ping PORT              -> one JSON line: the status it parsed
//   node peer.cjs login PORT NAME        -> one JSON line: how its login ended
//   node peer.cjs server [online]        -> "READY 127.0.0.1:PORT", then serves until killed
'use strict'
const mc = require('minecraft-protocol')
const [mode, ...args] = process.argv.slice(2)
const VERSION = '1.21.1'
const out = (v) => { process.stdout.write(JSON.stringify(v) + '\n') }

if (mode === 'ping') {
  mc.ping({ host: '127.0.0.1', port: Number(args[0]), version: VERSION, closeTimeout: 5000 }, (err, res) => {
    if (err) { out({ error: String(err) }); process.exit(1) }
    out(res)
    process.exit(0)
  })
} else if (mode === 'login') {
  const client = mc.createClient({ host: '127.0.0.1', port: Number(args[0]), username: args[1], version: VERSION, auth: 'offline', hideErrors: true })
  const timer = setTimeout(() => { out({ error: 'timeout' }); process.exit(1) }, 20000)
  client.on('disconnect', (packet) => {
    clearTimeout(timer)
    out({ event: 'disconnect', state: client.state, reason: JSON.parse(packet.reason) })
    process.exit(0)
  })
  client.on('error', (err) => { clearTimeout(timer); out({ error: String(err) }); process.exit(1) })
  client.on('end', (reason) => { clearTimeout(timer); out({ event: 'end', reason: String(reason) }); process.exit(0) })
} else if (mode === 'server') {
  const online = args[0] === 'online'
  const server = mc.createServer({
    host: '127.0.0.1',
    port: 0,
    version: VERSION,
    'online-mode': online,
    motd: 'Independent nmp server',
    maxPlayers: 33,
    hideErrors: true,
    // The status it answers, with a player sample, as a real server's list entry has one.
    beforePing: (response) => {
      response.players.online = 2
      response.players.sample = [
        { name: 'steve', id: '8667ba71-b85a-4004-af54-457a9734eed7' },
        { name: 'alex', id: 'ec561538-f3fd-461d-aff5-086b22154bce' }
      ]
      return response
    },
    // A player named banned_* is refused in the login state with a Disconnect.
    beforeLogin: (client) => {
      if (client.username.startsWith('banned_')) client.end('You are banned: ' + client.username)
    }
  })
  server.on('error', (err) => { process.stderr.write(String(err) + '\n') })
  server.on('listening', () => {
    process.stdout.write('READY 127.0.0.1:' + server.socketServer.address().port + '\n')
  })
} else {
  process.stderr.write('usage: peer.cjs ping|login|server ...\n')
  process.exit(2)
}
