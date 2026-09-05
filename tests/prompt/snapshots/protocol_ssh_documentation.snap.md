Protocol: SSH
State: Beta
Implementation: russh v0.40, russh-sftp v2.0; ephemeral Ed25519 host key
LLM Control: Auth decisions, shell banner and output, SFTP reads and listings
E2E Testing: openssh client (ssh/sftp) by hand - no automated test exists
Notes: Shell and SFTP only: no port forwarding, no X11, no keyboard-interactive. SFTP is read-only (write/remove/mkdir/rmdir/rename are not implemented). Host key is regenerated on every start, so clients warn about a changed key.