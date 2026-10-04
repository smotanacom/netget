"""pyepp 0.2.0 (InternetNZ), unchanged, as a registrar against an EPP server. Prints one JSON
object. SSL_CERT_FILE names the server certificate to trust (pyepp loads the default store).

    peer.py HOST PORT CAFILE
"""
import json
import os
import sys
from datetime import date

host, port, cafile = sys.argv[1:4]
os.environ["SSL_CERT_FILE"] = cafile

from bs4 import BeautifulSoup  # noqa: E402
from pyepp.contact import AddressData, Contact, ContactData, PostalInfoData  # noqa: E402
from pyepp.domain import Domain, DomainData  # noqa: E402
from pyepp.epp import EppCommunicator, EppCommunicatorException  # noqa: E402
from pyepp.host import Host, HostData, IPAddressData  # noqa: E402

out = {}
epp = EppCommunicator(host, port)
greeting = BeautifulSoup(epp.connect(), "xml")
out["sv_id"] = greeting.find("svID").text
out["obj_uris"] = [u.text for u in greeting.find_all("objURI")]

before = Domain(epp).check(["taken.example"])
out["before_login"] = before.code
try:
    epp.login("registrar1", "wrong")
except EppCommunicatorException as e:
    out["bad_login"] = str(e)
out["login"] = epp.login("registrar1", "secret-pw-1").code

domains = Domain(epp)
r = domains.check(["taken.example", "free.example"], client_transaction_id="TR-check-1")
out["check"] = r.result_data
out["check_cltrid"] = r.client_transaction_id
out["check_svtrid"] = r.server_transaction_id

contact = ContactData(id="c-1001", email="ada@example.com", password="c-pw-1",
                      postal_info=PostalInfoData(name="Ada Lovelace", address=AddressData(street_1="1 Analytical St", city="London", country_code="GB")))
out["contact_create"] = Contact(epp).create(contact).code
out["host_create"] = Host(epp).create(HostData(host_name="ns1.free.example", address=[IPAddressData(address="192.0.2.53", ip="v4")])).code
r = domains.create(DomainData(domain_name="free.example", period=2, registrant="c-1001", admin="c-1001", host=["ns1.free.example"], password="d-pw-1"))
out["domain_create"] = {"code": r.code, "ex_date": BeautifulSoup(r.raw_response, "xml").find("exDate").text}

info = domains.info("taken.example")
d = info.result_data
out["info"] = {"code": info.code, "name": d.domain_name, "registrant": d.registrant, "admin": d.admin, "status": d.status, "host": d.host, "sponsor": d.sponsoring_client_id, "expiry": d.expiry_date, "roid": info.repository_object_id}
missing = domains.info("missing.example")
out["missing"] = {"code": missing.code, "reason": missing.reason}

r = domains.renew("free.example", date(2028, 10, 4), period=1)
out["renew"] = {"code": r.code, "ex_date": BeautifulSoup(r.raw_response, "xml").find("exDate").text}
r = domains.transfer("taken.example", "move-me-1")
out["transfer"] = {"code": r.code, "status": BeautifulSoup(r.raw_response, "xml").find("trStatus").text}
r = domains.transfer("taken.example", "wrong")
out["bad_transfer"] = r.code
out["logout"] = epp.logout().code
print(json.dumps(out))
