"""jmapc 0.3.0, unchanged, against a JMAP server. Prints one JSON object.

    peer.py HOST:PORT USER PASSWORD CAFILE
"""
import json
import os
import sys

host, user, password, cafile = sys.argv[1:5]
os.environ["REQUESTS_CA_BUNDLE"] = cafile

import jmapc  # noqa: E402
from jmapc import Client, Comparator, EmailQueryFilterCondition, MailboxQueryFilterCondition, Ref  # noqa: E402
from jmapc.methods import (  # noqa: E402
    CoreEcho,
    EmailChanges,
    EmailGet,
    EmailQuery,
    EmailSet,
    MailboxGet,
    MailboxQuery,
)

out = {"jmapc": getattr(jmapc, "__version__", "0.3.0")}


def error_type(r):
    return getattr(r, "type", None)


client = Client.create_with_password(host, user, password)
try:
    session = client.jmap_session
except Exception as e:  # a refused login is a result, not a crash
    out["session_error"] = str(e)
    print(json.dumps(out))
    sys.exit(0)
out["username"] = session.username
out["account"] = client.account_id
out["api_url"] = session.api_url
out["echo"] = client.request(CoreEcho(data={"hello": "world"})).data

r = client.request([MailboxQuery(filter=MailboxQueryFilterCondition(role="inbox")), MailboxGet(ids=Ref("/ids"))])
mailboxes = r[1].response.data
out["mailboxes"] = [m.name for m in mailboxes]
inbox = mailboxes[0].id

r = client.request([
    EmailQuery(filter=EmailQueryFilterCondition(in_mailbox=inbox),
               sort=[Comparator(property="receivedAt", is_ascending=False)], limit=10),
    EmailGet(ids=Ref("/ids"), properties=["subject", "receivedAt", "keywords"]),
])
out["subjects"] = [e.subject for e in r[1].response.data]

r = client.request([
    EmailSet(create={"draft": {"mailboxIds": {inbox: True}, "subject": "Draft", "keywords": {"$draft": True}}},
             update={"e1": {"keywords/$seen": True}, "e9": {"keywords/$seen": True}}),
    EmailGet(ids=["#draft"], properties=["subject"]),
])
s = r[0].response
out["created"] = {k: v["id"] if isinstance(v, dict) else v.id for k, v in (s.created or {}).items()}
out["updated"] = sorted((s.updated or {}).keys())
out["not_updated"] = {k: v.type for k, v in (s.not_updated or {}).items()}
out["draft_subject"] = [e.subject for e in r[1].response.data]

r = client.request(EmailChanges(since_state="s1"))
out["changes"] = {"old": r.old_state, "new": r.new_state, "created": r.created, "updated": r.updated}
r = client.request(EmailChanges(since_state="ancient"))
out["old_changes_error"] = error_type(r)
print(json.dumps(out))
