# DICOM server (SCP) — Experimental

`pdu.rs` is the PS3.8 upper layer (A-ASSOCIATE-RQ/AC/RJ, P-DATA-TF fragmentation and reassembly,
A-RELEASE, A-ABORT); `dataset.rs` the PS3.5 codec (Implicit and Explicit VR Little Endian,
defined and undefined lengths, sequences) to and from the DICOM JSON model, plus PS3.4 C.2.2.2
C-FIND matching and projection. No compressed or big-endian transfer syntaxes.

Rust owns:
- Association: ARTIM 30 s for the A-ASSOCIATE-RQ; a called AE title other than `ae_title` is
  rejected (reason 7) without asking the handler; contexts are negotiated for Verification,
  Patient and Study Root C-FIND and every Storage SOP class, preferring Explicit VR LE.
- C-ECHO (always success), C-STORE SOP UIDs that differ from the command (A900), an identifier
  without a valid QueryRetrieveLevel (A900), undecodable datasets (C000), unknown commands (0211).
- Bounds: PDUs over 1 MiB (aborted from the announced length), datasets over 16 MiB, 50 000
  elements, nesting 8; sequences and items with undefined length must be delimited; the idle
  timeout (`idle_timeout_secs`, default 60) aborts.

The handler decides `dicom_associate` (`dicom_accept` / `dicom_reject`), `dicom_store`
(`dicom_store_status`: success, two warnings, A700, A900, C000) and `dicom_find`
(`dicom_find_matches` with candidate records, which Rust matches and projects — one FF00 pending
response per match, then 0000 — or `dicom_find_failed`). An associate the handler does not
decide is rejected transient; a store or find it does not answer is A700 / C000, never success.

Not implemented: C-MOVE, C-GET, C-CANCEL handling (ignored), N-services, TLS, extended
negotiation. No storage: the handler owns every instance.
