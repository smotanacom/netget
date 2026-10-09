# DICOM tests

Peer: `python3 tests/server/dicom/install_peers.py ROOT` installs pynetdicom 3.0.4 on pydicom 3.0.2
(hash-pinned wheels, no dependencies) and prints `NETGET_DICOM_PYTHON`. `peer.py scu` drives
NetGet's SCP; `peer.py scp` is the SCP for NetGet's client. `tests/helpers/dicom.rs` holds the
archive script (JSON file; BLOCKED rejected, BROKEN undecided, PatientID REJECT refused).

- `peer_test.rs` — pynetdicom SCU: wrong called AE, refused and undecided associations, the
  negotiated contexts, C-ECHO, three C-STOREs (one A700 with its comment), C-FIND by wildcard,
  date range, patient root and no match, A-RELEASE; what the handler saw.
- `wire_test.rs` — an oversized PDU and a non-RQ first PDU aborted, a SOP UID mismatch (A900,
  handler not asked), an unknown command (0211), the idle abort; the NetGet pair.
- `model_test.rs` — literal Explicit/Implicit LE bytes, round trips, hostile lengths, an
  unterminated sequence, a depth bomb, C-FIND matching and projection.

`tests/client/dicom/peer_test.rs` — NetGet's client against pynetdicom's SCP.
