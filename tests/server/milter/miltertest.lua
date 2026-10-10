-- OpenDKIM's miltertest (the MTA side, in C), against a filter at -D port=N. Every check
-- error()s with what it saw, so the exit status is the verdict.
local sock = "inet:" .. port .. "@127.0.0.1"
local function expect(conn, want, what)
  local got = mt.getreply(conn)
  if got ~= want then error(what .. ": reply " .. tostring(got) .. ", wanted " .. tostring(want)) end
end
local function ok(err, what) if err ~= nil then error(what .. ": " .. tostring(err)) end end

-- A clean message: every stage continues, end of message accepts with modifications.
local conn = mt.connect(sock, 5, 1)
if conn == nil then error("could not connect to " .. sock) end
ok(mt.negotiate(conn, nil, nil, nil), "negotiate")
ok(mt.conninfo(conn, "client.example", "192.0.2.10"), "conninfo")
expect(conn, SMFIR_CONTINUE, "connect")
ok(mt.helo(conn, "client.example"), "helo")
expect(conn, SMFIR_CONTINUE, "helo")
ok(mt.mailfrom(conn, "<alice@example.com>"), "mailfrom")
expect(conn, SMFIR_CONTINUE, "mail")
ok(mt.rcptto(conn, "<bob@example.net>"), "rcptto")
expect(conn, SMFIR_CONTINUE, "rcpt")
ok(mt.rcptto(conn, "<spam@example.net>"), "rcptto spam")
expect(conn, SMFIR_REJECT, "rcpt spam")
ok(mt.header(conn, "Subject", "hello"), "header")
ok(mt.header(conn, "From", "Alice <alice@example.com>"), "header from")
ok(mt.eoh(conn), "eoh")
ok(mt.bodystring(conn, "Hi Bob,\r\nlunch?\r\n"), "body")
ok(mt.eom(conn), "eom")
expect(conn, SMFIR_ACCEPT, "eom")
if not mt.eom_check(conn, MT_HDRADD, "X-NetGet", "checked") then error("no X-NetGet header added") end
if not mt.eom_check(conn, MT_HDRCHANGE, "Subject", "[netget] hello") then error("Subject not changed") end
if not mt.eom_check(conn, MT_RCPTADD, "<audit@example.com>") then error("no audit recipient") end
-- A second message on the same connection is quarantined.
ok(mt.mailfrom(conn, "<carol@example.com>"), "mailfrom 2")
expect(conn, SMFIR_CONTINUE, "mail 2")
ok(mt.rcptto(conn, "<bob@example.net>"), "rcptto 2")
expect(conn, SMFIR_CONTINUE, "rcpt 2")
ok(mt.header(conn, "Subject", "quarantine me"), "header 2")
ok(mt.eoh(conn), "eoh 2")
ok(mt.bodystring(conn, "x\r\n"), "body 2")
ok(mt.eom(conn), "eom 2")
if not mt.eom_check(conn, MT_QUARANTINE) then error("not quarantined") end
mt.disconnect(conn)

-- A refused sender gets the handler's own SMTP reply; a refused client is rejected at once.
conn = mt.connect(sock, 5, 1)
ok(mt.negotiate(conn, nil, nil, nil), "negotiate 2")
ok(mt.conninfo(conn, "client.example", "192.0.2.11"), "conninfo 2")
expect(conn, SMFIR_CONTINUE, "connect 2")
ok(mt.mailfrom(conn, "<spammer@bad.example>"), "mailfrom spammer")
expect(conn, SMFIR_REPLYCODE, "mail spammer")
mt.disconnect(conn)
conn = mt.connect(sock, 5, 1)
ok(mt.negotiate(conn, nil, nil, nil), "negotiate 3")
ok(mt.conninfo(conn, "evil.example", "203.0.113.66"), "conninfo evil")
expect(conn, SMFIR_REJECT, "connect evil")
mt.disconnect(conn)
mt.echo("all checks passed")
