# LMTP client (RFC 2033)

`connect` reads the 220 greeting and sends LHLO (`lhlo_domain`, default `netget.local`)
before reporting `lmtp_connected` with the capability list; a server that refuses LHLO fails
the connect. Each `lmtp_send` runs one transaction: MAIL FROM, one RCPT per recipient, and
DATA only when at least one recipient was accepted, then one delivery reply is read per
accepted recipient. A transaction nobody accepted is cleared with RSET. `lmtp_result`
reports the MAIL reply, each recipient's RCPT and delivery replies, and the delivered list.

The message is composed here: Date, Message-ID (`<uuid@lhlo_domain>`), From, To and Subject
unless the action supplies them, extra headers validated as single lines, the body sent with
CRLF line endings and dot-stuffed. An injected send is answered once all its bytes are on the
wire. Replies are bounded to 1000-byte lines and 64 lines, and every connect, write and reply
has a 30 s deadline. Plain TCP only.
