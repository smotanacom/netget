# dnp3 selected scope

Own bounded Rust codec; no commercial Step Function library or license is required.
Link addresses are master 1/outstation 10; CRC-validated unconfirmed TCP link data,
2048-byte transport reassembly, single-fragment application responses, application
confirmation and duplicate last-request replay. Class 0/1/2/3 polls return handler
binary, float analog and counter values; timestamp_ms supplies deterministic 48-bit
event times. Binary controls are CROB direct-operate only, 16-bit indices, explicit
handler success/refusal. No unsolicited reporting, SELECT/OPERATE, analog controls,
Secure Authentication, confirmed link data or serial. 10-second client deadlines.
Actions: dnp3_measurements (points), dnp3_control_result (status);
client dnp3_poll (classes), dnp3_control (index,code,count,on_ms,off_ms).
Independent Apache-2.0 OpenDNP3 3.1.2 exercises both roles.

See tests/helpers/ICS_PEERS.md for reproducible independent peers.
