"""Independent LMTP server for the NetGet client tests: aiosmtpd 1.4.6's LMTP class, unchanged.

Refuses RCPT for any mailbox whose local part is "nobody", answers DATA once per recipient
(a 452 for local part "full", 250 otherwise), and appends each accepted message to the
record file as one JSON line. Prints "READY <port>" once listening on 127.0.0.1.

Usage: python peer.py <record-file>
"""
import asyncio, json, sys
from aiosmtpd.lmtp import LMTP


class Handler:
    def __init__(self, record):
        self.record = record

    async def handle_RCPT(self, server, session, envelope, address, rcpt_options):
        if address.split("@")[0] == "nobody":
            return "550 5.1.1 No such user here"
        envelope.rcpt_tos.append(address)
        return "250 2.1.5 Recipient OK"

    async def handle_DATA(self, server, session, envelope):
        with open(self.record, "a", encoding="utf-8") as f:
            f.write(json.dumps({
                "mail_from": envelope.mail_from,
                "rcpt_tos": envelope.rcpt_tos,
                "content": envelope.content.decode("utf-8", "replace"),
            }) + "\n")
        replies = []
        for rcpt in envelope.rcpt_tos:
            if rcpt.split("@")[0] == "full":
                replies.append("452 4.2.2 Mailbox full")
            else:
                replies.append(f"250 2.0.0 Delivered to {rcpt}")
        # aiosmtpd's LMTP mode: one status line per recipient, in RCPT order.
        return "\r\n".join(replies)


async def main():
    handler = Handler(sys.argv[1])
    loop = asyncio.get_running_loop()
    server = await loop.create_server(lambda: LMTP(handler, hostname="aiosmtpd.test"), "127.0.0.1", 0)
    port = server.sockets[0].getsockname()[1]
    print(f"READY {port}", flush=True)
    async with server:
        await server.serve_forever()


asyncio.run(main())
