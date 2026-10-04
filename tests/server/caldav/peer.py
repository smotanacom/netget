"""python caldav 3.3.1, unchanged, against a CalDAV server: discovery from the root URL,
calendars, MKCALENDAR, saving events, a time-range search, lookup by UID, an update (with the
ETag it read), deletion, and a refused password. Prints one JSON line per step.

Usage: peer.py URL
"""
import json, sys
from datetime import datetime, timezone
import caldav

out = lambda **kw: print(json.dumps(kw), flush=True)
url = sys.argv[1]

EVENT = """BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//caldav peer//EN
BEGIN:VEVENT
UID:{uid}
DTSTAMP:20261001T000000Z
DTSTART:{start}
DTEND:{end}
SUMMARY:{summary}
END:VEVENT
END:VCALENDAR
"""

try:
    caldav.DAVClient(url, username="alice", password="wrong").principal()
    out(step="bad_login", refused=False)
except Exception as e:
    out(step="bad_login", refused=True, error=type(e).__name__)

client = caldav.DAVClient(url, username="alice", password="secret")
principal = client.principal()
out(step="principal", url=str(principal.url))
out(step="calendars", names=sorted(c.get_display_name() for c in principal.calendars()))
personal = principal.make_calendar(name="Personal", cal_id="personal")
out(step="made", url=str(personal.url))
work = [c for c in principal.calendars() if str(c.url).rstrip("/").endswith("/work")][0]
work.save_event(EVENT.format(uid="standup", start="20261005T090000Z", end="20261005T091500Z", summary="Standup"))
work.save_event(EVENT.format(uid="retro", start="20261020T150000Z", end="20261020T160000Z", summary="Retro"))
personal.save_event(EVENT.format(uid="dentist", start="20261006T080000Z", end="20261006T090000Z", summary="Dentist"))
found = work.search(start=datetime(2026, 10, 5, tzinfo=timezone.utc), end=datetime(2026, 10, 12, tzinfo=timezone.utc), event=True, expand=False)
out(step="search", uids=sorted(str(e.icalendar_component["UID"]) for e in found))
ev = work.event_by_uid("standup")
ev.icalendar_component["SUMMARY"] = "Daily standup"
ev.save()
again = work.event_by_uid("standup")
out(step="updated", summary=str(again.icalendar_component["SUMMARY"]))
work.event_by_uid("retro").delete()
out(step="remaining", uids=sorted(str(e.icalendar_component["UID"]) for e in work.events()))
try:
    personal.save_event(EVENT.format(uid="dentist", start="20261007T080000Z", end="20261007T090000Z", summary="Dup"), no_overwrite=True)
    out(step="duplicate", refused=False)
except Exception as e:
    out(step="duplicate", refused=True, error=type(e).__name__)
