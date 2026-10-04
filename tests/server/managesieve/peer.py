"""sievelib (unchanged) as a ManageSieve client of NetGet's server: one JSON line per step.

Usage: peer.py PORT USER PASSWORD
"""
import json, sys
from sievelib.managesieve import Client

port, user, password = int(sys.argv[1]), sys.argv[2], sys.argv[3]
c = Client("127.0.0.1", port)

def out(step, ok, **kw):
    print(json.dumps({"step": step, "ok": bool(ok), "error": None if ok else c.errmsg.decode() if isinstance(c.errmsg, bytes) else c.errmsg, **kw}), flush=True)

ok = c.connect(user, password, authmech="PLAIN")
out("connect", ok, implementation=c.get_implementation(), sieve=c.get_sieve_capabilities())
if not ok:
    sys.exit(0)
spam = 'require "fileinto";\r\nif header :contains "subject" "[SPAM]" {\r\n  fileinto "Junk";\r\n}\r\n'
out("put_spam", c.putscript("spam", spam))
out("put_vacation", c.putscript("vacation", 'require "vacation";\r\nvacation "Away";\r\n'))
out("put_bad", c.putscript("bad", "bogus;\r\n"))
out("check_bad", c.checkscript("bogus;\r\n"))
out("check_good", c.checkscript("keep;\r\n"))
out("setactive", c.setactive("spam"))
active, scripts = c.listscripts()
out("list", active is not None or scripts is not None, active=active, scripts=scripts)
out("get", True, script=c.getscript("spam"))
out("get_missing", c.getscript("missing") is not None)
out("delete_active", c.deletescript("spam"))
out("rename", c.renamescript("vacation", "away"))
out("rename_taken", c.renamescript("away", "spam"))
out("havespace_small", c.havespace("x", 100))
out("havespace_big", c.havespace("x", 10_000_000))
out("deactivate", c.setactive(""))
out("delete", c.deletescript("spam"))
active, scripts = c.listscripts()
out("list_after", True, active=active, scripts=scripts)
out("logout", c.logout() is None)
