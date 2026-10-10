"""py-consul, unchanged, against a Consul agent: KV put/get/list/cas/delete, a service
registered, looked up in the catalog and health, and deregistered. Prints one JSON line.

Usage: py_consul_peer.py HOST PORT
"""
import json, sys
import consul

c = consul.Consul(host=sys.argv[1], port=int(sys.argv[2]))
out = {}
out["put"] = c.kv.put("py/greeting", "hello from python", flags=3)
index, data = c.kv.get("py/greeting")
out["get"] = {"value": data["Value"].decode(), "flags": data["Flags"], "index": index}
out["cas_stale"] = c.kv.put("py/greeting", "nope", cas=1)
out["cas_current"] = c.kv.put("py/greeting", "updated", cas=data["ModifyIndex"])
c.kv.put("py/other", "x")
_, keys = c.kv.get("py/", keys=True)
out["keys"] = keys
_, entries = c.kv.get("py/", recurse=True)
out["recursed"] = {e["Key"]: e["Value"].decode() for e in entries}
out["delete"] = c.kv.delete("py/other")
_, gone = c.kv.get("py/other")
out["gone"] = gone is None
c.agent.service.register("api", service_id="api1", address="127.0.0.9", port=9000, tags=["py"])
_, services = c.catalog.services()
out["services"] = services
_, nodes = c.catalog.service("api")
out["catalog"] = [{"id": n["ServiceID"], "port": n["ServicePort"], "address": n["ServiceAddress"]} for n in nodes]
_, health = c.health.service("api", passing=True)
out["health"] = [{"id": h["Service"]["ID"], "status": h["Checks"][0]["Status"]} for h in health]
c.agent.service.deregister("api1")
_, after = c.catalog.services()
out["after"] = after
print(json.dumps(out), flush=True)
