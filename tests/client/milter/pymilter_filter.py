"""A pymilter filter (Sendmail's libmilter in C, through the Python binding) for NetGet's
milter client: listens on inet:PORT@127.0.0.1. Rejects spam@ recipients, answers 550 5.7.1 to
senders containing "spammer", and at end of message adds X-Py-Milter: seen, adds
<audit@example.com> and prefixes the Subject with [py].

Usage: python pymilter_filter.py PORT
"""
import sys

import Milter


class Filter(Milter.Base):
    def __init__(self):
        self.subject = ""

    def connect(self, hostname, family, hostaddr):
        return Milter.CONTINUE

    def hello(self, name):
        return Milter.CONTINUE

    def envfrom(self, sender, *args):
        if "spammer" in sender:
            self.setreply("550", "5.7.1", "pymilter refuses spammers")
            return Milter.REJECT
        self.subject = ""
        return Milter.CONTINUE

    def envrcpt(self, recipient, *args):
        if recipient.strip("<>").startswith("spam@"):
            return Milter.REJECT
        return Milter.CONTINUE

    def header(self, name, value):
        if name.lower() == "subject":
            self.subject = value
        return Milter.CONTINUE

    def eom(self):
        self.addheader("X-Py-Milter", "seen")
        self.addrcpt("<audit@example.com>")
        self.chgheader("Subject", 1, "[py] " + self.subject)
        return Milter.ACCEPT


Milter.factory = Filter
Milter.set_flags(Milter.ADDHDRS | Milter.ADDRCPT | Milter.CHGHDRS)
print("pymilter filter starting", flush=True)
Milter.runmilter("netget-test", "inet:%s@127.0.0.1" % sys.argv[1], 60)
