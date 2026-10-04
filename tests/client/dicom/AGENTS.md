# DICOM client tests

`peer_test.rs` runs pynetdicom 3.0.4 as an SCP titled PACS (independent, unchanged; called AE title
required) and drives NetGet's client: a wrong called AE title refused, then C-ECHO, a C-STORE the
SCP decodes (patient, calling AE, pixel bytes from its own output), one it refuses (A700), C-FIND
with one and no matches, and A-RELEASE. Needs `NETGET_DICOM_PYTHON` from
`tests/server/dicom/install_peers.py`; fails without it.
