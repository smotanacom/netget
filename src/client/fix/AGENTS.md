# FIX initiator — Experimental

Uses `src/server/fix/codec.rs` and `src/server/fix/session.rs`. Connect sends a Logon
(`sender_comp_id` → `target_comp_id`, `begin_string`, `heartbeat_secs`, `reset_seq_num`,
optional `username` / `password`) and fails unless the answer is a Logon (a Logout's text is the
error). Then `fix_logged_on`, one `fix_message` per in-sequence application message, and
`fix_logged_out` with the reason when the session ends. Actions: `fix_send`, `fix_logout` (waits
up to five seconds for the answer), `disconnect`. The session layer — sequence gaps, resends,
heartbeats, test requests — is the acceptor's.
