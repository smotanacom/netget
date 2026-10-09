# LPD server (RFC 1179)

Hand-written over Tokio TCP. One command per connection: print-waiting (no reply), receive
job, short and long queue state, remove jobs. Rust owns the framing and every acknowledgement;
handlers decide.

## Receive job

The queue is checked against `queues` (when set) at the first acknowledgement. Control and
data files may arrive in either order and several jobs may follow on one connection; the abort
subcommand discards what was received. Each file's count is checked against its bound when
announced, before any byte is read, and must be followed by a NUL. When the control file and
every data file it names are present, `lpd_print_job` is raised and the acknowledgement of
the file that completed the job carries the decision: 0 queued, 1 refused. The event carries
the control fields and, per print line, the file's format letter, source name, size and text
(first 64 KiB, `null` when binary). Nothing is spooled or kept.

## Failure modes

`decision=` tags: `model_answer`, `model_reject`, `model_silent`, `fail_closed_llm_error`,
`fail_closed_invalid_reply`. A failure refuses the job, answers a queue query with
`<queue>: queue status unavailable`, and reports nothing removed.

## Not implemented

RFC 1179's 721-731 source-port check, count-0 streaming of data files, and the
banner/fonts/indent control lines (accepted, not shown to the handler). Port 515 is
privileged; without that privilege the server starts on an OS-assigned port.
