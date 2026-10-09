# HL7 v2 MLLP sender — Experimental

`hl7_send` {message_type, segments, processing_id?}: Rust builds MSH from the startup identity
(`sending_application` NETGET, facilities, `version` 2.5, `processing_id` P), assigns the
control id `NGC<n>`, frames, sends and waits 30 s for the acknowledgment. The ACK's MSA-1 must
be an acknowledgment code and MSA-2 must equal the control id just sent — otherwise the
session ends. `hl7_ack_received` carries code, control id, text, the ACK's type and every
segment (ERR and response segments included). A frame from the receiver with nothing pending
ends the session. One message in flight; injected-action log entries record type and control
id only, because bodies carry patient data. Shares `wire.rs` with the endpoint.
