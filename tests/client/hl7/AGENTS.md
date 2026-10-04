# HL7 sender tests

`peer_test.rs` points the sender at python-hl7 0.4.5's MLLP server (independent, unchanged): ADT is
acknowledged AA, ORU AE with an ERR segment, SIU AR. The receiver prints what it parsed (type,
control id, sender, patient name, segment list), which is asserted beside what the sender heard.
Needs `NETGET_HL7_PYTHON` from `tests/server/hl7/install_peers.py`; fails without it.
